//! MCP client: the servers in `~/.august/mcp.json` lend their tools to the agent as
//! `mcp_<server>_<tool>`. Stdio servers run as child processes (one JSON-RPC message per
//! line); `url` servers speak streamable HTTP. A server that fails to start or crashes
//! is reported by `/mcp` and its tools are left out.

use crate::llm::ToolSpec;
use crate::llm::sse::SseParser;
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::process::Stdio as Piped;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{Mutex, oneshot};

const CONFIG: &str = "mcp.json";
const PROTOCOL: &str = "2025-06-18";
const START_TIMEOUT: Duration = Duration::from_secs(30);
const CALL_TIMEOUT: Duration = Duration::from_secs(600);
/// Stderr lines kept to explain a failed start or a crash.
const TAIL_LINES: usize = 20;
/// Longest tool name providers accept.
const MAX_NAME: usize = 64;
const MAX_OUTPUT: usize = 50_000;

#[derive(Deserialize, Default)]
struct Config {
    #[serde(default)]
    servers: BTreeMap<String, ServerConfig>,
}

#[derive(Deserialize)]
struct ServerConfig {
    command: Option<String>,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env: HashMap<String, String>,
    url: Option<String>,
    #[serde(default)]
    headers: HashMap<String, String>,
}

type Waiting = Arc<StdMutex<HashMap<u64, oneshot::Sender<Result<Value, String>>>>>;

struct StdioLink {
    stdin: Arc<Mutex<ChildStdin>>,
    waiting: Waiting,
    /// Set with the last stderr lines once the process is gone.
    exited: Arc<StdMutex<Option<String>>>,
    /// Killed when dropped.
    _child: Child,
}

struct HttpLink {
    client: reqwest::Client,
    url: String,
    headers: HashMap<String, String>,
    session: StdMutex<Option<String>>,
}

enum Link {
    Stdio(StdioLink),
    Http(HttpLink),
}

struct Running {
    link: Link,
    next_id: AtomicU64,
    /// `(name on the server, spec shown to the model)`.
    tools: Vec<(String, ToolSpec)>,
}

struct Server {
    name: String,
    state: Result<Running, String>,
}

pub struct Mcp {
    servers: Vec<Server>,
}

fn rpc_error(msg: &Value) -> String {
    msg["error"]["message"].as_str().map(String::from).unwrap_or_else(|| msg["error"].to_string())
}

async fn write_line(stdin: &Mutex<ChildStdin>, msg: &Value) -> std::io::Result<()> {
    let mut stdin = stdin.lock().await;
    stdin.write_all(format!("{msg}\n").as_bytes()).await?;
    stdin.flush().await
}

impl StdioLink {
    fn spawn(name: &str, command: &str, cfg: &ServerConfig) -> Result<StdioLink, String> {
        let mut child = Command::new(command)
            .args(&cfg.args)
            .envs(&cfg.env)
            .stdin(Piped::piped())
            .stdout(Piped::piped())
            .stderr(Piped::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| format!("could not run {command}: {e}"))?;
        let stdin = Arc::new(Mutex::new(child.stdin.take().expect("piped stdin")));
        let (stdout, stderr) = (child.stdout.take().expect("piped stdout"), child.stderr.take().expect("piped stderr"));

        let tail: Arc<StdMutex<VecDeque<String>>> = Arc::default();
        let stderr_done = {
            let (tail, name) = (tail.clone(), name.to_string());
            tokio::spawn(async move {
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    eprintln!("mcp {name}: {line}");
                    let mut t = tail.lock().unwrap();
                    if t.len() == TAIL_LINES {
                        t.pop_front();
                    }
                    t.push_back(line);
                }
            })
        };

        let (waiting, exited): (Waiting, Arc<StdMutex<Option<String>>>) = Default::default();
        {
            let (stdin, waiting, exited, name) = (stdin.clone(), waiting.clone(), exited.clone(), name.to_string());
            tokio::spawn(async move {
                let mut lines = BufReader::new(stdout).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    let Ok(msg) = serde_json::from_str::<Value>(&line) else {
                        eprintln!("mcp {name}: {line}");
                        continue;
                    };
                    match (msg.get("method"), msg.get("id")) {
                        // A request from the server: answer pings, decline the rest.
                        (Some(method), Some(id)) => {
                            let reply = if method == "ping" {
                                json!({"jsonrpc": "2.0", "id": id, "result": {}})
                            } else {
                                json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": "method not found"}})
                            };
                            write_line(&stdin, &reply).await.ok();
                        }
                        (Some(_), None) => {} // notification
                        (None, _) => {
                            let Some(tx) = msg["id"].as_u64().and_then(|id| waiting.lock().unwrap().remove(&id)) else {
                                continue;
                            };
                            let result = if msg.get("error").is_some() { Err(rpc_error(&msg)) } else { Ok(msg["result"].clone()) };
                            tx.send(result).ok();
                        }
                    }
                }
                tokio::time::timeout(Duration::from_secs(1), stderr_done).await.ok();
                let tail = tail.lock().unwrap().iter().cloned().collect::<Vec<_>>().join("\n");
                let why = if tail.is_empty() { "the server exited".to_string() } else { format!("the server exited: {tail}") };
                eprintln!("mcp {name}: exited");
                *exited.lock().unwrap() = Some(why.clone());
                for (_, tx) in waiting.lock().unwrap().drain() {
                    tx.send(Err(why.clone())).ok();
                }
            });
        }
        Ok(StdioLink { stdin, waiting, exited, _child: child })
    }
}

impl HttpLink {
    /// POSTs one message; returns the response to it (`None` for notifications).
    async fn post(&self, msg: &Value) -> Result<Option<Value>, String> {
        let mut req = self
            .client
            .post(&self.url)
            .header("accept", "application/json, text/event-stream")
            .header("mcp-protocol-version", PROTOCOL)
            .json(msg);
        for (k, v) in &self.headers {
            req = req.header(k, v);
        }
        if let Some(s) = self.session.lock().unwrap().clone() {
            req = req.header("mcp-session-id", s);
        }
        let resp = req.send().await.map_err(|e| format!("{e}"))?;
        if let Some(s) = resp.headers().get("mcp-session-id").and_then(|v| v.to_str().ok()) {
            *self.session.lock().unwrap() = Some(s.to_string());
        }
        let status = resp.status();
        let sse = resp.headers().get("content-type").and_then(|v| v.to_str().ok()).is_some_and(|c| c.starts_with("text/event-stream"));
        let body = resp.bytes().await.map_err(|e| format!("{e}"))?;
        if !status.is_success() {
            return Err(format!("HTTP {status}: {}", String::from_utf8_lossy(&body).trim()));
        }
        if msg.get("id").is_none() {
            return Ok(None);
        }
        let reply = if sse {
            // ponytail: reads the whole stream; fine while servers close it after the reply.
            SseParser::default()
                .push(&body)
                .iter()
                .filter_map(|e| serde_json::from_str::<Value>(&e.data).ok())
                .find(|m| m.get("id") == msg.get("id"))
        } else {
            serde_json::from_slice(&body).ok()
        };
        reply.map(Some).ok_or_else(|| "no response from the server".into())
    }
}

impl Running {
    async fn request(&self, method: &str, params: Value, timeout: Duration) -> Result<Value, String> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let msg = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        let timed_out = || format!("`{method}` timed out after {} s", timeout.as_secs());
        match &self.link {
            Link::Stdio(s) => {
                let (tx, rx) = oneshot::channel();
                s.waiting.lock().unwrap().insert(id, tx);
                if let Some(why) = s.exited.lock().unwrap().clone() {
                    s.waiting.lock().unwrap().remove(&id);
                    return Err(why);
                }
                if write_line(&s.stdin, &msg).await.is_err() {
                    s.waiting.lock().unwrap().remove(&id);
                    return Err("the server exited".into());
                }
                match tokio::time::timeout(timeout, rx).await {
                    Ok(r) => r.unwrap_or_else(|_| Err("the server exited".into())),
                    Err(_) => {
                        s.waiting.lock().unwrap().remove(&id);
                        Err(timed_out())
                    }
                }
            }
            Link::Http(h) => {
                let reply = tokio::time::timeout(timeout, h.post(&msg)).await.map_err(|_| timed_out())??;
                let reply = reply.unwrap_or_default();
                if reply.get("error").is_some() { Err(rpc_error(&reply)) } else { Ok(reply["result"].clone()) }
            }
        }
    }

    async fn notify(&self, method: &str) -> Result<(), String> {
        let msg = json!({"jsonrpc": "2.0", "method": method});
        match &self.link {
            Link::Stdio(s) => write_line(&s.stdin, &msg).await.map_err(|_| "the server exited".to_string()),
            Link::Http(h) => h.post(&msg).await.map(|_| ()),
        }
    }

    fn crashed(&self) -> Option<String> {
        match &self.link {
            Link::Stdio(s) => s.exited.lock().unwrap().clone(),
            Link::Http(_) => None,
        }
    }
}

/// `mcp_<server>_<tool>` in the charset providers accept, capped in length.
fn tool_name(server: &str, tool: &str) -> String {
    format!("mcp_{server}_{tool}")
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .take(MAX_NAME)
        .collect()
}

async fn connect(name: &str, cfg: &ServerConfig) -> Result<Running, String> {
    let link = match (&cfg.command, &cfg.url) {
        (Some(command), _) => Link::Stdio(StdioLink::spawn(name, command, cfg)?),
        (None, Some(url)) => Link::Http(HttpLink {
            client: reqwest::Client::new(),
            url: url.clone(),
            headers: cfg.headers.clone(),
            session: StdMutex::default(),
        }),
        (None, None) => return Err("needs a `command` or a `url`".into()),
    };
    let mut server = Running { link, next_id: AtomicU64::new(1), tools: Vec::new() };
    let init = json!({
        "protocolVersion": PROTOCOL,
        "capabilities": {},
        "clientInfo": {"name": "august", "version": env!("CARGO_PKG_VERSION")},
    });
    server.request("initialize", init, START_TIMEOUT).await?;
    server.notify("notifications/initialized").await?;
    let mut cursor = Value::Null;
    loop {
        let params = if cursor.is_null() { json!({}) } else { json!({"cursor": cursor}) };
        let page = server.request("tools/list", params, START_TIMEOUT).await?;
        for t in page["tools"].as_array().into_iter().flatten() {
            let Some(remote) = t["name"].as_str() else { continue };
            let schema = if t["inputSchema"].is_object() { t["inputSchema"].clone() } else { json!({"type": "object"}) };
            let spec = ToolSpec {
                name: tool_name(name, remote),
                description: t["description"].as_str().unwrap_or_default().to_string(),
                input_schema: schema,
            };
            server.tools.push((remote.to_string(), spec));
        }
        cursor = page["nextCursor"].clone();
        if !cursor.is_string() {
            break;
        }
    }
    Ok(server)
}

/// Text of a `tools/call` result; non-text blocks are only named. `isError` maps to `Err`.
fn render(result: &Value) -> Result<String, String> {
    let blocks = result["content"].as_array().into_iter().flatten();
    let mut text = blocks
        .map(|b| {
            let kind = b["type"].as_str().unwrap_or("unknown");
            let uri = b["resource"]["uri"].as_str().or(b["uri"].as_str()).unwrap_or("?");
            match kind {
                "text" => b["text"].as_str().unwrap_or_default().to_string(),
                "resource" => b["resource"]["text"].as_str().map(String::from).unwrap_or_else(|| format!("[resource {uri}]")),
                "resource_link" => format!("[resource link {uri}]"),
                _ => format!("[{kind} content ({}) not shown]", b["mimeType"].as_str().unwrap_or("unknown type")),
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    if text.is_empty() && !result["structuredContent"].is_null() {
        text = result["structuredContent"].to_string();
    }
    let text = crate::tools::truncate(text, MAX_OUTPUT);
    if result["isError"] == true { Err(text) } else { Ok(text) }
}

impl Mcp {
    /// Connects to every configured server at once.
    pub async fn start() -> Arc<Mcp> {
        let cfg: Config = match crate::config::load(CONFIG) {
            Ok(c) => c,
            Err(e) => return Arc::new(Mcp { servers: vec![Server { name: CONFIG.into(), state: Err(format!("{e:#}")) }] }),
        };
        let states = futures_util::future::join_all(cfg.servers.iter().map(|(name, c)| connect(name, c))).await;
        let servers = cfg.servers.into_keys().zip(states).map(|(name, state)| Server { name, state }).collect();
        Arc::new(Mcp { servers })
    }

    fn running(&self) -> impl Iterator<Item = &Running> {
        self.servers.iter().filter_map(|s| s.state.as_ref().ok()).filter(|r| r.crashed().is_none())
    }

    /// Tools of all live servers; a name taken by an earlier server is skipped.
    pub fn tool_specs(&self) -> Vec<ToolSpec> {
        let mut seen = HashSet::new();
        self.running().flat_map(|r| &r.tools).filter(|(_, s)| seen.insert(s.name.clone())).map(|(_, s)| s.clone()).collect()
    }

    /// Runs an MCP tool; `None` if no live server has it.
    pub async fn call_tool(&self, name: &str, input: &Value) -> Option<Result<String, String>> {
        let (server, remote) = self.running().find_map(|r| Some((r, &r.tools.iter().find(|(_, s)| s.name == name)?.0)))?;
        let args = if input.is_object() { input.clone() } else { json!({}) };
        let params = json!({"name": remote, "arguments": args});
        Some(server.request("tools/call", params, CALL_TIMEOUT).await.and_then(|r| render(&r)))
    }

    /// One line per server, for `/mcp`.
    pub fn status(&self) -> String {
        if self.servers.is_empty() {
            return format!("No MCP servers. Configure them in `{}`.", crate::config::home().join(CONFIG).display());
        }
        let line = |s: &Server| match &s.state {
            Err(e) => format!("❌ {} — {}", s.name, e.trim()),
            Ok(r) => match r.crashed() {
                Some(why) => format!("❌ {} — crashed: {}", s.name, why.trim()),
                None if r.tools.is_empty() => format!("✅ {} — no tools", s.name),
                None => {
                    let names: Vec<_> = r.tools.iter().map(|(_, t)| t.name.as_str()).collect();
                    format!("✅ {} — tools: {}", s.name, names.join(", "))
                }
            },
        };
        self.servers.iter().map(line).collect::<Vec<_>>().join("\n")
    }
}
