//! One extension process (`bun run host.ts <entry> <name>`) and the JSON-RPC link to it:
//! one JSON object per line on stdin/stdout, requests in both directions.

use super::Core;
use crate::llm::ToolSpec;
use serde_json::{Value, json};
use std::collections::{HashMap, VecDeque};
use std::path::Path;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{Mutex, oneshot};

const START_TIMEOUT: Duration = Duration::from_secs(30);
/// Stderr lines kept to explain a failed start or a crash.
const TAIL_LINES: usize = 20;

type Waiting = Arc<StdMutex<HashMap<u64, oneshot::Sender<Result<Value, String>>>>>;
type Tail = Arc<StdMutex<VecDeque<String>>>;

/// What an extension registered while starting.
#[derive(Debug, Default)]
pub struct Manifest {
    pub tools: Vec<ToolSpec>,
    pub commands: Vec<(String, String)>,
    pub events: Vec<String>,
}

pub struct Host {
    stdin: Arc<Mutex<ChildStdin>>,
    waiting: Waiting,
    next_id: AtomicU64,
    pub manifest: Manifest,
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
        bun: &Path,
        host_ts: &Path,
        name: &str,
        entry: &Path,
        core: Option<Arc<dyn Core>>,
        on_exit: Box<dyn FnOnce(String) + Send>,
    ) -> Result<Host, String> {
        let mut child = Command::new(bun)
            .arg("run")
            .arg(host_ts)
            .arg(entry)
            .arg(name)
            .current_dir(entry.parent().unwrap_or(Path::new(".")))
            .env("AUGUST_WORKSPACE", crate::config::workspace().unwrap_or_default())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| format!("could not run {}: {e}", bun.display()))?;
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
        let (ready_tx, ready_rx) = oneshot::channel::<Value>();
        {
            let (stdin, waiting, tail, name) = (stdin.clone(), waiting.clone(), tail.clone(), name.to_string());
            tokio::spawn(async move {
                let mut ready_tx = Some(ready_tx);
                let mut lines = BufReader::new(stdout).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    let Ok(msg) = serde_json::from_str::<Value>(&line) else {
                        eprintln!("extension {name}: {line}");
                        continue;
                    };
                    match msg["method"].as_str() {
                        Some("ready") => {
                            if let Some(tx) = ready_tx.take() {
                                tx.send(msg["params"].clone()).ok();
                            }
                        }
                        Some(method) => {
                            let (method, core, stdin) = (method.to_string(), core.clone(), stdin.clone());
                            tokio::spawn(async move {
                                let reply = match serve(core.as_deref(), &method, &msg["params"]).await {
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

        let manifest = match tokio::time::timeout(START_TIMEOUT, ready_rx).await {
            Ok(Ok(params)) => parse_manifest(&params),
            Ok(Err(_)) => {
                // Exited before registering; give stderr a moment to drain.
                let _ = child.wait().await;
                tokio::time::sleep(Duration::from_millis(50)).await;
                let tail = tail_text(&tail);
                return Err(if tail.is_empty() { "the extension exited during startup".into() } else { tail });
            }
            Err(_) => return Err(format!("did not start within {} s", START_TIMEOUT.as_secs())),
        };
        Ok(Host { stdin, waiting, next_id: AtomicU64::new(1), manifest, _child: child })
    }

    pub async fn request(&self, method: &str, params: Value, timeout: Duration) -> Result<Value, String> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.waiting.lock().unwrap().insert(id, tx);
        let msg = json!({"id": id, "method": method, "params": params});
        if write_line(&self.stdin, &msg).await.is_err() {
            self.waiting.lock().unwrap().remove(&id);
            return Err("the extension process exited".into());
        }
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err("the extension process exited".into()),
            Err(_) => {
                self.waiting.lock().unwrap().remove(&id);
                Err(format!("timed out after {} s", timeout.as_secs()))
            }
        }
    }
}

/// A call from the extension into August.
async fn serve(core: Option<&dyn Core>, method: &str, params: &Value) -> anyhow::Result<Value> {
    let core = core.ok_or_else(|| anyhow::anyhow!("August is not ready yet"))?;
    let arg = |k: &str| params[k].as_str().ok_or_else(|| anyhow::anyhow!("missing string `{k}`"));
    Ok(match method {
        "send" => core.send(arg("channel")?, arg("chat")?, arg("text")?).await.map(|_| Value::Null)?,
        "prompt" => core.prompt(arg("channel")?, arg("chat")?, arg("text")?).await.map(|_| Value::Null)?,
        "agent" => {
            let opts = serde_json::from_value(params["opts"].clone()).unwrap_or_default();
            json!(core.agent(arg("channel")?, arg("chat")?, arg("task")?, opts).await?)
        }
        "ask" => {
            let options: Vec<String> = serde_json::from_value(params["options"].clone()).unwrap_or_default();
            if options.is_empty() {
                anyhow::bail!("ask needs at least one option");
            }
            json!(core.ask(arg("channel")?, arg("chat")?, arg("question")?, &options).await?)
        }
        "approve" => json!(core.approve(arg("channel")?, arg("chat")?, arg("action")?).await?),
        "callTool" => {
            let (output, is_error) = core.call_tool(arg("channel")?, arg("chat")?, arg("name")?, &params["input"]).await?;
            json!({"output": output, "isError": is_error})
        }
        "llm" => json!(core.llm(arg("prompt")?, params["system"].as_str().filter(|s| !s.is_empty()).unwrap_or("You are a helpful assistant.")).await?),
        other => anyhow::bail!("unknown method {other}"),
    })
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
    }
}
