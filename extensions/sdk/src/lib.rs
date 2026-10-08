//! SDK for August's default extensions, which are Rust binaries run as their own processes.
//! It speaks the same protocol as `src/extensions/host.ts` does for TypeScript extensions:
//! one JSON-RPC message per line on stdin/stdout, requests in both directions. Every request
//! from August runs as its own task, so a handler can call back into August (`ctx.llm`,
//! `ctx.ask`, ...) while others are served. stdout is the protocol; log with `eprintln!`.

use anyhow::{Result, anyhow, bail};
use serde_json::{Map, Value, json};
use std::collections::HashMap;
use std::future::Future;
use std::io::Write;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;
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

/// A conversation in a messenger: a Telegram chat, a terminal window, ...
#[derive(Clone, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct Thread {
    pub messenger: String,
    pub id: String,
}

impl Thread {
    /// `messenger:id`.
    pub fn key(&self) -> String {
        format!("{}:{}", self.messenger, self.id)
    }
}

/// A button under a message; a press comes back with its `id`.
#[derive(Clone, Debug, serde::Serialize)]
pub struct Button {
    pub id: String,
    pub label: String,
}

/// What a listener took (`August::next`), or why nothing came.
#[derive(Clone, Debug, PartialEq)]
pub enum Reply {
    Press(String),
    Text(String),
    Timeout,
    /// The thread got `/stop` or `/new` (the reason).
    Cancelled(String),
}

impl Link {
    async fn send(&self, thread: &Thread, text: &str, buttons: &[Button]) -> Result<String> {
        let v = self.call("send", json!({"thread": thread, "message": {"text": text, "buttons": buttons}})).await?;
        Ok(v.as_str().unwrap_or_default().to_string())
    }

    async fn ask(&self, thread: &Thread, question: &str, options: &[String], timeout: Duration) -> Result<Option<String>> {
        let key = format!("{:x}", self.next_id.fetch_add(1, Ordering::Relaxed) ^ std::process::id() as u64);
        let buttons: Vec<Button> = options.iter().enumerate().map(|(i, o)| Button { id: format!("{key}.{i}"), label: o.clone() }).collect();
        // Listen before sending, so a quick answer can't slip past.
        let ids: Vec<&str> = buttons.iter().map(|b| b.id.as_str()).collect();
        let ttl = timeout.as_millis() as u64 + 5_000;
        let listener = self.call("listen", json!({"thread": thread, "buttons": ids, "text": true, "ttl_ms": ttl})).await?;
        let text = format!("❓ {question}");
        let id = self.send(thread, &text, &buttons).await?;
        let reply = self.call("next", json!({"listener": listener, "timeout_ms": timeout.as_millis() as u64})).await?;
        let answer = match (reply["press"].as_str(), reply["text"].as_str()) {
            (Some(press), _) => buttons.iter().position(|b| b.id == press).map(|i| options[i].clone()),
            (_, Some(t)) => {
                let t = t.trim();
                let by_number = t.parse::<usize>().ok().and_then(|n| n.checked_sub(1)).and_then(|i| options.get(i));
                let by_name = options.iter().find(|o| o.eq_ignore_ascii_case(t));
                Some(by_number.or(by_name).cloned().unwrap_or_else(|| t.to_string()))
            }
            _ => None,
        };
        let cancelled = reply["cancelled"].is_string();
        let none = if cancelled { "⏹ cancelled" } else { "⌛ no answer" };
        let done = format!("{text}\n→ {}", answer.as_deref().unwrap_or(none));
        self.call("edit", json!({"thread": thread, "id": id, "message": done})).await.ok();
        if cancelled {
            bail!("the user cancelled the question (/stop)");
        }
        Ok(answer)
    }
}

/// The turn a call runs in.
#[derive(Clone, Debug, serde::Deserialize)]
pub struct Turn {
    pub id: u64,
    /// `visible`, `quiet`, `fork` or `fresh`.
    pub mode: String,
    pub source: Option<String>,
    pub parent: Option<u64>,
    /// What the turn's starter attached (`turn_start {meta}`).
    #[serde(default)]
    pub meta: Value,
}

/// What a tool, command or hook runs for, and the calls back into August.
#[derive(Clone)]
pub struct Ctx {
    link: Arc<Link>,
    /// The thread the call belongs to.
    pub thread: Option<Thread>,
    /// The turn it runs in.
    pub turn: Option<Turn>,
}

impl Ctx {
    fn thread(&self) -> Result<&Thread> {
        self.thread.as_ref().ok_or_else(|| anyhow!("this call has no thread"))
    }

    async fn in_thread(&self, method: &str, mut params: Value) -> Result<Value> {
        params["thread"] = json!(self.thread()?);
        // So the core knows which turn asks, and hooks of what it runs see that turn.
        if let Some(turn) = &self.turn {
            params["from_turn"] = json!(turn.id);
        }
        self.link.call(method, params).await
    }

    /// Runs operation `op` of the core's table for this thread (`thread` filled in).
    pub async fn call(&self, op: &str, params: Value) -> Result<Value> {
        self.in_thread(op, if params.is_object() { params } else { json!({}) }).await
    }

    /// `messenger:id` of the thread, or `none`.
    pub fn key(&self) -> String {
        self.thread.as_ref().map(Thread::key).unwrap_or_else(|| "none".into())
    }

    /// Sends a Markdown message to the thread; returns its id.
    pub async fn send(&self, text: &str) -> Result<String> {
        self.link.send(self.thread()?, text, &[]).await
    }

    /// Hands the thread `text` as if the user sent it: joins the running turn, or starts one.
    pub async fn prompt(&self, text: &str) -> Result<()> {
        self.in_thread("prompt", json!({"text": text})).await.map(drop)
    }

    /// Like `prompt`, with `opts`: `source` (who it is from, shown to the model; default
    /// `ext:<name>`) and `deliver` (`steer`, `followUp` or `nextTurn`).
    pub async fn prompt_with(&self, text: &str, opts: Value) -> Result<()> {
        let mut params = json!({"text": text});
        params.as_object_mut().unwrap().extend(opts.as_object().cloned().unwrap_or_default());
        self.in_thread("prompt", params).await.map(drop)
    }

    /// Runs a sub-agent with a fresh conversation for the thread; returns its final reply.
    /// `opts`: `{system, tools, exclude}`.
    /// Fails if it failed or was cancelled (/stop cancels all of a thread's turns).
    pub async fn agent(&self, task: &str, opts: Value) -> Result<String> {
        let mut turn = if opts.is_object() { opts } else { json!({}) };
        turn["text"] = json!(task);
        turn["mode"] = json!("fresh");
        turn["parent"] = json!(self.turn.as_ref().map(|t| t.id));
        let id = self.in_thread("turn_start", json!({"turn": turn})).await?;
        let out = self.link.call("turn_wait", json!({"id": id})).await?;
        match out["status"].as_str() {
            Some("ok") => Ok(out["reply"].as_str().unwrap_or_default().to_string()),
            Some("cancelled") => Err(anyhow!("cancelled (/stop)")),
            _ => Err(anyhow!("{}", out["error"].as_str().unwrap_or("the sub-agent failed"))),
        }
    }

    /// Asks in the thread with `options` as buttons; the answer is the option pressed,
    /// numbered or named, the user's own words, or `None` after `timeout`. Fails if the user
    /// cancelled it (/stop).
    pub async fn ask(&self, question: &str, options: &[String], timeout: Duration) -> Result<Option<String>> {
        self.link.ask(self.thread()?, question, options, timeout).await
    }

    /// Runs any agent tool (built-in, MCP or an extension's) for the thread, with its hooks:
    /// `(output, is_error)`.
    pub async fn call_tool(&self, name: &str, input: Value) -> Result<(String, bool)> {
        let v = self.in_thread("callTool", json!({"name": name, "input": input})).await?;
        Ok((v["output"].as_str().unwrap_or_default().to_string(), v["isError"] == true))
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
    sections: RwLock<Vec<(String, String)>>,
    settings: RwLock<Value>,
    needs: RwLock<Vec<String>>,
    timeouts: RwLock<HashMap<String, u64>>,
    /// Calls from August still running, by request id, so a cancel can stop them.
    running: Mutex<HashMap<u64, tokio::task::AbortHandle>>,
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
            sections: RwLock::default(),
            settings: RwLock::new(Value::Null),
            needs: RwLock::default(),
            timeouts: RwLock::default(),
            running: Mutex::default(),
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

    /// This extension's stored value at `key` (JSON), kept across restarts.
    pub async fn get(&self, key: &str) -> Result<Option<Value>> {
        let v = self.0.link.call("store_get", json!({"key": key})).await?;
        Ok((!v.is_null()).then_some(v))
    }

    /// Stores `value` at `key`; `Value::Null` deletes it.
    pub async fn set(&self, key: &str, value: Value) -> Result<()> {
        self.0.link.call("store_set", json!({"key": key, "value": value})).await.map(drop)
    }

    /// `(key, value)` of every stored key starting with `prefix`, in key order.
    pub async fn list(&self, prefix: &str) -> Result<Vec<(String, Value)>> {
        let v = self.0.link.call("store_list", json!({"prefix": prefix})).await?;
        Ok(v.as_array().into_iter().flatten().map(|e| (e["key"].as_str().unwrap_or_default().to_string(), e["value"].clone())).collect())
    }

    /// Every messenger: description, capabilities and threads (see `august.d.ts`).
    pub async fn messengers(&self) -> Result<Value> {
        self.0.link.call("messengers", json!({})).await
    }

    /// Sends a Markdown message with `buttons` to any thread; returns its id.
    pub async fn send(&self, thread: &Thread, text: &str, buttons: &[Button]) -> Result<String> {
        self.0.link.send(thread, text, buttons).await
    }

    pub async fn edit(&self, thread: &Thread, id: &str, text: &str) -> Result<()> {
        self.0.link.call("edit", json!({"thread": thread, "id": id, "message": text})).await.map(drop)
    }

    /// Starts listening in `thread` for a press of one of `buttons` or (with `text`) a text
    /// message; what it takes doesn't reach the agent. Listen before sending the question.
    /// It ends after `ttl` even if `next` is never called.
    pub async fn listen(&self, thread: &Thread, buttons: &[&str], text: bool, ttl: Duration) -> Result<u64> {
        let params = json!({"thread": thread, "buttons": buttons, "text": text, "ttl_ms": ttl.as_millis() as u64});
        let v = self.0.link.call("listen", params).await?;
        v.as_u64().ok_or_else(|| anyhow!("bad listener id"))
    }

    /// What the listener took within `timeout`, or why nothing came.
    pub async fn next(&self, listener: u64, timeout: Duration) -> Result<Reply> {
        let v = self.0.link.call("next", json!({"listener": listener, "timeout_ms": timeout.as_millis() as u64})).await?;
        Ok(match (v["press"].as_str(), v["text"].as_str(), v["cancelled"].as_str()) {
            (Some(p), _, _) => Reply::Press(p.into()),
            (_, Some(t), _) => Reply::Text(t.into()),
            (_, _, Some(why)) => Reply::Cancelled(why.into()),
            _ => Reply::Timeout,
        })
    }

    /// `Ctx::ask` for any thread.
    pub async fn ask(&self, thread: &Thread, question: &str, options: &[String], timeout: Duration) -> Result<Option<String>> {
        self.0.link.ask(thread, question, options, timeout).await
    }

    /// Starts a turn in `thread` (`{text, mode: quiet|fork|fresh, source, parent, system,
    /// tools, exclude, meta}`); returns its id.
    pub async fn start_turn(&self, thread: &Thread, turn: Value) -> Result<u64> {
        let v = self.0.link.call("turn_start", json!({"thread": thread, "turn": turn})).await?;
        v.as_u64().ok_or_else(|| anyhow!("bad turn id"))
    }

    /// The turn's outcome `{status, reply, error, toolCalls}` (once per turn), or
    /// `{status: "running"}` after `timeout`.
    pub async fn wait_turn(&self, id: u64, timeout: Duration) -> Result<Value> {
        self.0.link.call("turn_wait", json!({"id": id, "timeout_ms": timeout.as_millis() as u64})).await
    }

    pub async fn cancel_turn(&self, id: u64) -> Result<bool> {
        Ok(self.0.link.call("turn_cancel", json!({"id": id})).await? == true)
    }

    /// Runs an agent tool for `thread`, with its hooks: `(output, is_error)`.
    pub async fn call_tool(&self, thread: &Thread, name: &str, input: Value) -> Result<(String, bool)> {
        let v = self.0.link.call("callTool", json!({"thread": thread, "name": name, "input": input})).await?;
        Ok((v["output"].as_str().unwrap_or_default().to_string(), v["isError"] == true))
    }

    /// Any operation of the core's table by name (`ops` lists them), e.g.
    /// `call("model_set", json!({"model": "..."}))`; its permission must be declared.
    pub async fn call(&self, op: &str, params: Value) -> Result<Value> {
        self.0.link.call(op, params).await
    }

    /// Declares this extension's settings: a JSON Schema (`properties` with `default`s;
    /// `"secret": true` marks secrets). The user sets them with `/config` or `august config`.
    pub fn settings_schema(&self, schema: Value) {
        *self.0.settings.write().unwrap() = schema;
        self.changed();
    }

    /// This extension's settings, defaults filled in.
    pub async fn settings(&self) -> Result<Value> {
        self.0.link.call("settings", json!({})).await
    }

    /// Hands `thread` a message as if the user sent it.
    pub async fn prompt(&self, thread: &Thread, text: &str) -> Result<()> {
        self.0.link.call("prompt", json!({"thread": thread, "text": text})).await.map(drop)
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

    /// Declares what this extension uses beyond its own thread: `messaging`, `turns`,
    /// `tools`, `llm`, `models`, `sessions`, `memory`, `config`, `admin` (see the guide); other such
    /// calls are refused.
    pub fn needs(&self, permissions: &[&str]) {
        self.0.needs.write().unwrap().extend(permissions.iter().map(|p| p.to_string()));
        self.changed();
    }

    /// A section of the system prompt (Markdown): how and when the model should use what
    /// this extension offers. Fixed per conversation, so changes apply from the next one.
    pub fn register_prompt_section(&self, name: &str, text: &str) {
        let mut sections = self.0.sections.write().unwrap();
        sections.retain(|(n, _)| n != name);
        sections.push((name.into(), text.into()));
        drop(sections);
        self.changed();
    }

    /// How long August waits for this extension's `event` hooks (default 10 s; e.g. longer for
    /// a `tool_call` hook that asks the user).
    pub fn hook_timeout(&self, event: &str, timeout: Duration) {
        self.0.timeouts.write().unwrap().insert(event.into(), timeout.as_millis() as u64);
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
            "needs": *self.0.needs.read().unwrap(),
            "timeouts": *self.0.timeouts.read().unwrap(),
            "protocol": 2,
            "sections": self.0.sections.read().unwrap().iter().map(|(n, t)| json!({"name": n, "text": t})).collect::<Vec<_>>(),
            "settings": *self.0.settings.read().unwrap(),
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
            if msg["method"] == "cancel" {
                if let Some(task) = msg["params"]["id"].as_u64().and_then(|id| self.0.running.lock().unwrap().remove(&id)) {
                    task.abort();
                }
            } else if let Some(method) = msg["method"].as_str().map(String::from) {
                let (me, link, id) = (self.clone(), link.clone(), msg["id"].as_u64());
                let task = tokio::spawn(async move {
                    let thread = serde_json::from_value(msg["params"]["ctx"]["thread"].clone()).ok();
                    let turn = serde_json::from_value(msg["params"]["ctx"]["turn"].clone()).ok();
                    let ctx = Ctx { link: link.clone(), thread, turn };
                    let reply = match me.handle(&method, &msg["params"], ctx).await {
                        Ok(result) => json!({"id": msg["id"], "result": result}),
                        Err(e) => {
                            eprintln!("{method} failed: {e:#}");
                            json!({"id": msg["id"], "error": {"message": format!("{e:#}")}})
                        }
                    };
                    link.write(&reply);
                    if let Some(id) = id {
                        me.0.running.lock().unwrap().remove(&id);
                    }
                });
                if let Some(id) = id {
                    self.0.running.lock().unwrap().insert(id, task.abort_handle());
                }
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

/// Runs `bash -c cmd` in `dir` in a process group of its own, and kills the whole group if
/// it outlives `timeout` or the call is dropped (cancelled), so nothing it started
/// (`sleep`, a server, a pipeline) keeps running behind it. Commands that finish leave what
/// they deliberately put in the background alone.
pub async fn sh(cmd: &str, dir: &std::path::Path, timeout: std::time::Duration) -> Result<std::process::Output> {
    struct Group(Option<i32>);
    impl Drop for Group {
        fn drop(&mut self) {
            if let Some(pid) = self.0 {
                // SAFETY: plain syscall; the group may already be gone.
                unsafe { libc::killpg(pid, libc::SIGKILL) };
            }
        }
    }
    let child = tokio::process::Command::new("bash")
        .arg("-c")
        .arg(cmd)
        .current_dir(dir)
        .process_group(0)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    let mut group = Group(child.id().map(|p| p as i32));
    let out = tokio::time::timeout(timeout, child.wait_with_output())
        .await
        .map_err(|_| anyhow!("timed out after {}s", timeout.as_secs()))??;
    group.0 = None;
    Ok(out)
}
