//! SDK for August's default extensions, which are Rust binaries run as their own processes.
//! It speaks the same protocol as `src/extensions/host.ts` does for TypeScript extensions:
//! one JSON-RPC message per line on stdin/stdout, requests in both directions. Every request
//! from August runs as its own task, so a handler can call back into August (`ctx.llm`,
//! `ctx.ask`, ...) while others are served. stdout is the protocol; log with `eprintln!`.

use anyhow::{Result, anyhow};
use serde_json::{Map, Value, json};
use std::collections::HashMap;
use std::future::Future;
use std::io::Write;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::oneshot;

type Fut<T> = Pin<Box<dyn Future<Output = Result<T>> + Send>>;
type ToolFn = Arc<dyn Fn(Value, Ctx) -> Fut<String> + Send + Sync>;
type CommandFn = Arc<dyn Fn(String, Ctx) -> Fut<Option<String>> + Send + Sync>;
/// Returns fields that replace the event's data (`None` leaves it as is).
type HookFn = Arc<dyn Fn(Value, Ctx) -> Fut<Option<Value>> + Send + Sync>;

struct Link {
    out: Mutex<std::io::Stdout>,
    next_id: AtomicU64,
    waiting: Mutex<HashMap<u64, oneshot::Sender<Result<Value, String>>>>,
}

impl Link {
    fn write(&self, msg: &Value) {
        let mut out = self.out.lock().unwrap();
        writeln!(out, "{msg}").ok();
        out.flush().ok();
    }

    async fn call(&self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.waiting.lock().unwrap().insert(id, tx);
        self.write(&json!({"id": id, "method": method, "params": params}));
        rx.await.map_err(|_| anyhow!("August went away"))?.map_err(|e| anyhow!(e))
    }
}

/// A conversation: `channel` (`telegram`, `cli`, ...) and `chat`.
#[derive(Clone, Debug)]
pub struct Chat {
    pub channel: String,
    pub chat: String,
}

/// What a tool, command or hook runs for, and the calls back into August.
#[derive(Clone)]
pub struct Ctx {
    link: Arc<Link>,
    pub chat: Option<Chat>,
}

impl Ctx {
    async fn chat_call(&self, method: &str, mut params: Value) -> Result<Value> {
        let chat = self.chat.as_ref().ok_or_else(|| anyhow!("this call has no chat"))?;
        params["channel"] = json!(chat.channel);
        params["chat"] = json!(chat.chat);
        self.link.call(method, params).await
    }

    /// `channel:chat`, or `cli` without a chat.
    pub fn key(&self) -> String {
        self.chat.as_ref().map(|c| format!("{}:{}", c.channel, c.chat)).unwrap_or_else(|| "cli".into())
    }

    /// Sends a Markdown message to the chat.
    pub async fn send(&self, text: &str) -> Result<()> {
        self.chat_call("send", json!({"text": text})).await.map(drop)
    }

    /// Hands the chat `text` as if the user sent it: joins the running turn, or starts one.
    pub async fn prompt(&self, text: &str) -> Result<()> {
        self.chat_call("prompt", json!({"text": text})).await.map(drop)
    }

    /// Runs a sub-agent with a fresh conversation in the chat; returns its final reply.
    /// `opts`: `{system, tools, exclude}`.
    pub async fn agent(&self, task: &str, opts: Value) -> Result<String> {
        let v = self.chat_call("agent", json!({"task": task, "opts": opts})).await?;
        Ok(v.as_str().unwrap_or_default().to_string())
    }

    /// Asks the user to pick one of `options`; `None` if they didn't answer.
    pub async fn ask(&self, question: &str, options: &[String]) -> Result<Option<String>> {
        let v = self.chat_call("ask", json!({"question": question, "options": options})).await?;
        Ok(v.as_str().map(String::from))
    }

    /// Asks the user whether `action` may run.
    pub async fn approve(&self, action: &str) -> Result<bool> {
        Ok(self.chat_call("approve", json!({"action": action})).await? == true)
    }

    /// One completion on the configured model, without tools.
    pub async fn llm(&self, prompt: &str, system: Option<&str>) -> Result<String> {
        let v = self.link.call("llm", json!({"prompt": prompt, "system": system})).await?;
        Ok(v.as_str().unwrap_or_default().to_string())
    }
}

struct Tool {
    name: String,
    description: String,
    parameters: Value,
    run: ToolFn,
}

struct Inner {
    link: Arc<Link>,
    tools: RwLock<Vec<Tool>>,
    commands: RwLock<Vec<(String, String, CommandFn)>>,
    hooks: RwLock<Vec<(String, HookFn)>>,
    /// Set once `ready` was sent; later changes send a new manifest.
    started: AtomicBool,
    dir: PathBuf,
    workspace: PathBuf,
}

/// The extension: register tools, commands and hooks, then `run()`. Clones share it, so
/// tools can be registered or removed from background tasks at any time.
#[derive(Clone)]
pub struct August(Arc<Inner>);

impl Default for August {
    fn default() -> Self {
        Self::new()
    }
}

impl August {
    pub fn new() -> Self {
        let env = |k: &str| PathBuf::from(std::env::var(k).unwrap_or_default());
        August(Arc::new(Inner {
            link: Arc::new(Link { out: Mutex::new(std::io::stdout()), next_id: AtomicU64::new(1), waiting: Mutex::default() }),
            tools: RwLock::default(),
            commands: RwLock::default(),
            hooks: RwLock::default(),
            started: AtomicBool::new(false),
            dir: env("AUGUST_EXTENSION_DIR"),
            workspace: env("AUGUST_WORKSPACE"),
        }))
    }

    /// The extension's own folder; keep state files here.
    pub fn dir(&self) -> &PathBuf {
        &self.0.dir
    }

    /// The agent's workspace folder (absolute).
    pub fn workspace(&self) -> &PathBuf {
        &self.0.workspace
    }

    /// A tool the model can call; `run` returns its output, an error is reported to the model.
    pub fn register_tool<F, R>(&self, name: &str, description: &str, parameters: Value, run: F)
    where
        F: Fn(Value, Ctx) -> R + Send + Sync + 'static,
        R: Future<Output = Result<String>> + Send + 'static,
    {
        let run: ToolFn = Arc::new(move |input, ctx| Box::pin(run(input, ctx)));
        let tool = Tool { name: name.into(), description: description.into(), parameters, run };
        let mut tools = self.0.tools.write().unwrap();
        tools.retain(|t| t.name != name);
        tools.push(tool);
        drop(tools);
        self.changed();
    }

    pub fn unregister_tool(&self, name: &str) {
        let mut tools = self.0.tools.write().unwrap();
        let before = tools.len();
        tools.retain(|t| t.name != name);
        let removed = tools.len() != before;
        drop(tools);
        if removed {
            self.changed();
        }
    }

    /// `/name args`; a returned string is the reply.
    pub fn register_command<F, R>(&self, name: &str, description: &str, run: F)
    where
        F: Fn(String, Ctx) -> R + Send + Sync + 'static,
        R: Future<Output = Result<Option<String>>> + Send + 'static,
    {
        let run: CommandFn = Arc::new(move |args, ctx| Box::pin(run(args, ctx)));
        self.0.commands.write().unwrap().push((name.into(), description.into(), run));
        self.changed();
    }

    /// A hook; returned fields replace the event's data (`None` leaves it unchanged).
    pub fn on<F, R>(&self, event: &str, run: F)
    where
        F: Fn(Value, Ctx) -> R + Send + Sync + 'static,
        R: Future<Output = Result<Option<Value>>> + Send + 'static,
    {
        let run: HookFn = Arc::new(move |data, ctx| Box::pin(run(data, ctx)));
        self.0.hooks.write().unwrap().push((event.into(), run));
        self.changed();
    }

    fn manifest(&self) -> Value {
        let tools = self.0.tools.read().unwrap();
        let commands = self.0.commands.read().unwrap();
        let mut events: Vec<String> = Vec::new();
        for (e, _) in self.0.hooks.read().unwrap().iter() {
            if !events.contains(e) {
                events.push(e.clone());
            }
        }
        json!({
            "tools": tools.iter().map(|t| json!({"name": t.name, "description": t.description, "parameters": t.parameters})).collect::<Vec<_>>(),
            "commands": commands.iter().map(|(n, d, _)| json!({"name": n, "description": d})).collect::<Vec<_>>(),
            "events": events,
        })
    }

    fn changed(&self) {
        if self.0.started.load(Ordering::SeqCst) {
            self.0.link.write(&json!({"method": "manifest", "params": self.manifest()}));
        }
    }

    async fn handle(&self, method: &str, params: &Value, ctx: Ctx) -> Result<Value> {
        let name = params["name"].as_str().unwrap_or_default();
        match method {
            "event" => {
                let mut data = params["data"].as_object().cloned().unwrap_or_default();
                let hooks: Vec<HookFn> = self.0.hooks.read().unwrap().iter().filter(|(e, _)| e == name).map(|(_, h)| h.clone()).collect();
                for hook in hooks {
                    if let Some(Value::Object(changes)) = hook(Value::Object(data.clone()), ctx.clone()).await? {
                        data.extend(changes);
                    }
                    if stopped(&data) {
                        break;
                    }
                }
                Ok(Value::Object(data))
            }
            "tool" => {
                let run = self.0.tools.read().unwrap().iter().find(|t| t.name == name).map(|t| t.run.clone());
                let run = run.ok_or_else(|| anyhow!("no tool named {name}"))?;
                Ok(Value::String(run(params["input"].clone(), ctx).await?))
            }
            "command" => {
                let run = self.0.commands.read().unwrap().iter().find(|c| c.0 == name).map(|c| c.2.clone());
                let run = run.ok_or_else(|| anyhow!("no command named {name}"))?;
                let reply = run(params["args"].as_str().unwrap_or_default().to_string(), ctx).await?;
                Ok(reply.map(Value::String).unwrap_or(Value::Null))
            }
            other => Err(anyhow!("unknown method {other}")),
        }
    }

    /// Announces what is registered and serves August until it closes stdin.
    pub async fn run(self) {
        let link = self.0.link.clone();
        link.write(&json!({"method": "ready", "params": self.manifest()}));
        self.0.started.store(true, Ordering::SeqCst);
        let mut lines = BufReader::new(tokio::io::stdin()).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let Ok(msg) = serde_json::from_str::<Value>(&line) else { continue };
            if let Some(method) = msg["method"].as_str().map(String::from) {
                let (me, link) = (self.clone(), link.clone());
                tokio::spawn(async move {
                    let chat = msg["params"]["ctx"]["chat"].as_object().map(|c| Chat {
                        channel: str_of(c, "channel"),
                        chat: str_of(c, "chat"),
                    });
                    let ctx = Ctx { link: link.clone(), chat };
                    let reply = match me.handle(&method, &msg["params"], ctx).await {
                        Ok(result) => json!({"id": msg["id"], "result": result}),
                        Err(e) => {
                            eprintln!("{method} failed: {e:#}");
                            json!({"id": msg["id"], "error": {"message": format!("{e:#}")}})
                        }
                    };
                    link.write(&reply);
                });
            } else if let Some(tx) = msg["id"].as_u64().and_then(|id| link.waiting.lock().unwrap().remove(&id)) {
                let result = match msg.get("error") {
                    Some(e) => Err(e["message"].as_str().unwrap_or("error").to_string()),
                    None => Ok(msg["result"].clone()),
                };
                tx.send(result).ok();
            }
        }
    }
}

fn stopped(data: &Map<String, Value>) -> bool {
    let block = data.get("block");
    block == Some(&Value::Bool(true)) || block.and_then(Value::as_str).is_some_and(|s| !s.is_empty()) || data.get("handled") == Some(&Value::Bool(true))
}

fn str_of(c: &Map<String, Value>, k: &str) -> String {
    c.get(k).and_then(Value::as_str).unwrap_or_default().to_string()
}

/// `s` cut to `max` characters, noting how much was cut.
pub fn truncate(s: String, max: usize) -> String {
    let n = s.chars().count();
    if n <= max {
        return s;
    }
    let mut cut: String = s.chars().take(max).collect();
    cut += &format!("\n... [truncated {} chars]", n - max);
    cut
}

/// A string argument of a tool's input.
pub fn str_arg<'a>(input: &'a Value, key: &str) -> &'a str {
    input[key].as_str().unwrap_or_default()
}
