//! `FakeAugust`: an in-memory core for testing an extension on its own. It speaks the
//! extension protocol with the extension's `August` (`FakeAugust::new` hands one out to
//! run), sends it events, tool calls, commands and any other request as the core would,
//! records every call the extension makes, and answers them: the store, messages and
//! settings out of the box, everything else as the test scripts it (`on`), `null` if not.

use crate::{August, Button};
use anyhow::{Result, anyhow};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, oneshot};

type Answer = Arc<dyn Fn(Value) -> Pin<Box<dyn Future<Output = Result<Value>> + Send>> + Send + Sync>;

/// The messenger threads of `ctx` and `thread` belong to.
pub const MESSENGER: &str = "test";
const WAIT: Duration = Duration::from_secs(10);

/// A thread of the fake messenger, as the protocol writes it.
pub fn thread(id: &str) -> Value {
    json!({"messenger": MESSENGER, "id": id})
}

/// The context of a call in thread `id`, outside any turn.
pub fn ctx(id: &str) -> Value {
    json!({"thread": thread(id), "turn": null, "depth": 0})
}

/// The context of a call in thread `id` during turn `turn` (`{id, conversation, show, source, parent, meta}`).
pub fn turn_ctx(id: &str, turn: Value) -> Value {
    json!({"thread": thread(id), "turn": turn, "depth": 0})
}

/// A message the extension sent.
#[derive(Debug, Clone)]
pub struct Sent {
    pub id: String,
    /// `messenger:id`.
    pub thread: String,
    pub text: String,
    pub buttons: Vec<Button>,
    pub files: Vec<String>,
}

/// What a user did in a thread, for listeners (`listen`/`next`).
enum UserEvent {
    Press(String),
    Text(String),
}

struct Listener {
    thread: String,
    buttons: Vec<String>,
    text: bool,
}

#[derive(Default)]
struct State {
    manifest: Option<Value>,
    calls: Vec<(String, Value)>,
    store: BTreeMap<String, Value>,
    settings: Value,
    sent: Vec<Sent>,
    answers: HashMap<String, Answer>,
    listeners: HashMap<u64, Listener>,
    /// What users did, by thread, not yet taken by a listener.
    user: HashMap<String, VecDeque<UserEvent>>,
    /// `stream` notifications by request id.
    streams: HashMap<u64, Vec<Value>>,
}

struct Inner {
    out: mpsc::UnboundedSender<String>,
    next_id: AtomicU64,
    waiting: Mutex<HashMap<u64, oneshot::Sender<Result<Value, String>>>>,
    state: Mutex<State>,
}

/// The fake core. Clones share it.
#[derive(Clone)]
pub struct FakeAugust(Arc<Inner>);

impl FakeAugust {
    /// A fake core and the `August` of an extension linked to it, whose environment is `env`.
    /// Run the extension with it (e.g. `tokio::spawn(serve(august))`), then `started()`.
    pub fn new(env: &[(&str, &str)]) -> (FakeAugust, August) {
        let (ext_end, core_end) = tokio::io::duplex(1 << 20);
        let (read, write) = tokio::io::split(ext_end);
        let env = env.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        let august = August::over(env, read, write);
        let (out, mut lines) = mpsc::unbounded_channel::<String>();
        let fake = FakeAugust(Arc::new(Inner { out, next_id: AtomicU64::new(1), waiting: Mutex::default(), state: Mutex::default() }));
        let (read, mut write) = tokio::io::split(core_end);
        tokio::spawn(async move {
            while let Some(mut line) = lines.recv().await {
                line.push('\n');
                if write.write_all(line.as_bytes()).await.is_err() {
                    break;
                }
            }
        });
        let me = fake.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(read).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let Ok(msg) = serde_json::from_str::<Value>(&line) else { continue };
                me.receive(msg);
            }
        });
        (fake, august)
    }

    fn receive(&self, msg: Value) {
        match (msg["method"].as_str(), msg["id"].as_u64()) {
            (Some("ready" | "manifest"), None) => self.0.state.lock().unwrap().manifest = Some(msg["params"].clone()),
            (Some("stream"), None) => {
                let request = msg["params"]["request"].as_u64().unwrap_or(0);
                self.0.state.lock().unwrap().streams.entry(request).or_default().push(msg["params"]["event"].clone());
            }
            (Some(method), Some(id)) => {
                let (me, method, params) = (self.clone(), method.to_string(), msg["params"].clone());
                tokio::spawn(async move {
                    let reply = match me.serve(&method, params).await {
                        Ok(result) => json!({"id": id, "result": result}),
                        Err(e) => json!({"id": id, "error": {"message": format!("{e:#}")}}),
                    };
                    me.0.out.send(reply.to_string()).ok();
                });
            }
            (None, Some(id)) => {
                let Some(tx) = self.0.waiting.lock().unwrap().remove(&id) else { return };
                let result = match msg.get("error") {
                    Some(e) => Err(e["message"].as_str().unwrap_or("error").to_string()),
                    None => Ok(msg["result"].clone()),
                };
                tx.send(result).ok();
            }
            _ => {}
        }
    }

    /// A call of the extension into August: recorded, then answered.
    async fn serve(&self, method: &str, p: Value) -> Result<Value> {
        let answer = {
            let mut s = self.0.state.lock().unwrap();
            s.calls.push((method.to_string(), p.clone()));
            s.answers.get(method).cloned()
        };
        if let Some(answer) = answer {
            return answer(p).await;
        }
        let key = |p: &Value| format!("{}:{}", p["thread"]["messenger"].as_str().unwrap_or(""), p["thread"]["id"].as_str().unwrap_or(""));
        let mut s = self.0.state.lock().unwrap();
        Ok(match method {
            "store_get" => s.store.get(p["key"].as_str().unwrap_or("")).cloned().unwrap_or(Value::Null),
            "store_set" => {
                let k = p["key"].as_str().unwrap_or("").to_string();
                match &p["value"] {
                    Value::Null => s.store.remove(&k),
                    v => s.store.insert(k, v.clone()),
                };
                Value::Null
            }
            "store_list" => {
                let prefix = p["prefix"].as_str().unwrap_or("");
                Value::Array(s.store.iter().filter(|(k, _)| k.starts_with(prefix)).map(|(k, v)| json!({"key": k, "value": v})).collect())
            }
            "settings" => {
                let schema = s.manifest.as_ref().map(|m| m["settings"].clone()).unwrap_or(Value::Null);
                let mut out = if s.settings.is_object() { s.settings.clone() } else { json!({}) };
                for (k, prop) in schema["properties"].as_object().into_iter().flatten() {
                    if out.get(k).is_none_or(Value::is_null) && let Some(d) = prop.get("default") {
                        out[k] = d.clone();
                    }
                }
                out
            }
            "send" => {
                let m = &p["message"];
                let id = format!("m{}", s.sent.len() + 1);
                let buttons = m["buttons"].as_array().into_iter().flatten().flat_map(|b| b.as_array().cloned().unwrap_or_else(|| vec![b.clone()]));
                let sent = Sent {
                    id: id.clone(),
                    thread: key(&p),
                    text: m.as_str().or(m["text"].as_str()).unwrap_or_default().to_string(),
                    buttons: buttons.filter_map(|b| serde_json::from_value(b).ok()).collect(),
                    files: m["files"].as_array().into_iter().flatten().filter_map(|f| f.as_str().map(String::from)).collect(),
                };
                s.sent.push(sent);
                json!(id)
            }
            "edit" => {
                let text = p["message"].as_str().or(p["message"]["text"].as_str()).unwrap_or_default().to_string();
                if let Some(m) = s.sent.iter_mut().find(|m| m.id == p["id"]) {
                    m.text = text;
                }
                Value::Null
            }
            "listen" => {
                let id = self.0.next_id.fetch_add(1, Ordering::Relaxed);
                let buttons = p["buttons"].as_array().into_iter().flatten().filter_map(|b| b.as_str().map(String::from)).collect();
                s.listeners.insert(id, Listener { thread: key(&p), buttons, text: p["text"] == true });
                json!(id)
            }
            "next" => {
                drop(s);
                return self.next(p["listener"].as_u64().unwrap_or(0), Duration::from_millis(p["timeout_ms"].as_u64().unwrap_or(0))).await;
            }
            _ => Value::Null,
        })
    }

    /// What listener `id` takes within `timeout`: the first press of its buttons or text (if
    /// it takes text) a user sent its thread.
    async fn next(&self, id: u64, timeout: Duration) -> Result<Value> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            {
                let mut s = self.0.state.lock().unwrap();
                let Some(l) = s.listeners.get(&id) else { return Ok(json!({"timeout": true})) };
                let (thread, buttons, text) = (l.thread.clone(), l.buttons.clone(), l.text);
                let queue = s.user.entry(thread).or_default();
                let pos = queue.iter().position(|e| match e {
                    UserEvent::Press(b) => buttons.contains(b),
                    UserEvent::Text(_) => text,
                });
                if let Some(event) = pos.and_then(|i| queue.remove(i)) {
                    s.listeners.remove(&id);
                    return Ok(match event {
                        UserEvent::Press(b) => json!({"press": b}),
                        UserEvent::Text(t) => json!({"text": t}),
                    });
                }
            }
            if tokio::time::Instant::now() >= deadline {
                self.0.state.lock().unwrap().listeners.remove(&id);
                return Ok(json!({"timeout": true}));
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    // ---- scripting ----

    /// Answers the extension's calls of `op` with `answer` (instead of the default).
    pub fn on(&self, op: &str, answer: impl Fn(Value) -> Result<Value> + Send + Sync + 'static) {
        let answer = Arc::new(answer);
        self.on_async(op, move |p| {
            let r = answer(p);
            async move { r }
        });
    }

    /// `on` with an answer that takes its time.
    pub fn on_async<F, R>(&self, op: &str, answer: F)
    where
        F: Fn(Value) -> R + Send + Sync + 'static,
        R: Future<Output = Result<Value>> + Send + 'static,
    {
        let answer: Answer = Arc::new(move |p| Box::pin(answer(p)));
        self.0.state.lock().unwrap().answers.insert(op.into(), answer);
    }

    /// Answers `llm` calls with `answer(prompt, system)`.
    pub fn on_llm(&self, answer: impl Fn(&str, &str) -> Result<String> + Send + Sync + 'static) {
        self.on("llm", move |p| {
            let prompt = p["prompt"].as_str().map(String::from).unwrap_or_else(|| p["messages"].to_string());
            answer(&prompt, p["system"].as_str().unwrap_or_default()).map(Value::String)
        });
    }

    /// The extension's settings (its schema's defaults fill the rest).
    pub fn set_settings(&self, settings: Value) {
        self.0.state.lock().unwrap().settings = settings;
    }

    /// Puts `value` in the extension's store at `key`.
    pub fn store_set(&self, key: &str, value: Value) {
        self.0.state.lock().unwrap().store.insert(key.into(), value);
    }

    /// The press of button `id` in thread `thread`, for a listener.
    pub fn press(&self, thread: &str, id: &str) {
        let key = format!("{MESSENGER}:{thread}");
        self.0.state.lock().unwrap().user.entry(key).or_default().push_back(UserEvent::Press(id.into()));
    }

    /// A text the user sends thread `thread`, for a listener.
    pub fn reply(&self, thread: &str, text: &str) {
        let key = format!("{MESSENGER}:{thread}");
        self.0.state.lock().unwrap().user.entry(key).or_default().push_back(UserEvent::Text(text.into()));
    }

    // ---- driving the extension ----

    /// Waits until the extension has registered everything (`ready`).
    pub async fn started(&self) -> Value {
        self.wait_until("the extension to start", |f| f.manifest().is_some()).await;
        self.manifest().unwrap()
    }

    /// What the extension registers, as it last told.
    pub fn manifest(&self) -> Option<Value> {
        self.0.state.lock().unwrap().manifest.clone()
    }

    /// Sends the extension `method` with `params`, as the core does; its result.
    pub async fn request(&self, method: &str, params: Value) -> Result<Value> {
        self.request_id(method, params).1.await
    }

    /// `request`, with the request's id (for its `stream` notifications and `cancel`).
    pub fn request_id(&self, method: &str, params: Value) -> (u64, impl Future<Output = Result<Value>> + use<>) {
        let id = self.0.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.0.waiting.lock().unwrap().insert(id, tx);
        self.0.out.send(json!({"id": id, "method": method, "params": params}).to_string()).ok();
        (id, async move { rx.await.map_err(|_| anyhow!("the extension went away"))?.map_err(|e| anyhow!(e)) })
    }

    /// Tells the extension to stop working on request `id`.
    pub fn cancel(&self, id: u64) {
        self.0.out.send(json!({"method": "cancel", "params": {"id": id}}).to_string()).ok();
    }

    /// Runs the extension's handlers of `event` with `data` in `ctx`; the data they leave.
    pub async fn event(&self, event: &str, data: Value, ctx: Value) -> Result<Value> {
        self.request("event", json!({"name": event, "data": data, "ctx": ctx})).await
    }

    /// Calls the extension's tool `name`; its output.
    pub async fn tool(&self, name: &str, input: Value, ctx: Value) -> Result<String> {
        let v = self.request("tool", json!({"name": name, "input": input, "ctx": ctx})).await?;
        Ok(v.as_str().map(String::from).unwrap_or_else(|| v.to_string()))
    }

    /// Runs the extension's command `/name args`; its reply.
    pub async fn command(&self, name: &str, args: &str, ctx: Value) -> Result<Option<String>> {
        let v = self.request("command", json!({"name": name, "args": args, "ctx": ctx})).await?;
        Ok(v.as_str().map(String::from))
    }

    /// The `stream` notifications the extension sent for request `id`.
    pub fn streamed(&self, id: u64) -> Vec<Value> {
        self.0.state.lock().unwrap().streams.get(&id).cloned().unwrap_or_default()
    }

    // ---- what the extension did ----

    /// Every call of `op` the extension made, by its params.
    pub fn calls(&self, op: &str) -> Vec<Value> {
        self.0.state.lock().unwrap().calls.iter().filter(|(m, _)| m == op).map(|(_, p)| p.clone()).collect()
    }

    /// Names of every call the extension made, in order.
    pub fn ops(&self) -> Vec<String> {
        self.0.state.lock().unwrap().calls.iter().map(|(m, _)| m.clone()).collect()
    }

    /// Every message the extension sent, as it reads now.
    pub fn sent(&self) -> Vec<Sent> {
        self.0.state.lock().unwrap().sent.clone()
    }

    /// The texts it sent, in order.
    pub fn texts(&self) -> Vec<String> {
        self.sent().into_iter().map(|m| m.text).collect()
    }

    /// The value in the extension's store at `key`.
    pub fn stored(&self, key: &str) -> Option<Value> {
        self.0.state.lock().unwrap().store.get(key).cloned()
    }

    /// Waits until `done` holds; panics after a while.
    pub async fn wait_until(&self, what: &str, done: impl Fn(&FakeAugust) -> bool) {
        let start = tokio::time::Instant::now();
        while !done(self) {
            if start.elapsed() > WAIT {
                let calls: Vec<String> = self.0.state.lock().unwrap().calls.iter().map(|(m, p)| format!("{m} {p}")).collect();
                panic!("timed out waiting for {what}; calls so far:\n{}", calls.join("\n"));
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    /// Waits for a message containing `needle`; returns it.
    pub async fn wait_sent(&self, needle: &str) -> Sent {
        self.wait_until(&format!("a message with {needle:?}"), |f| f.texts().iter().any(|t| t.contains(needle))).await;
        self.sent().into_iter().find(|m| m.text.contains(needle)).unwrap()
    }

    /// Waits for the `n`-th call of `op`; returns its params.
    pub async fn wait_call(&self, op: &str, n: usize) -> Value {
        self.wait_until(&format!("call #{n} of {op}"), |f| f.calls(op).len() >= n).await;
        self.calls(op)[n - 1].clone()
    }
}
