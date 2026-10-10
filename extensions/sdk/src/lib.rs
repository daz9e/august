//! SDK for August's default extensions, which are Rust binaries run as their own processes.
//! It speaks the same protocol as `extensions/sdk-ts/host.ts` does for TypeScript extensions:
//! one JSON-RPC message per line on stdin/stdout, requests in both directions. Every request
//! from August runs as its own task, so a handler can call back into August (`ctx.llm`,
//! `ctx.ask`, ...) while others are served. stdout is the protocol; log with `eprintln!`.

pub mod chunk;
pub mod client;
pub mod fake;
pub mod llm;
pub mod messenger;

pub use chunk::split_markdown;
pub use fake::FakeAugust;

use anyhow::{Result, anyhow, bail};
use serde_json::{Map, Value, json};
use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, oneshot};

/// What August writes to the extension, what it writes to August, and the queue of its lines.
type Io = (Box<dyn AsyncRead + Send + Unpin>, Box<dyn AsyncWrite + Send + Unpin>, mpsc::UnboundedReceiver<String>);

type Fut<T> = Pin<Box<dyn Future<Output = Result<T>> + Send>>;
type ToolFn = Arc<dyn Fn(Value, Ctx) -> Fut<String> + Send + Sync>;
type CommandFn = Arc<dyn Fn(String, Ctx) -> Fut<Option<String>> + Send + Sync>;
type CompleteFn = Arc<dyn Fn(llm::Request, Stream) -> Fut<llm::Completion> + Send + Sync>;
type ModelsFn = Arc<dyn Fn(String) -> Fut<Vec<llm::ModelInfo>> + Send + Sync>;
type CheckFn = Arc<dyn Fn(String, String) -> Fut<()> + Send + Sync>;
type LoginFn = Arc<dyn Fn(String, Login) -> Fut<Signed> + Send + Sync>;
type LogoutFn = Arc<dyn Fn(String) -> Fut<()> + Send + Sync>;
/// Returns fields that replace the event's data (`None` leaves it as is).
type HookFn = Arc<dyn Fn(Value, Ctx) -> Fut<Option<Value>> + Send + Sync>;
type HealthFn = Arc<dyn Fn() -> Fut<Value> + Send + Sync>;

struct Link {
    /// Lines for August, written out in order once the extension runs.
    out: mpsc::UnboundedSender<String>,
    next_id: AtomicU64,
    waiting: Mutex<HashMap<u64, oneshot::Sender<Result<Value, String>>>>,
}

impl Link {
    fn write(&self, msg: &Value) {
        self.out.send(msg.to_string()).ok();
    }

    async fn call(&self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.waiting.lock().unwrap().insert(id, tx);
        self.write(&json!({"id": id, "method": method, "params": params}));
        rx.await.map_err(|_| anyhow!("August went away"))?.map_err(|e| anyhow!(e))
    }
}

/// Where a provider sends what the model says as it arrives.
#[derive(Clone)]
pub struct Stream {
    link: Arc<Link>,
    request: u64,
}

impl Stream {
    fn event(&self, event: Value) {
        self.link.write(&json!({"method": "stream", "params": {"request": self.request, "event": event}}));
    }

    /// A piece of the reply text.
    pub fn text(&self, text: &str) {
        if !text.is_empty() {
            self.event(json!({"type": "text", "text": text}));
        }
    }
}

/// Something a user signs in to (a model provider's account, a service an extension
/// uses). August runs the sign-in and keeps its secrets; the extension only scripts it.
struct Account {
    id: String,
    label: String,
    /// Model providers it unlocks (after `/login` August switches to the first).
    providers: Vec<String>,
    /// Signed in with an API key August asks for: `(what to ask, env var that stands in)`.
    key: Option<(String, Option<String>)>,
    check: Option<CheckFn>,
    login: Option<LoginFn>,
    logout: Option<LogoutFn>,
}

impl Account {
    fn manifest(&self) -> Value {
        let key = self.key.as_ref().map(|(label, env)| json!({"label": label, "env": env}));
        json!({"id": self.id, "label": self.label, "providers": self.providers, "key": key, "login": self.login.is_some()})
    }
}

/// A finished sign-in: who is signed in, and until when (unix seconds).
pub struct Signed {
    pub who: String,
    pub expires_at: Option<u64>,
}

/// A sign-in in progress. Every step goes through August, which shows it wherever the
/// user started it (a chat, the terminal) and brings back the answer.
pub struct Login {
    link: Arc<Link>,
    session: u64,
}

impl Login {
    async fn step(&self, op: &str, mut params: Value) -> Result<Value> {
        params["session"] = json!(self.session);
        self.link.call(op, params).await
    }

    /// Asks for a value; with `secret` the answer is deleted from the chat and hidden.
    pub async fn ask(&self, label: &str, secret: bool) -> Result<String> {
        let v = self.step("login_ask", json!({"label": label, "secret": secret})).await?;
        Ok(v.as_str().unwrap_or_default().to_string())
    }

    /// Lets the user pick one of `options`.
    pub async fn choose(&self, question: &str, options: &[&str]) -> Result<String> {
        let v = self.step("login_choose", json!({"question": question, "options": options})).await?;
        Ok(v.as_str().unwrap_or_default().to_string())
    }

    /// Shows `url` to open (and opens it when the user is at this machine).
    pub async fn open(&self, url: &str, note: &str) -> Result<()> {
        self.step("login_open", json!({"url": url, "note": note})).await.map(drop)
    }

    pub async fn progress(&self, text: &str) -> Result<()> {
        self.step("login_progress", json!({"text": text})).await.map(drop)
    }

    /// Starts receiving one redirect on `http://localhost:<port><path>` (`port` 0: any
    /// free one); returns that address.
    pub async fn callback(&self, port: u16, path: &str) -> Result<String> {
        let v = self.step("login_callback", json!({"port": port, "path": path})).await?;
        Ok(v.as_str().unwrap_or_default().to_string())
    }

    /// The redirect's query parameters, once it came (or the user pasted its address).
    pub async fn wait_callback(&self, timeout: Duration) -> Result<Value> {
        self.step("login_wait", json!({"timeout_ms": timeout.as_millis() as u64})).await
    }
}

struct Provider {
    id: String,
    label: String,
    default_model: Option<String>,
    models: ModelsFn,
    complete: CompleteFn,
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

pub use messenger::Button;

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
    /// The conversation it runs in: `thread` (the thread's own), `copy` or `new`.
    pub conversation: String,
    /// Whether it is shown in its thread as it runs.
    #[serde(default)]
    pub show: bool,
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
    /// How many extension events this call is nested in (sent back with `emit`).
    depth: u64,
    /// The id of the request being served.
    request: u64,
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

    /// Runs a declared event (`August::define_event`) through its handlers for this thread
    /// and turn; returns the data they leave (an observed event: `data`, at once).
    pub async fn emit(&self, event: &str, data: Value) -> Result<Value> {
        let mut params = json!({"event": event, "data": data, "thread": self.thread, "depth": self.depth});
        if let Some(turn) = &self.turn {
            params["from_turn"] = json!(turn.id);
        }
        self.link.call("emit", params).await
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
        turn["conversation"] = json!("new");
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

    /// One completion without tools, on the thread's conversation's model (counted in its
    /// usage), or the configured one.
    pub async fn llm(&self, prompt: &str, system: Option<&str>) -> Result<String> {
        self.llm_with(json!({"prompt": prompt, "system": system})).await
    }

    /// `llm` with `{prompt}` or `{messages}` (as the `context` hook has them) and `{system}`.
    pub async fn llm_with(&self, params: Value) -> Result<String> {
        let v = match self.thread {
            Some(_) => self.in_thread("llm", params).await?,
            None => self.link.call("llm", params).await?,
        };
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
    accounts: RwLock<Vec<Account>>,
    hooks: RwLock<Vec<(String, HookFn)>>,
    /// How it says it is doing (`health`); without one, `ok` whenever it answers.
    health: RwLock<Option<HealthFn>>,
    sections: RwLock<Vec<(String, String)>>,
    /// `(summary, details)` from `describe`.
    about: RwLock<(String, String)>,
    settings: RwLock<Value>,
    needs: RwLock<Vec<String>>,
    timeouts: RwLock<HashMap<String, u64>>,
    /// Events it declared (`define_event`), as the manifest lists them.
    emits: RwLock<Vec<Value>>,
    replaces: RwLock<Vec<String>>,
    takes: RwLock<Vec<String>>,
    providers: RwLock<Vec<Provider>>,
    messengers: RwLock<Vec<(messenger::Description, Arc<dyn messenger::Messenger>)>>,
    /// Commands of the `august` program: `(name, description, exec)`.
    cli: RwLock<Vec<(String, String, Vec<String>)>>,
    /// Calls from August still running, by request id, so a cancel can stop them.
    running: Mutex<HashMap<u64, tokio::task::AbortHandle>>,
    /// Set once `ready` was sent; later changes send a new manifest.
    started: AtomicBool,
    /// Its environment, as August started it.
    env: HashMap<String, String>,
    dir: PathBuf,
    workspace: PathBuf,
    /// Taken by `run`.
    io: Mutex<Option<Io>>,
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
    /// The extension August started as a process: the protocol on stdin/stdout, the
    /// process's environment.
    pub fn new() -> Self {
        Self::over(std::env::vars().collect(), tokio::io::stdin(), tokio::io::stdout())
    }

    /// The extension speaking the protocol over `read` and `write` (e.g. hosted in the same
    /// process), with environment `env`.
    pub fn over(env: HashMap<String, String>, read: impl AsyncRead + Send + Unpin + 'static, write: impl AsyncWrite + Send + Unpin + 'static) -> Self {
        let path = |k: &str| PathBuf::from(env.get(k).cloned().unwrap_or_default());
        let (out, lines) = mpsc::unbounded_channel();
        August(Arc::new(Inner {
            link: Arc::new(Link { out, next_id: AtomicU64::new(1), waiting: Mutex::default() }),
            tools: RwLock::default(),
            commands: RwLock::default(),
            accounts: RwLock::default(),
            emits: RwLock::default(),
            replaces: RwLock::default(),
            takes: RwLock::default(),
            providers: RwLock::default(),
            messengers: RwLock::default(),
            cli: RwLock::default(),
            hooks: RwLock::default(),
            health: RwLock::default(),
            sections: RwLock::default(),
            about: RwLock::default(),
            settings: RwLock::new(Value::Null),
            needs: RwLock::default(),
            timeouts: RwLock::default(),
            running: Mutex::default(),
            started: AtomicBool::new(false),
            dir: path("AUGUST_EXTENSION_DIR"),
            workspace: path("AUGUST_WORKSPACE"),
            io: Mutex::new(Some((Box::new(read), Box::new(write), lines))),
            env,
        }))
    }

    /// An environment variable August started it with, unless unset or empty.
    pub fn env(&self, key: &str) -> Option<String> {
        self.0.env.get(key).filter(|v| !v.is_empty()).cloned()
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

    /// Every messenger: description, capabilities and threads (see `extensions/sdk-ts/august.d.ts`).
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

    /// Starts a turn in `thread` (`{text, conversation: thread|copy|new, show, source,
    /// parent, system, tools, exclude, meta}`); returns its id.
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

    /// An account signed in to with an API key: August asks for it (`/login`), runs `check(account,
    /// key)` (an error rejects it) and keeps it; read it with `secret(account)`. `env`: a
    /// variable that stands in for it.
    pub fn register_key_account<F, R>(&self, id: &str, label: &str, providers: &[&str], ask: &str, env: Option<&str>, check: F)
    where
        F: Fn(String, String) -> R + Send + Sync + 'static,
        R: Future<Output = Result<()>> + Send + 'static,
    {
        let check: CheckFn = Arc::new(move |a, k| Box::pin(check(a, k)));
        let key = Some((ask.to_string(), env.map(String::from)));
        self.add_account(Account { id: id.into(), label: label.into(), providers: providers.iter().map(|p| p.to_string()).collect(), key, check: Some(check), login: None, logout: None });
    }

    /// An account with a sign-in of its own (OAuth, a device code, ...): `login(account, steps)`
    /// scripts it with the steps of `Login` and returns who signed in; `logout(account)`
    /// forgets what it keeps. Keep tokens with `set_secret`.
    pub fn register_login_account<F, R, G, GR>(&self, id: &str, label: &str, providers: &[&str], login: F, logout: G)
    where
        F: Fn(String, Login) -> R + Send + Sync + 'static,
        R: Future<Output = Result<Signed>> + Send + 'static,
        G: Fn(String) -> GR + Send + Sync + 'static,
        GR: Future<Output = Result<()>> + Send + 'static,
    {
        let login: LoginFn = Arc::new(move |a, l| Box::pin(login(a, l)));
        let logout: LogoutFn = Arc::new(move |a| Box::pin(logout(a)));
        self.add_account(Account { id: id.into(), label: label.into(), providers: providers.iter().map(|p| p.to_string()).collect(), key: None, check: None, login: Some(login), logout: Some(logout) });
    }

    fn add_account(&self, account: Account) {
        let mut accounts = self.0.accounts.write().unwrap();
        accounts.retain(|a| a.id != account.id);
        accounts.push(account);
        drop(accounts);
        self.changed();
    }

    pub fn unregister_account(&self, id: &str) {
        self.0.accounts.write().unwrap().retain(|a| a.id != id);
        self.changed();
    }

    /// This extension's secret `key` (an account's API key is kept under the account's id).
    pub async fn secret(&self, key: &str) -> Result<Option<String>> {
        let v = self.0.link.call("secret_get", json!({"key": key})).await?;
        Ok(v.as_str().map(String::from))
    }

    /// Keeps (or with `None` deletes) secret `key`.
    pub async fn set_secret(&self, key: &str, value: Option<&str>) -> Result<()> {
        self.0.link.call("secret_set", json!({"key": key, "value": value})).await.map(drop)
    }

    /// Tells August how an account stands: `connected` (by `who`) or `expired` (the user is
    /// told to sign in again) or `none`.
    pub async fn account_update(&self, id: &str, status: &str, who: Option<&str>) -> Result<()> {
        self.0.link.call("account_update", json!({"account": id, "status": status, "who": who})).await.map(drop)
    }

    /// Declares what this extension uses beyond its own thread: `messaging`, `turns`,
    /// `tools`, `llm`, `models`, `sessions`, `memory`, `config`, `admin` (see the guide); other such
    /// calls are refused.
    pub fn needs(&self, permissions: &[&str]) {
        self.0.needs.write().unwrap().extend(permissions.iter().map(|p| p.to_string()));
        self.changed();
    }

    /// What this extension does: a one-line summary and the details.
    pub fn describe(&self, summary: &str, details: &str) {
        *self.0.about.write().unwrap() = (summary.trim().into(), details.trim().into());
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

    /// Declares an event this extension emits; others hook it as `<name>:<event>`. `schema`:
    /// JSON Schema of the data (null: any object). `observe`: handlers only watch; otherwise
    /// they form a chain and may change the data or `block`.
    pub fn define_event(&self, event: &str, description: &str, schema: Value, observe: bool) {
        let mut emits = self.0.emits.write().unwrap();
        emits.retain(|e| e["name"] != event);
        emits.push(json!({"name": event, "description": description, "schema": schema, "observe": observe}));
        drop(emits);
        self.changed();
    }

    /// A model provider. `complete` answers one model call, sending text through the
    /// stream as it arrives; `models(query)` lists what it offers. A provider is chosen by
    /// its `id` (`provider` in `august.json`); calling this again with an id replaces it.
    pub fn register_provider<M, MR, C, CR>(&self, id: &str, label: &str, default_model: Option<&str>, models: M, complete: C)
    where
        M: Fn(String) -> MR + Send + Sync + 'static,
        MR: Future<Output = Result<Vec<llm::ModelInfo>>> + Send + 'static,
        C: Fn(llm::Request, Stream) -> CR + Send + Sync + 'static,
        CR: Future<Output = Result<llm::Completion>> + Send + 'static,
    {
        let provider = Provider {
            id: id.into(),
            label: label.into(),
            default_model: default_model.map(String::from),
            models: Arc::new(move |id| Box::pin(models(id))),
            complete: Arc::new(move |req, stream| Box::pin(complete(req, stream))),
        };
        let mut providers = self.0.providers.write().unwrap();
        providers.retain(|p| p.id != id);
        providers.push(provider);
        drop(providers);
        self.changed();
    }

    pub fn unregister_provider(&self, id: &str) {
        self.0.providers.write().unwrap().retain(|p| p.id != id);
        self.changed();
    }

    /// A messenger: August sends through `messenger` what goes to its threads; what comes
    /// in goes to `inbound`. Its `description.id` names it (`{messenger: id}` in threads);
    /// registering an id again replaces it.
    pub fn register_messenger(&self, description: messenger::Description, messenger: Arc<dyn messenger::Messenger>) {
        let mut all = self.0.messengers.write().unwrap();
        all.retain(|(d, _)| d.id != description.id);
        all.push((description, messenger));
        drop(all);
        self.changed();
    }

    pub fn unregister_messenger(&self, id: &str) {
        self.0.messengers.write().unwrap().retain(|(d, _)| d.id != id);
        self.changed();
    }

    /// A command of the `august` program: `august <name> args...` runs `exec` with the args
    /// appended, in the user's terminal, with `AUGUST_SOCKET` and `AUGUST_TOKEN` to call the
    /// core's operations as this extension (see `Client`). `""` is `august` alone. Of two
    /// with one name, the user's own extension's wins over a default's.
    pub fn register_cli(&self, name: &str, description: &str, exec: &[String]) {
        let mut all = self.0.cli.write().unwrap();
        all.retain(|(n, ..)| n != name);
        all.push((name.into(), description.into(), exec.to_vec()));
        drop(all);
        self.changed();
    }

    pub fn unregister_cli(&self, name: &str) {
        self.0.cli.write().unwrap().retain(|(n, ..)| n != name);
        self.changed();
    }

    /// Hands August what came in to `thread` (of a messenger this extension registered, at
    /// `place`) from `user`.
    pub async fn inbound(&self, thread: &Thread, place: &messenger::Place, user: &messenger::User, kind: &messenger::InboundKind) -> Result<()> {
        let mut params = serde_json::to_value(kind)?;
        params["thread"] = json!(thread);
        params["place"] = serde_json::to_value(place)?;
        params["user"] = serde_json::to_value(user)?;
        self.0.link.call("inbound", params).await.map(drop)
    }

    /// Takes over the event namespace of extensions this one stands in for.
    pub fn replaces(&self, extensions: &[&str]) {
        self.0.replaces.write().unwrap().extend(extensions.iter().map(|e| e.to_string()));
        self.changed();
    }

    /// Does jobs of the core in its place (`render`: draw visible turns from `render` events);
    /// the core leaves them to the first extension that takes them.
    pub fn takes(&self, jobs: &[&str]) {
        self.0.takes.write().unwrap().extend(jobs.iter().map(|j| j.to_string()));
        self.changed();
    }

    /// Runs a declared event outside any thread (inside a handler use `Ctx::emit`).
    pub async fn emit(&self, event: &str, data: Value) -> Result<Value> {
        self.0.link.call("emit", json!({"event": event, "data": data})).await
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

    /// Its health check, which August runs every so often: `{status: ok|degraded|failed,
    /// detail}`. `degraded`: something outside is wrong (restarting won't help); `failed`:
    /// broken inside, August restarts it.
    pub fn health<F, R>(&self, run: F)
    where
        F: Fn() -> R + Send + Sync + 'static,
        R: Future<Output = Result<Value>> + Send + 'static,
    {
        *self.0.health.write().unwrap() = Some(Arc::new(move || Box::pin(run())));
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
        let (summary, details) = self.0.about.read().unwrap().clone();
        json!({
            "summary": summary,
            "details": details,
            "tools": tools.iter().map(|t| json!({"name": t.name, "description": t.description, "parameters": t.parameters})).collect::<Vec<_>>(),
            "commands": commands.iter().map(|(n, d, _)| json!({"name": n, "description": d})).collect::<Vec<_>>(),
            "accounts": self.0.accounts.read().unwrap().iter().map(Account::manifest).collect::<Vec<_>>(),
            "events": events,
            "needs": *self.0.needs.read().unwrap(),
            "timeouts": *self.0.timeouts.read().unwrap(),
            "protocol": 2,
            "sections": self.0.sections.read().unwrap().iter().map(|(n, t)| json!({"name": n, "text": t})).collect::<Vec<_>>(),
            "settings": *self.0.settings.read().unwrap(),
            "emits": *self.0.emits.read().unwrap(),
            "replaces": *self.0.replaces.read().unwrap(),
            "takes": *self.0.takes.read().unwrap(),
            "providers": self.0.providers.read().unwrap().iter().map(|p| json!({"id": p.id, "label": p.label, "default_model": p.default_model})).collect::<Vec<_>>(),
            "messengers": self.0.messengers.read().unwrap().iter().map(|(d, _)| d).collect::<Vec<_>>(),
            "cli": self.0.cli.read().unwrap().iter().map(|(n, d, e)| json!({"name": n, "description": d, "exec": e})).collect::<Vec<_>>(),
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
            "account_check" | "login" | "logout" => {
                let id = params["account"].as_str().unwrap_or_default().to_string();
                let account = self.0.accounts.read().unwrap().iter().find(|a| a.id == id).map(|a| (a.check.clone(), a.login.clone(), a.logout.clone()));
                let (check, login, logout) = account.ok_or_else(|| anyhow!("no account named {id}"))?;
                match (method, check, login, logout) {
                    ("account_check", Some(check), ..) => check(id, params["key"].as_str().unwrap_or_default().to_string()).await.map(|_| Value::Null),
                    ("account_check", None, ..) => Ok(Value::Null),
                    ("login", _, Some(login), _) => {
                        let session = Login { link: ctx.link.clone(), session: params["session"].as_u64().unwrap_or(0) };
                        let signed = login(id, session).await?;
                        Ok(json!({"who": signed.who, "expires_at": signed.expires_at}))
                    }
                    ("logout", _, _, Some(logout)) => logout(id).await.map(|_| Value::Null),
                    ("logout", ..) => Ok(Value::Null),
                    _ => Err(anyhow!("account {id} has no {method}")),
                }
            }
            "complete" => {
                let req = llm::Request::from_json(params).ok_or_else(|| anyhow!("bad complete request"))?;
                let run = self.0.providers.read().unwrap().iter().find(|p| p.id == req.provider).map(|p| p.complete.clone());
                let run = run.ok_or_else(|| anyhow!("no provider named {}", req.provider))?;
                Ok(run(req, Stream { link: ctx.link.clone(), request: ctx.request }).await?.to_json())
            }
            "models" => {
                let id = params["provider"].as_str().unwrap_or_default();
                let run = self.0.providers.read().unwrap().iter().find(|p| p.id == id).map(|p| p.models.clone());
                let run = run.ok_or_else(|| anyhow!("no provider named {id}"))?;
                let models = run(id.to_string()).await?;
                Ok(Value::Array(models.iter().map(llm::ModelInfo::to_json).collect()))
            }
            "health" => {
                let check = self.0.health.read().unwrap().clone();
                match check {
                    Some(check) => check().await,
                    None => Ok(json!({"status": "ok"})),
                }
            }
            m if m.starts_with("messenger_") => self.serve_messenger(m, params).await,
            other => Err(anyhow!("unknown method {other}")),
        }
    }

    /// A call of August to one of this extension's messengers (`messenger_send`, ...).
    async fn serve_messenger(&self, method: &str, p: &Value) -> Result<Value> {
        let id = p["messenger"].as_str().unwrap_or_default();
        let found = self.0.messengers.read().unwrap().iter().find(|(d, _)| d.id == id).map(|(_, m)| m.clone());
        let m = found.ok_or_else(|| anyhow!("no messenger named {id}"))?;
        let thread = p["thread"].as_str().unwrap_or_default();
        let msg_id = p["id"].as_str().unwrap_or_default();
        let message = || serde_json::from_value::<messenger::OutMessage>(p["message"].clone());
        Ok(match method {
            "messenger_send" => json!(m.send(thread, &message()?).await?),
            "messenger_edit" => m.edit(thread, msg_id, &message()?).await.map(|_| Value::Null)?,
            "messenger_delete" => m.delete(thread, msg_id).await.map(|_| Value::Null)?,
            "messenger_react" => m.react(thread, msg_id, p["emoji"].as_str().unwrap_or_default()).await.map(|_| Value::Null)?,
            "messenger_presence" => {
                m.presence(thread, p["busy"] == true).await;
                Value::Null
            }
            "messenger_commands" => m.set_commands(&serde_json::from_value::<Vec<messenger::CommandSpec>>(p["commands"].clone())?).await.map(|_| Value::Null)?,
            "messenger_threads" => json!(m.threads().await),
            "messenger_open_thread" => json!(m.open_thread(p["parent"].as_str().unwrap_or_default(), p["title"].as_str().unwrap_or_default()).await?),
            "messenger_action" => m.action(thread, p["action"].as_str().unwrap_or_default(), p["args"].clone()).await?,
            "messenger_download" => {
                use base64::Engine;
                let bytes = m.download(&serde_json::from_value(p["file"].clone())?).await?;
                json!(base64::engine::general_purpose::STANDARD.encode(bytes))
            }
            other => bail!("unknown method {other}"),
        })
    }

    /// Announces what is registered and serves August until it closes the link.
    pub async fn run(self) {
        let link = self.0.link.clone();
        let (read, mut write, mut out) = self.0.io.lock().unwrap().take().expect("an extension runs once");
        tokio::spawn(async move {
            while let Some(mut line) = out.recv().await {
                line.push('\n');
                if write.write_all(line.as_bytes()).await.is_err() || write.flush().await.is_err() {
                    break;
                }
            }
        });
        link.write(&json!({"method": "ready", "params": self.manifest()}));
        self.0.started.store(true, Ordering::SeqCst);
        let mut lines = BufReader::new(read).lines();
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
                    let depth = msg["params"]["ctx"]["depth"].as_u64().unwrap_or(0);
                    let ctx = Ctx { link: link.clone(), thread, turn, depth, request: id.unwrap_or(0) };
                    let reply = match me.handle(&method, &msg["params"], ctx).await {
                        Ok(result) => json!({"id": msg["id"], "result": result}),
                        Err(e) => {
                            eprintln!("{method} failed: {e:#}");
                            json!({"id": msg["id"], "error": {"message": format!("{e:#}"), "kind": llm::error::ErrorKind::of(&e).as_str()}})
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
