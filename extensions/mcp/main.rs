//! MCP client: the servers in `AUGUST_HOME/mcp.json` lend their tools to the agent as
//! `mcp_<server>_<tool>`. Stdio servers run as child processes (one JSON-RPC message per
//! line); `url` servers speak streamable HTTP. Servers that answer within SETUP_WAIT are
//! ready for the first message, slower ones add their tools when they connect. A server
//! that fails to start is reported by `/mcp`; one that crashes loses its tools until it is
//! started again, a few seconds later.

use august_ext::{August, truncate};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{Mutex, Notify, oneshot};

const PROTOCOL: &str = "2025-06-18";
const START_TIMEOUT: Duration = Duration::from_secs(30);
const CALL_TIMEOUT: Duration = Duration::from_secs(600);
/// How long setup waits for servers before the agent starts without the slow ones.
const SETUP_WAIT: Duration = Duration::from_secs(20);
/// Stderr lines kept to explain a failed start or a crash.
const TAIL_LINES: usize = 20;
/// Restarts of a crashed server in a row (1 s, 2 s, 4 s apart) before it stays down.
const RESTARTS: u32 = 3;
/// A server that ran this long before crashing gets its restarts back.
const RESTART_RESET: Duration = Duration::from_secs(60);
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
    /// Notified (once) when the process is gone.
    gone: Arc<Notify>,
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

struct Server {
    link: Link,
    next_id: AtomicU64,
}

enum State {
    Connecting,
    Running { server: Arc<Server>, tools: Vec<String> },
    Failed(String),
}

/// `/mcp` status by server name, in config order.
type Status = Arc<StdMutex<Vec<(String, State)>>>;

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
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
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
                    eprintln!("{name}: {line}");
                    let mut t = tail.lock().unwrap();
                    if t.len() == TAIL_LINES {
                        t.pop_front();
                    }
                    t.push_back(line);
                }
            })
        };

        let (waiting, exited): (Waiting, Arc<StdMutex<Option<String>>>) = Default::default();
        let gone = Arc::new(Notify::new());
        {
            let (stdin, waiting, exited, gone, name) = (stdin.clone(), waiting.clone(), exited.clone(), gone.clone(), name.to_string());
            tokio::spawn(async move {
                let mut lines = BufReader::new(stdout).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    let Ok(msg) = serde_json::from_str::<Value>(&line) else {
                        eprintln!("{name}: {line}");
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
                eprintln!("{name}: exited");
                *exited.lock().unwrap() = Some(why.clone());
                for (_, tx) in waiting.lock().unwrap().drain() {
                    tx.send(Err(why.clone())).ok();
                }
                gone.notify_one();
            });
        }
        Ok(StdioLink { stdin, waiting, exited, gone, _child: child })
    }
}

/// What an HTTP server answers when it no longer knows our session.
const EXPIRED: &str = "the server ended the session (HTTP 404)";

fn init_params() -> Value {
    json!({"protocolVersion": PROTOCOL, "capabilities": {}, "clientInfo": {"name": "august", "version": env!("CARGO_PKG_VERSION")}})
}

/// Messages in an SSE body.
fn sse_messages(body: &str) -> Vec<Value> {
    body.replace("\r\n", "\n")
        .split("\n\n")
        .filter_map(|event| {
            let data: Vec<&str> = event.lines().filter_map(|l| l.strip_prefix("data:")).map(|d| d.strip_prefix(' ').unwrap_or(d)).collect();
            serde_json::from_str(&data.join("\n")).ok()
        })
        .collect()
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
        let session = self.session.lock().unwrap().clone();
        if let Some(s) = &session {
            req = req.header("mcp-session-id", s);
        }
        let resp = req.send().await.map_err(|e| format!("{e}"))?;
        let status = resp.status();
        if status == reqwest::StatusCode::NOT_FOUND && session.is_some() {
            *self.session.lock().unwrap() = None;
            return Err(EXPIRED.into());
        }
        if let Some(s) = resp.headers().get("mcp-session-id").and_then(|v| v.to_str().ok()) {
            *self.session.lock().unwrap() = Some(s.to_string());
        }
        let sse = resp.headers().get("content-type").and_then(|v| v.to_str().ok()).is_some_and(|c| c.starts_with("text/event-stream"));
        // ponytail: reads the whole stream; fine while servers close it after the reply.
        let body = resp.text().await.map_err(|e| format!("{e}"))?;
        if !status.is_success() {
            return Err(format!("HTTP {status}: {}", body.trim()));
        }
        if msg.get("id").is_none() {
            return Ok(None);
        }
        let reply = if sse { sse_messages(&body).into_iter().find(|m| m.get("id") == msg.get("id")) } else { serde_json::from_str(&body).ok() };
        reply.map(Some).ok_or_else(|| "no response from the server".into())
    }
}

impl Server {
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
                let call = async {
                    match h.post(&msg).await {
                        // The server forgot our session (e.g. it restarted): start a new one.
                        Err(e) if e == EXPIRED => {
                            h.post(&json!({"jsonrpc": "2.0", "id": 0, "method": "initialize", "params": init_params()})).await?;
                            h.post(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"})).await?;
                            h.post(&msg).await
                        }
                        r => r,
                    }
                };
                let reply = tokio::time::timeout(timeout, call).await.map_err(|_| timed_out())??;
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

/// `mcp_<server>_<tool>` in the charset providers accept, capped in length, with `_2`, `_3`,
/// ... when cleaning or capping made it the name of a tool in `taken`. Adds it to `taken`.
fn tool_name(taken: &mut HashSet<String>, server: &str, tool: &str) -> String {
    let full: String = format!("mcp_{server}_{tool}")
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect();
    let mut name: String = full.chars().take(MAX_NAME).collect();
    let mut n = 1;
    while !taken.insert(name.clone()) {
        n += 1;
        let suffix = format!("_{n}");
        name = full.chars().take(MAX_NAME - suffix.len()).collect::<String>() + &suffix;
    }
    name
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
    let text = truncate(text, MAX_OUTPUT);
    if result["isError"] == true { Err(text) } else { Ok(text) }
}

/// Connects, lists the tools and registers them; returns the server and its tool names.
async fn connect(august: &August, taken: &StdMutex<HashSet<String>>, name: &str, cfg: &ServerConfig) -> Result<(Arc<Server>, Vec<String>), String> {
    let link = match (&cfg.command, &cfg.url) {
        (Some(command), _) => Link::Stdio(StdioLink::spawn(name, command, cfg)?),
        (None, Some(url)) => Link::Http(HttpLink { client: reqwest::Client::new(), url: url.clone(), headers: cfg.headers.clone(), session: StdMutex::default() }),
        (None, None) => return Err("needs a `command` or a `url`".into()),
    };
    let server = Arc::new(Server { link, next_id: AtomicU64::new(1) });
    server.request("initialize", init_params(), START_TIMEOUT).await?;
    server.notify("notifications/initialized").await?;
    let mut found = Vec::new();
    let mut cursor = Value::Null;
    loop {
        let params = if cursor.is_null() { json!({}) } else { json!({"cursor": cursor}) };
        let page = server.request("tools/list", params, START_TIMEOUT).await?;
        found.extend(page["tools"].as_array().cloned().unwrap_or_default());
        cursor = page["nextCursor"].clone();
        if !cursor.is_string() {
            break;
        }
    }
    if let Some(why) = server.crashed() {
        return Err(why);
    }
    let mut tools = Vec::new();
    for t in found {
        let Some(remote) = t["name"].as_str().map(String::from) else { continue };
        let local = tool_name(&mut taken.lock().unwrap(), name, &remote);
        let description = t["description"].as_str().filter(|d| !d.is_empty()).map(String::from).unwrap_or_else(|| format!("Tool `{remote}` of the MCP server `{name}`"));
        let schema = if t["inputSchema"].is_object() { t["inputSchema"].clone() } else { json!({"type": "object"}) };
        let (server, name) = (server.clone(), name.to_string());
        august.register_tool(&local, &description, schema, move |input, _| {
            let (server, remote, name) = (server.clone(), remote.clone(), name.clone());
            async move {
                let args = if input.is_object() { input } else { json!({}) };
                let result = server.request("tools/call", json!({"name": remote, "arguments": args}), CALL_TIMEOUT).await;
                result.and_then(|r| render(&r)).map_err(|e| match server.crashed() {
                    Some(_) => anyhow::anyhow!("the MCP server `{name}` crashed ({e}); August restarts it, so its tools are back in a few seconds unless it keeps crashing"),
                    None => anyhow::anyhow!(e),
                })
            }
        });
        tools.push(local);
    }
    Ok((server, tools))
}

/// Connects the server and, after a stdio server crashes, removes its tools and starts it
/// again, unless it crashed RESTARTS times without running RESTART_RESET in between.
/// `first` is told when the first attempt is over.
async fn keep(august: August, status: Status, taken: Arc<StdMutex<HashSet<String>>>, name: String, cfg: ServerConfig, first: oneshot::Sender<()>) {
    let mut first = Some(first);
    let mut crashes = 0;
    loop {
        let connected = connect(&august, &taken, &name, &cfg).await;
        first.take().map(|tx| tx.send(()));
        let (server, tools) = match connected {
            Ok(c) => c,
            Err(e) => {
                eprintln!("{name}: {e}");
                return set(&status, &name, State::Failed(e));
            }
        };
        let up = Instant::now();
        set(&status, &name, State::Running { server: server.clone(), tools: tools.clone() });
        let Link::Stdio(s) = &server.link else { return };
        s.gone.notified().await;
        for t in &tools {
            august.unregister_tool(t);
            taken.lock().unwrap().remove(t);
        }
        crashes = if up.elapsed() > RESTART_RESET { 1 } else { crashes + 1 };
        if crashes > RESTARTS {
            return eprintln!("{name}: crashed {RESTARTS} times in a row, not restarting");
        }
        tokio::time::sleep(Duration::from_secs(1 << (crashes - 1))).await;
        set(&status, &name, State::Connecting);
    }
}

fn set(status: &Status, name: &str, state: State) {
    if let Some(slot) = status.lock().unwrap().iter_mut().find(|(n, _)| n == name) {
        slot.1 = state;
    }
}

fn report(status: &Status, config: &PathBuf) -> String {
    let servers = status.lock().unwrap();
    if servers.is_empty() {
        return format!("No MCP servers. Configure them in `{}`.", config.display());
    }
    let line = |(name, state): &(String, State)| match state {
        State::Connecting => format!("⏳ {name} — connecting"),
        State::Failed(e) => format!("❌ {name} — {}", e.trim()),
        State::Running { server, tools } => match server.crashed() {
            Some(why) => format!("❌ {name} — crashed: {}", why.trim()),
            None if tools.is_empty() => format!("✅ {name} — no tools"),
            None => format!("✅ {name} — tools: {}", tools.join(", ")),
        },
    };
    servers.iter().map(line).collect::<Vec<_>>().join("\n")
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    serve(August::new()).await;
}

async fn serve(august: August) {
    let home = august.env("AUGUST_HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(august.env("HOME").unwrap_or_default()).join(".august"));
    let config_path = home.join("mcp.json");
    let status: Status = Arc::default();
    // A server that failed is outside this extension (its command, its address): degraded.
    let watched = status.clone();
    august.health(move || {
        let failed: Vec<String> = watched.lock().unwrap().iter().filter_map(|(n, s)| match s {
            State::Failed(e) => Some(format!("{n}: {e}")),
            _ => None,
        }).collect();
        async move {
            Ok(match failed.is_empty() {
                true => json!({"status": "ok"}),
                false => json!({"status": "degraded", "detail": format!("MCP servers down — {}", failed.join("; "))}),
            })
        }
    });

    let (s, path) = (status.clone(), config_path.clone());
    august.register_command("mcp", "List MCP servers and their tools", move |_, _| {
        let reply = report(&s, &path);
        async move { Ok(Some(reply)) }
    });

    let config: Config = match std::fs::read_to_string(&config_path) {
        Err(_) => Config::default(),
        Ok(text) => match serde_json::from_str(&text) {
            Ok(c) => c,
            Err(e) => {
                status.lock().unwrap().push(("mcp.json".into(), State::Failed(format!("invalid {}: {e}", config_path.display()))));
                Config::default()
            }
        },
    };
    let taken: Arc<StdMutex<HashSet<String>>> = Arc::default();
    let mut connecting = Vec::new();
    for (name, cfg) in config.servers {
        status.lock().unwrap().push((name.clone(), State::Connecting));
        let (tx, rx) = oneshot::channel();
        tokio::spawn(keep(august.clone(), status.clone(), taken.clone(), name, cfg, tx));
        connecting.push(rx);
    }
    tokio::time::timeout(SETUP_WAIT, futures_util::future::join_all(connecting)).await.ok();
    august.run().await;
}
