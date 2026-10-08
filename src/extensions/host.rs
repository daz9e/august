//! One extension process (`bun run host.ts <entry> <name>`, or a default extension's binary)
//! and the JSON-RPC link to it: one JSON object per line on stdin/stdout, requests in both
//! directions.

use super::Core;
use crate::llm::ToolSpec;
use crate::gateway::ops;
use crate::messengers::Thread;
use serde_json::{Value, json};
use std::collections::{HashMap, VecDeque};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex, RwLock, RwLockReadGuard};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{Mutex, oneshot};

const START_TIMEOUT: Duration = Duration::from_secs(30);
/// The version of the protocol this August speaks (`ready.protocol`).
pub const PROTOCOL: u64 = 2;
/// Stderr lines kept to explain a failed start or a crash.
const TAIL_LINES: usize = 20;

type Waiting = Arc<StdMutex<HashMap<u64, oneshot::Sender<Result<Value, String>>>>>;
type Tail = Arc<StdMutex<VecDeque<String>>>;

/// What an extension has registered (sent once it started, again whenever it changes).
#[derive(Debug, Default)]
pub struct Manifest {
    pub tools: Vec<ToolSpec>,
    pub commands: Vec<(String, String)>,
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
}

/// Threads of August's calls into the extension that are still running.
type Busy = Arc<StdMutex<HashMap<Thread, usize>>>;

fn call_thread(params: &Value) -> Option<Thread> {
    ops::thread(&json!({"thread": params["ctx"]["thread"]})).ok()
}

pub struct Host {
    stdin: Arc<Mutex<ChildStdin>>,
    waiting: Waiting,
    next_id: AtomicU64,
    manifest: Arc<RwLock<Manifest>>,
    busy: Busy,
    /// Killed when the host is dropped.
    _child: Child,
}

async fn write_line(stdin: &Mutex<ChildStdin>, msg: &Value) -> std::io::Result<()> {
    let mut line = msg.to_string();
    line.push('\n');
    let mut stdin = stdin.lock().await;
    stdin.write_all(line.as_bytes()).await?;
    stdin.flush().await
}

fn tail_text(tail: &Tail) -> String {
    tail.lock().unwrap().iter().cloned().collect::<Vec<_>>().join("\n")
}

impl Host {
    /// Starts the extension and waits until it has registered everything. `on_exit` runs
    /// (with the last stderr lines) if the process dies after a successful start.
    pub async fn start(
        mut command: Command,
        name: &str,
        core: Option<Arc<dyn Core>>,
        on_exit: Box<dyn FnOnce(String) + Send>,
    ) -> Result<Host, String> {
        let program = command.as_std().get_program().to_string_lossy().to_string();
        let mut child = command
            .env("AUGUST_WORKSPACE", crate::config::workspace().unwrap_or_default())
            .env("AUGUST_HOME", crate::config::home())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| format!("could not run {program}: {e}"))?;
        let stdin = Arc::new(Mutex::new(child.stdin.take().expect("piped stdin")));
        let stdout = child.stdout.take().expect("piped stdout");
        let stderr = child.stderr.take().expect("piped stderr");

        let tail: Tail = Arc::default();
        let stderr_done = {
            let (tail, name) = (tail.clone(), name.to_string());
            tokio::spawn(async move {
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    eprintln!("extension {name}: {line}");
                    let mut t = tail.lock().unwrap();
                    if t.len() == TAIL_LINES {
                        t.pop_front();
                    }
                    t.push_back(line);
                }
            })
        };

        let waiting: Waiting = Arc::default();
        let manifest: Arc<RwLock<Manifest>> = Arc::default();
        let busy: Busy = Arc::default();
        let (ready_tx, ready_rx) = oneshot::channel::<()>();
        {
            let (stdin, waiting, tail, name) = (stdin.clone(), waiting.clone(), tail.clone(), name.to_string());
            let (manifest, busy) = (manifest.clone(), busy.clone());
            tokio::spawn(async move {
                let mut ready_tx = Some(ready_tx);
                let mut lines = BufReader::new(stdout).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    let Ok(msg) = serde_json::from_str::<Value>(&line) else {
                        eprintln!("extension {name}: {line}");
                        continue;
                    };
                    match msg["method"].as_str() {
                        // `ready` once started, `manifest` when it registers more later.
                        Some(m @ ("ready" | "manifest")) => {
                            *manifest.write().unwrap() = parse_manifest(&msg["params"]);
                            if m == "manifest"
                                && let Some(core) = &core
                            {
                                core.changed();
                            }
                            if let Some(tx) = ready_tx.take() {
                                tx.send(()).ok();
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
                                Some(e) => Err(e["message"].as_str().unwrap_or("extension error").to_string()),
                                None => Ok(msg["result"].clone()),
                            };
                            tx.send(result).ok();
                        }
                    }
                }
                // The process is gone: fail whatever still waits for it.
                for (_, tx) in waiting.lock().unwrap().drain() {
                    tx.send(Err("the extension process exited".into())).ok();
                }
                stderr_done.await.ok();
                if ready_tx.is_none() {
                    on_exit(tail_text(&tail));
                }
            });
        }

        match tokio::time::timeout(START_TIMEOUT, ready_rx).await {
            Ok(Ok(())) => {
                let speaks = manifest.read().unwrap().protocol;
                if speaks != PROTOCOL {
                    return Err(format!("speaks extension protocol {speaks}; this August needs {PROTOCOL} (update its SDK)"));
                }
            }
            Ok(Err(_)) => {
                // Exited before registering; give stderr a moment to drain.
                let _ = child.wait().await;
                tokio::time::sleep(Duration::from_millis(50)).await;
                let tail = tail_text(&tail);
                return Err(if tail.is_empty() { "the extension exited during startup".into() } else { tail });
            }
            Err(_) => return Err(format!("did not start within {} s", START_TIMEOUT.as_secs())),
        }
        Ok(Host { stdin, waiting, next_id: AtomicU64::new(1), manifest, busy, _child: child })
    }

    pub fn manifest(&self) -> RwLockReadGuard<'_, Manifest> {
        self.manifest.read().unwrap()
    }

    pub async fn request(&self, method: &str, params: Value, timeout: Duration) -> Result<Value, String> {
        // While this call runs, the extension may answer in its thread without `messaging`.
        let thread = call_thread(&params);
        if let Some(t) = &thread {
            *self.busy.lock().unwrap().entry(t.clone()).or_default() += 1;
        }
        let result = self.request_inner(method, params, timeout).await;
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

    async fn request_inner(&self, method: &str, params: Value, timeout: Duration) -> Result<Value, String> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.waiting.lock().unwrap().insert(id, tx);
        // If this call is dropped (its turn was cancelled) or times out, the extension is told
        // to stop working on it.
        let mut guard = CancelOnDrop { id, stdin: self.stdin.clone(), waiting: self.waiting.clone(), done: false };
        let msg = json!({"id": id, "method": method, "params": params});
        if write_line(&self.stdin, &msg).await.is_err() {
            guard.done = true;
            self.waiting.lock().unwrap().remove(&id);
            return Err("the extension process exited".into());
        }
        let r = match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err("the extension process exited".into()),
            Err(_) => return Err(format!("timed out after {} s", timeout.as_secs())),
        };
        guard.done = true;
        r
    }
}

/// Cancels a call the extension is still working on when nobody waits for it any more.
struct CancelOnDrop {
    id: u64,
    stdin: Arc<Mutex<ChildStdin>>,
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
        events: list("events").iter().filter_map(|e| e.as_str().map(String::from)).collect(),
        needs: list("needs").iter().filter_map(|e| e.as_str().map(String::from)).collect(),
        timeouts: params["timeouts"].as_object().into_iter().flatten().filter_map(|(k, v)| Some((k.clone(), v.as_u64()?))).collect(),
        protocol: params["protocol"].as_u64().unwrap_or(1),
        settings: params["settings"].clone(),
        sections: list("sections")
            .iter()
            .filter_map(|c| Some((c["name"].as_str()?.to_string(), c["text"].as_str().unwrap_or_default().to_string())))
            .collect(),
    }
}
