//! One extension process (the command of its `extension.json`, or a default extension's binary)
//! and the JSON-RPC link to it: one JSON object per line on stdin/stdout, requests in both
//! directions.

use super::Core;
use super::logs::Log;
use crate::llm::ToolSpec;
use crate::gateway::ops;
use crate::messengers::Thread;
use serde_json::{Value, json};
use std::collections::{HashMap, VecDeque};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex as StdMutex, RwLock, RwLockReadGuard};
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::{Mutex, Notify, oneshot};

const START_TIMEOUT: Duration = Duration::from_secs(30);
/// The version of the protocol this August speaks (`ready.protocol`).
pub const PROTOCOL: u64 = 2;
/// How an extension process ended after a successful start.
pub struct Exit {
    /// `exit`, or what the core stopped it for (`hang`, `unhealthy`).
    pub reason: String,
    /// Why, for people: the stderr tail, or the core's reason for stopping it.
    pub error: String,
    pub code: Option<i32>,
}

/// Stderr lines kept to explain a failed start or a crash.
const TAIL_LINES: usize = 20;

type Waiting = Arc<StdMutex<HashMap<u64, oneshot::Sender<Result<Value, RpcError>>>>>;
/// Where `stream` notifications of a running request go, by request id.
type Streams = Arc<StdMutex<HashMap<u64, tokio::sync::mpsc::UnboundedSender<Value>>>>;

/// An error reply: its message, and the kind an extension attached (a provider's failure).
#[derive(Debug)]
pub struct RpcError {
    pub message: String,
    pub kind: Option<String>,
}

impl From<String> for RpcError {
    fn from(message: String) -> Self {
        Self { message, kind: None }
    }
}
type Tail = Arc<StdMutex<VecDeque<String>>>;

/// What an extension has registered (sent once it started, again whenever it changes).
#[derive(Debug, Default)]
pub struct Manifest {
    /// What it does: one line, and as much more as the agent may need (`describe`).
    pub summary: String,
    pub details: String,
    pub tools: Vec<ToolSpec>,
    pub commands: Vec<(String, String)>,
    /// Accounts it signs in to (`/login`).
    pub accounts: Vec<AccountInfo>,
    pub events: Vec<String>,
    /// `(name, text)` of sections for the system prompt.
    pub sections: Vec<(String, String)>,
    /// What it declared it needs (`messaging`, `turns`, `tools`, `llm`).
    pub needs: Vec<String>,
    /// Its own timeouts for hooks, by event (ms).
    pub timeouts: HashMap<String, u64>,
    /// The protocol version it speaks.
    pub protocol: u64,
    /// JSON Schema of its settings (`config/extensions/<name>.json` → `settings`); properties
    /// with `"secret": true` are shown masked.
    pub settings: Value,
    /// Events it emits itself (`august.defineEvent`).
    pub emits: Vec<EventDef>,
    /// Model providers it offers.
    pub providers: Vec<ProviderInfo>,
    /// Extensions whose events it emits in their place (it took over their namespace).
    pub replaces: Vec<String>,
    /// Jobs of the core it does instead (`render`): the core leaves them to it.
    pub takes: Vec<String>,
    /// Messengers it offers.
    pub messengers: Vec<crate::messengers::Description>,
    /// Commands of the `august` program it offers (`""`: `august` alone).
    pub cli: Vec<CliCommand>,
}

/// `august <name> args...`: runs `exec` with the args appended, in the user's terminal.
#[derive(Debug, Clone)]
pub struct CliCommand {
    pub name: String,
    pub description: String,
    pub exec: Vec<String>,
}

/// An account an extension signs in to.
#[derive(Debug, Clone)]
pub struct AccountInfo {
    pub id: String,
    pub label: String,
    /// Model providers it unlocks.
    pub providers: Vec<String>,
    /// Signed in with an API key August asks for.
    pub key: Option<KeyLogin>,
    /// Signed in by the extension's own script (`login`).
    pub login: bool,
}

#[derive(Debug, Clone)]
pub struct KeyLogin {
    /// What to ask for.
    pub label: String,
    /// A variable that stands in for the key.
    pub env: Option<String>,
}

/// A model provider an extension offers.
#[derive(Debug, Clone)]
pub struct ProviderInfo {
    pub id: String,
    pub label: String,
    pub default_model: Option<String>,
}

/// An event an extension declares; others subscribe to it as `<namespace>:<name>`.
#[derive(Debug, Clone)]
pub struct EventDef {
    pub name: String,
    pub description: String,
    /// JSON Schema of its data (null: any object).
    pub schema: Value,
    /// Handlers only observe (run at once, in the background) rather than form a chain.
    pub observe: bool,
}

/// Threads of August's calls into the extension that are still running.
type Busy = Arc<StdMutex<HashMap<Thread, usize>>>;

fn call_thread(params: &Value) -> Option<Thread> {
    ops::thread(&json!({"thread": params["ctx"]["thread"]})).ok()
}

/// Where August writes to the extension.
type Writer = Arc<Mutex<Box<dyn AsyncWrite + Send + Unpin>>>;

pub struct Host {
    stdin: Writer,
    waiting: Waiting,
    next_id: AtomicU64,
    manifest: Arc<RwLock<Manifest>>,
    busy: Busy,
    streams: Streams,
    /// Its process group (the process leads one), killed when the host is dropped; 0 for
    /// an extension linked in.
    pid: i32,
    /// Closes the link of one linked in.
    close: Arc<Notify>,
    exited: Arc<AtomicBool>,
    /// Why the core stopped it, when it did: `(reason, error)`.
    stopped: Arc<StdMutex<Option<(String, String)>>>,
    /// The permissions it may have, when a hook narrowed what it declared.
    limit: Arc<RwLock<Option<Vec<String>>>>,
    pub started: Instant,
}

impl Drop for Host {
    fn drop(&mut self) {
        if !self.exited.load(Ordering::SeqCst) {
            self.kill();
        }
    }
}

/// `m` with its needs cut down to `limit`.
fn limited(mut m: Manifest, limit: &RwLock<Option<Vec<String>>>) -> Manifest {
    if let Some(l) = &*limit.read().unwrap() {
        m.needs.retain(|n| l.contains(n));
    }
    m
}

async fn write_line(stdin: &Mutex<Box<dyn AsyncWrite + Send + Unpin>>, msg: &Value) -> std::io::Result<()> {
    let mut line = msg.to_string();
    line.push('\n');
    let mut stdin = stdin.lock().await;
    stdin.write_all(line.as_bytes()).await?;
    stdin.flush().await
}

fn tail_text(tail: &Tail) -> String {
    tail.lock().unwrap().iter().cloned().collect::<Vec<_>>().join("\n")
}

/// The process behind a link, when there is one.
struct Process {
    pid: i32,
    /// Its exit code, once it ended.
    waited: tokio::task::JoinHandle<Option<i32>>,
    /// Done once its stderr is read to the end.
    stderr_done: tokio::task::JoinHandle<()>,
    tail: Tail,
}

impl Host {
    /// Starts the extension in a process group of its own and waits until it has registered
    /// everything. Its stderr goes to `log` and `on_line`; `on_exit` runs if the process ends
    /// after a successful start.
    pub async fn start(
        mut command: Command,
        name: &str,
        core: Option<Arc<dyn Core>>,
        log: Arc<Log>,
        on_line: Arc<dyn Fn(String) + Send + Sync>,
        on_exit: Box<dyn FnOnce(Exit) + Send>,
    ) -> Result<Host, String> {
        let program = command.as_std().get_program().to_string_lossy().to_string();
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0)
            .spawn()
            .map_err(|e| match e.kind() {
                std::io::ErrorKind::NotFound => format!("`{program}` was not found: is it installed and on PATH?"),
                _ => format!("could not run `{program}`: {e}"),
            })?;
        let pid = child.id().map_or(0, |p| p as i32);
        log.note(&format!("started, pid {pid}"));
        let stdin = child.stdin.take().expect("piped stdin");
        let stdout = child.stdout.take().expect("piped stdout");
        let stderr = child.stderr.take().expect("piped stderr");

        let waited = tokio::spawn(async move {
            let code = child.wait().await.ok().and_then(|s| s.code());
            // What it started dies with it, so a restart doesn't find the old ones running.
            if pid > 0 {
                // SAFETY: plain syscall on the extension's own process group; it may be gone.
                unsafe { libc::killpg(pid, libc::SIGKILL) };
            }
            code
        });
        let tail: Tail = Arc::default();
        let stderr_done = {
            let (tail, log) = (tail.clone(), log.clone());
            tokio::spawn(async move {
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    log.line(&line);
                    on_line(line.clone());
                    let mut t = tail.lock().unwrap();
                    if t.len() == TAIL_LINES {
                        t.pop_front();
                    }
                    t.push_back(line);
                }
            })
        };
        let process = Process { pid, waited, stderr_done, tail };
        Self::serve((Box::new(stdout), Box::new(stdin)), Some(process), name, core, log, on_exit).await
    }

    /// Serves an extension August reaches over `io` rather than a process of its own, and
    /// waits until it has registered everything; `on_exit` runs if the link closes after.
    pub async fn link(io: super::Io, name: &str, core: Option<Arc<dyn Core>>, log: Arc<Log>, on_exit: Box<dyn FnOnce(Exit) + Send>) -> Result<Host, String> {
        log.note("linked");
        Self::serve(io, None, name, core, log, on_exit).await
    }

    async fn serve(
        (stdout, stdin): super::Io,
        process: Option<Process>,
        name: &str,
        core: Option<Arc<dyn Core>>,
        log: Arc<Log>,
        on_exit: Box<dyn FnOnce(Exit) + Send>,
    ) -> Result<Host, String> {
        let stdin: Writer = Arc::new(Mutex::new(stdin));
        let pid = process.as_ref().map_or(0, |p| p.pid);
        let tail = process.as_ref().map(|p| p.tail.clone()).unwrap_or_default();
        let exited: Arc<AtomicBool> = Arc::default();
        let close: Arc<Notify> = Arc::default();
        let waiting: Waiting = Arc::default();
        let manifest: Arc<RwLock<Manifest>> = Arc::default();
        let stopped: Arc<StdMutex<Option<(String, String)>>> = Arc::default();
        let limit: Arc<RwLock<Option<Vec<String>>>> = Arc::default();
        let busy: Busy = Arc::default();
        let streams: Streams = Arc::default();
        let (ready_tx, ready_rx) = oneshot::channel::<()>();
        {
            let (stdin, waiting, tail, name) = (stdin.clone(), waiting.clone(), tail.clone(), name.to_string());
            let (manifest, busy, streams) = (manifest.clone(), busy.clone(), streams.clone());
            let (stopped, limit, log, exited, close) = (stopped.clone(), limit.clone(), log.clone(), exited.clone(), close.clone());
            tokio::spawn(async move {
                let mut ready_tx = Some(ready_tx);
                let mut lines = BufReader::new(stdout).lines();
                loop {
                    let line = tokio::select! {
                        line = lines.next_line() => line,
                        _ = close.notified() => break,
                    };
                    let Ok(Some(line)) = line else { break };
                    let Ok(msg) = serde_json::from_str::<Value>(&line) else {
                        log.line(&line);
                        continue;
                    };
                    match msg["method"].as_str() {
                        // `ready` once started, `manifest` when it registers more later.
                        // A `manifest` during setup only tells what it needs so far.
                        Some(m @ ("ready" | "manifest")) => {
                            *manifest.write().unwrap() = limited(parse_manifest(&msg["params"]), &limit);
                            if m == "ready"
                                && let Some(tx) = ready_tx.take()
                            {
                                tx.send(()).ok();
                            } else if ready_tx.is_none()
                                && let Some(core) = &core
                            {
                                core.changed();
                            }
                        }
                        Some("stream") if msg.get("id").is_none() => {
                            let sender = msg["params"]["request"].as_u64().and_then(|id| streams.lock().unwrap().get(&id).cloned());
                            if let Some(tx) = sender {
                                tx.send(msg["params"]["event"].clone()).ok();
                            }
                        }
                        Some(method) => {
                            let (method, core, stdin, name) = (method.to_string(), core.clone(), stdin.clone(), name.clone());
                            let allowed = allowed(&manifest, &busy, &method, &msg["params"]);
                            tokio::spawn(async move {
                                let served = match allowed {
                                    Ok(()) => serve(core.as_deref(), &name, &method, &msg["params"]).await,
                                    Err(e) => Err(e),
                                };
                                let reply = match served {
                                    Ok(result) => json!({"id": msg["id"], "result": result}),
                                    Err(e) => json!({"id": msg["id"], "error": {"message": format!("{e:#}")}}),
                                };
                                write_line(&stdin, &reply).await.ok();
                            });
                        }
                        None => {
                            let Some(tx) = msg["id"].as_u64().and_then(|id| waiting.lock().unwrap().remove(&id)) else {
                                continue;
                            };
                            let result = match msg.get("error") {
                                Some(e) => Err(RpcError {
                                    message: e["message"].as_str().unwrap_or("extension error").to_string(),
                                    kind: e["kind"].as_str().map(String::from),
                                }),
                                None => Ok(msg["result"].clone()),
                            };
                            tx.send(result).ok();
                        }
                    }
                }
                // The extension is gone: fail whatever still waits for it.
                for (_, tx) in waiting.lock().unwrap().drain() {
                    tx.send(Err("the extension process exited".to_string().into())).ok();
                }
                stdin.lock().await.shutdown().await.ok();
                let code = match process {
                    Some(p) => {
                        p.stderr_done.await.ok();
                        p.waited.await.ok().flatten()
                    }
                    None => None,
                };
                exited.store(true, Ordering::SeqCst);
                let by_core = stopped.lock().unwrap().take();
                let why = by_core.as_ref().map_or_else(String::new, |(_, e)| format!(": {e}"));
                let (reason, error) = by_core.unwrap_or_else(|| ("exit".into(), tail_text(&tail)));
                log.note(&match code {
                    Some(c) => format!("exited with code {c} ({reason}{why})"),
                    None => format!("ended ({reason}{why})"),
                });
                if ready_tx.is_none() {
                    on_exit(Exit { reason, error, code });
                }
            });
        }

        let host = Host { stdin, waiting, next_id: AtomicU64::new(1), manifest, busy, streams, pid, close, exited, stopped, limit, started: Instant::now() };
        match tokio::time::timeout(START_TIMEOUT, ready_rx).await {
            Ok(Ok(())) => {
                let speaks = host.manifest.read().unwrap().protocol;
                if speaks != PROTOCOL {
                    return Err(format!("speaks extension protocol {speaks}; this August needs {PROTOCOL} (update its SDK)"));
                }
            }
            Ok(Err(_)) => {
                // Exited before registering; give stderr a moment to drain.
                tokio::time::sleep(Duration::from_millis(50)).await;
                let tail = tail_text(&tail);
                return Err(if tail.is_empty() { "the extension exited during startup".into() } else { tail });
            }
            Err(_) => return Err(format!("did not start within {} s", START_TIMEOUT.as_secs())),
        }
        Ok(host)
    }

    /// Whether it may call `method` with `params` (see `allowed`).
    pub fn allows(&self, method: &str, params: &Value) -> anyhow::Result<()> {
        allowed(&self.manifest, &self.busy, method, params)
    }

    /// Ends it: kills its process group, or closes the link.
    pub fn kill(&self) {
        if self.pid > 0 {
            // SAFETY: plain syscall; the group is the extension's own (`process_group(0)`).
            unsafe { libc::killpg(self.pid, libc::SIGKILL) };
        } else {
            self.close.notify_one();
        }
    }

    pub fn manifest(&self) -> RwLockReadGuard<'_, Manifest> {
        self.manifest.read().unwrap()
    }

    pub fn pid(&self) -> i32 {
        self.pid
    }

    /// Ends it; it then ends as `reason` (`hang`, `unhealthy`) with `error`.
    pub fn stop(&self, reason: &str, error: &str) {
        *self.stopped.lock().unwrap() = Some((reason.into(), error.into()));
        self.kill();
    }

    /// From now on it has at most these permissions, whatever it declares.
    pub fn limit_needs(&self, needs: Vec<String>) {
        self.manifest.write().unwrap().needs.retain(|n| needs.contains(n));
        *self.limit.write().unwrap() = Some(needs);
    }

    pub async fn request(&self, method: &str, params: Value, timeout: Duration) -> Result<Value, String> {
        self.request_with(method, params, timeout, None).await.map_err(|e| e.message)
    }

    /// Like `request`, but `stream` notifications of the extension for this request go to
    /// `stream`; the error keeps the kind the extension gave it. All of them are in the
    /// channel by the time the result returns.
    pub async fn request_with(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
        stream: Option<tokio::sync::mpsc::UnboundedSender<Value>>,
    ) -> Result<Value, RpcError> {
        // While this call runs, the extension may answer in its thread without `messaging`.
        let thread = call_thread(&params);
        if let Some(t) = &thread {
            *self.busy.lock().unwrap().entry(t.clone()).or_default() += 1;
        }
        let result = self.request_inner(method, params, timeout, stream).await;
        if let Some(t) = thread {
            let mut busy = self.busy.lock().unwrap();
            if let Some(n) = busy.get_mut(&t) {
                *n -= 1;
                if *n == 0 {
                    busy.remove(&t);
                }
            }
        }
        result
    }

    async fn request_inner(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
        stream: Option<tokio::sync::mpsc::UnboundedSender<Value>>,
    ) -> Result<Value, RpcError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        struct Unstream<'a>(&'a Streams, u64);
        impl Drop for Unstream<'_> {
            fn drop(&mut self) {
                self.0.lock().unwrap().remove(&self.1);
            }
        }
        let _unstream = stream.map(|tx| {
            self.streams.lock().unwrap().insert(id, tx);
            Unstream(&self.streams, id)
        });
        let (tx, rx) = oneshot::channel();
        self.waiting.lock().unwrap().insert(id, tx);
        // If this call is dropped (its turn was cancelled) or times out, the extension is told
        // to stop working on it.
        let mut guard = CancelOnDrop { id, stdin: self.stdin.clone(), waiting: self.waiting.clone(), done: false };
        let msg = json!({"id": id, "method": method, "params": params});
        if write_line(&self.stdin, &msg).await.is_err() {
            guard.done = true;
            self.waiting.lock().unwrap().remove(&id);
            return Err("the extension process exited".to_string().into());
        }
        let r = match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err("the extension process exited".to_string().into()),
            Err(_) => return Err(RpcError { message: format!("timed out after {} s", timeout.as_secs()), kind: Some("timeout".into()) }),
        };
        guard.done = true;
        r
    }
}

/// Cancels a call the extension is still working on when nobody waits for it any more.
struct CancelOnDrop {
    id: u64,
    stdin: Writer,
    waiting: Waiting,
    done: bool,
}

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        if self.done {
            return;
        }
        self.waiting.lock().unwrap().remove(&self.id);
        let (stdin, msg) = (self.stdin.clone(), json!({"method": "cancel", "params": {"id": self.id}}));
        tokio::spawn(async move { write_line(&stdin, &msg).await.ok() });
    }
}

/// Whether the extension may make this call: the operation's permission (from the core's
/// table) must be among what it declared it needs, and `user` to act for the user. Answering in the thread of a call in
/// progress needs no `messaging`.
fn allowed(manifest: &RwLock<Manifest>, busy: &Busy, method: &str, params: &Value) -> anyhow::Result<()> {
    let Some(op) = ops::find(method) else { anyhow::bail!("unknown method {method}") };
    if params["as_user"] == true && !manifest.read().unwrap().needs.iter().any(|n| n == "user") {
        anyhow::bail!("acting for the user (`as_user`) needs the `user` permission: declare it with august.needs(\"user\")");
    }
    let Some(need) = op.permission else { return Ok(()) };
    if manifest.read().unwrap().needs.iter().any(|n| n == need) {
        return Ok(());
    }
    let own_thread = need == "messaging" && ops::thread(params).is_ok_and(|t| busy.lock().unwrap().contains_key(&t));
    if own_thread {
        return Ok(());
    }
    anyhow::bail!("`{method}` needs the `{need}` permission: declare it with august.needs(\"{need}\")")
}

/// A call from extension `name` into August.
async fn serve(core: Option<&dyn Core>, name: &str, method: &str, params: &Value) -> anyhow::Result<Value> {
    let core = core.ok_or_else(|| anyhow::anyhow!("August is not ready yet"))?;
    core.call(name, method, params).await
}

fn parse_manifest(params: &Value) -> Manifest {
    let list = |k: &str| params[k].as_array().cloned().unwrap_or_default();
    Manifest {
        summary: params["summary"].as_str().unwrap_or_default().to_string(),
        details: params["details"].as_str().unwrap_or_default().to_string(),
        tools: list("tools")
            .iter()
            .filter_map(|t| {
                Some(ToolSpec {
                    name: t["name"].as_str()?.to_string(),
                    description: t["description"].as_str().unwrap_or_default().to_string(),
                    input_schema: t["parameters"].clone(),
                })
            })
            .collect(),
        commands: list("commands")
            .iter()
            .filter_map(|c| Some((c["name"].as_str()?.to_string(), c["description"].as_str().unwrap_or_default().to_string())))
            .collect(),
        accounts: list("accounts")
            .iter()
            .filter_map(|a| {
                Some(AccountInfo {
                    id: a["id"].as_str()?.to_string(),
                    label: a["label"].as_str().unwrap_or_default().to_string(),
                    providers: a["providers"].as_array().into_iter().flatten().filter_map(|p| p.as_str().map(String::from)).collect(),
                    key: a["key"]["label"].as_str().map(|label| KeyLogin { label: label.into(), env: a["key"]["env"].as_str().map(String::from) }),
                    login: a["login"] == true,
                })
            })
            .collect(),
        events: list("events").iter().filter_map(|e| e.as_str().map(String::from)).collect(),
        needs: list("needs").iter().filter_map(|e| e.as_str().map(String::from)).collect(),
        timeouts: params["timeouts"].as_object().into_iter().flatten().filter_map(|(k, v)| Some((k.clone(), v.as_u64()?))).collect(),
        protocol: params["protocol"].as_u64().unwrap_or(1),
        settings: params["settings"].clone(),
        sections: list("sections")
            .iter()
            .filter_map(|c| Some((c["name"].as_str()?.to_string(), c["text"].as_str().unwrap_or_default().to_string())))
            .collect(),
        emits: list("emits")
            .iter()
            .filter_map(|e| {
                Some(EventDef {
                    name: e["name"].as_str()?.to_string(),
                    description: e["description"].as_str().unwrap_or_default().to_string(),
                    schema: e["schema"].clone(),
                    observe: e["observe"] == true,
                })
            })
            .collect(),
        replaces: list("replaces").iter().filter_map(|e| e.as_str().map(String::from)).collect(),
        takes: list("takes").iter().filter_map(|e| e.as_str().map(String::from)).collect(),
        messengers: list("messengers").into_iter().filter_map(|m| serde_json::from_value(m).ok()).collect(),
        cli: list("cli")
            .iter()
            .filter_map(|c| {
                let exec: Vec<String> = c["exec"].as_array()?.iter().filter_map(|a| a.as_str().map(String::from)).collect();
                Some(CliCommand { name: c["name"].as_str()?.to_string(), description: c["description"].as_str().unwrap_or_default().to_string(), exec })
            })
            .filter(|c| !c.exec.is_empty())
            .collect(),
        providers: list("providers")
            .iter()
            .filter_map(|p| {
                Some(ProviderInfo {
                    id: p["id"].as_str()?.to_string(),
                    label: p["label"].as_str().unwrap_or_default().to_string(),
                    default_model: p["default_model"].as_str().map(String::from),
                })
            })
            .collect(),
    }
}
