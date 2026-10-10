//! Shared test harness. August runs as the real `august gateway` binary against a fake
//! LLM (an OpenAI-compatible endpoint on a local HTTP server); tests talk to it as a
//! messenger client would, through the terminal messenger's socket (`Gateway::chat`).
//! The same server can play the Telegram Bot API for tests of the Telegram adapter.

#![allow(dead_code)]

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{Method, Uri};
use axum::response::{IntoResponse, Response};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub const TOKEN: &str = "TEST";
pub const OWNER: i64 = 7;
pub const CHAT: i64 = 5;
/// What the fake speech-to-text endpoint hears in any audio.
pub const TRANSCRIPT: &str = "Remind me to water the plants at six.";

/// One request the fake server received.
#[derive(Clone)]
pub struct Req {
    pub path: String,
    pub body: Vec<u8>,
    /// The API key it carried (`x-api-key`, or a bearer token).
    pub key: String,
}

impl Req {
    pub fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or(Value::Null)
    }
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
    /// The Bot API method, e.g. `sendMessage`.
    pub fn method(&self) -> &str {
        self.path.rsplit('/').next().unwrap_or("")
    }
}

/// Answers a chat-completions request body with a full (non-streamed) response.
pub type Llm = Box<dyn Fn(&Value) -> Value + Send + Sync>;

struct Inner {
    log: Mutex<Vec<Req>>,
    updates: Mutex<Vec<Value>>,
    files: HashMap<String, Vec<u8>>,
    llm: Option<Llm>,
    next_id: AtomicI64,
}

pub struct Fake {
    pub url: String,
    inner: Arc<Inner>,
}

impl Fake {
    /// `updates` are delivered on the first `getUpdates`; `files` maps a file id to
    /// its contents for `getFile` + download.
    pub async fn start(updates: Vec<Value>, files: HashMap<String, Vec<u8>>, llm: Option<Llm>) -> Fake {
        let inner = Arc::new(Inner {
            log: Mutex::default(),
            updates: Mutex::new(updates),
            files,
            llm,
            next_id: AtomicI64::new(100),
        });
        let app = axum::Router::new().fallback(handle).with_state(inner.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Fake { url, inner }
    }

    /// Only the fake LLM (no Telegram updates or files).
    pub async fn llm(llm: Llm) -> Fake {
        Fake::start(Vec::new(), HashMap::new(), Some(llm)).await
    }

    /// Queues updates for the next `getUpdates`.
    pub fn push_updates(&self, updates: Vec<Value>) {
        self.inner.updates.lock().unwrap().extend(updates);
    }

    /// Markdown/HTML text of every message the bot sent or edited, in order.
    pub fn sent_texts(&self) -> Vec<String> {
        self.requests()
            .iter()
            .filter(|r| matches!(r.method(), "sendMessage" | "editMessageText"))
            .filter_map(|r| r.json()["text"].as_str().map(String::from))
            .collect()
    }

    pub fn requests(&self) -> Vec<Req> {
        self.inner.log.lock().unwrap().clone()
    }

    /// Bot API calls of one method, in order.
    pub fn calls(&self, method: &str) -> Vec<Req> {
        self.requests().into_iter().filter(|r| r.path.starts_with("/bot") && r.method() == method).collect()
    }

    pub fn llm_requests(&self) -> Vec<Value> {
        self.requests().iter().filter(|r| r.path.ends_with("/chat/completions")).map(Req::json).collect()
    }

    /// Polls until `done` holds, panicking with the request log after `timeout`.
    pub async fn wait_for(&self, timeout: Duration, done: impl Fn(&Fake) -> bool) {
        let start = Instant::now();
        while !done(self) {
            if start.elapsed() > timeout {
                let log: Vec<String> = self.requests().iter().map(|r| format!("{} {}", r.path, r.text())).collect();
                panic!("timed out; requests so far:\n{}", log.join("\n"));
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
}

fn ok(result: Value) -> Response {
    axum::Json(json!({"ok": true, "result": result})).into_response()
}

async fn handle(State(s): State<Arc<Inner>>, method: Method, uri: Uri, headers: axum::http::HeaderMap, body: Bytes) -> Response {
    let path = uri.path().to_string();
    let header = |name: &str| headers.get(name).and_then(|v| v.to_str().ok()).unwrap_or_default().to_string();
    let key = Some(header("x-api-key")).filter(|k| !k.is_empty()).unwrap_or_else(|| header("authorization").trim_start_matches("Bearer ").to_string());
    s.log.lock().unwrap().push(Req { path: path.clone(), body: body.to_vec(), key: key.clone() });

    // The Anthropic Messages API, streamed: the fake LLM's text answer as one delta.
    if path.ends_with("/messages") {
        assert!(s.llm.is_some(), "no fake LLM configured");
        let req: Value = serde_json::from_slice(&body).unwrap();
        let fake = s.clone();
        let reply = tokio::task::spawn_blocking(move || (fake.llm.as_ref().unwrap())(&req)).await.unwrap();
        let text = reply["choices"][0]["message"]["content"].as_str().unwrap_or_default();
        let events = [
            json!({"type": "message_start", "message": {"usage": {"input_tokens": 10}}}),
            json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}),
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": text}}),
            json!({"type": "content_block_stop", "index": 0}),
            json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}, "usage": {"output_tokens": 5}}),
            json!({"type": "message_stop"}),
        ];
        let sse: String = events.iter().map(|e| format!("event: {}\ndata: {e}\n\n", e["type"].as_str().unwrap())).collect();
        return ([("content-type", "text/event-stream")], sse).into_response();
    }

    if path.ends_with("/chat/completions") {
        assert!(s.llm.is_some(), "no fake LLM configured");
        let req: Value = serde_json::from_slice(&body).unwrap();
        // Off the runtime: a fake model may sleep to play a slow one.
        let fake = s.clone();
        let reply = tokio::task::spawn_blocking(move || (fake.llm.as_ref().unwrap())(&req)).await.unwrap();
        return axum::Json(reply).into_response();
    }
    // A model list, for any API; the key `bad` is refused.
    if path.ends_with("/models") {
        if key == "bad" {
            return axum::http::StatusCode::UNAUTHORIZED.into_response();
        }
        return axum::Json(json!({"data": [{"id": "fake-model"}]})).into_response();
    }
    if path.ends_with("/audio/transcriptions") {
        return axum::Json(json!({"text": TRANSCRIPT})).into_response();
    }
    if let Some(file) = path.strip_prefix(&format!("/file/bot{TOKEN}/")) {
        assert_eq!(method, Method::GET);
        return match s.files.get(file) {
            Some(bytes) => bytes.clone().into_response(),
            None => axum::http::StatusCode::NOT_FOUND.into_response(),
        };
    }
    let Some(api) = path.strip_prefix(&format!("/bot{TOKEN}/")) else {
        return axum::http::StatusCode::NOT_FOUND.into_response();
    };
    let params: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    match api {
        "getMe" => ok(json!({"id": 99, "is_bot": true, "username": "AugustBot"})),
        "getUpdates" => {
            let updates = std::mem::take(&mut *s.updates.lock().unwrap());
            if updates.is_empty() {
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            ok(Value::Array(updates))
        }
        "getFile" => {
            let id = params["file_id"].as_str().unwrap_or("");
            ok(json!({"file_id": id, "file_path": id}))
        }
        "sendMessage" | "sendPhoto" | "sendDocument" => {
            ok(json!({"message_id": s.next_id.fetch_add(1, Ordering::SeqCst)}))
        }
        _ => ok(json!(true)),
    }
}

/// A private message from the owner.
pub fn message(id: i64, fields: Value) -> Value {
    let mut m = json!({
        "message_id": id,
        "chat": {"id": CHAT, "type": "private"},
        "from": {"id": OWNER, "is_bot": false, "first_name": "Owner"},
        "date": 0,
    });
    for (k, v) in fields.as_object().unwrap() {
        m[k] = v.clone();
    }
    json!({"update_id": id, "message": m})
}

/// The owner pressing an inline button.
pub fn button_press(id: i64, data: &str) -> Value {
    json!({"update_id": id, "callback_query": {
        "id": format!("cb{id}"),
        "from": {"id": OWNER, "is_bot": false, "first_name": "Owner"},
        "message": {"message_id": 1, "chat": {"id": CHAT, "type": "private"}},
        "data": data,
    }})
}

/// Chat-completions answers for the fake LLM.
pub fn reply_text(text: &str) -> Value {
    json!({"choices": [{"message": {"role": "assistant", "content": text}, "finish_reason": "stop"}]})
}

pub fn reply_tool(name: &str, args: Value) -> Value {
    json!({"choices": [{"message": {"role": "assistant", "content": null, "tool_calls": [{
        "id": "call_1", "type": "function", "function": {"name": name, "arguments": args.to_string()}
    }]}, "finish_reason": "tool_calls"}]})
}

/// Adds a chat-completions `usage` object to a fake reply (`prompt` includes `cached`).
pub fn with_usage(mut reply: Value, prompt: u64, completion: u64, cached: u64) -> Value {
    reply["usage"] = json!({
        "prompt_tokens": prompt,
        "completion_tokens": completion,
        "prompt_tokens_details": {"cached_tokens": cached},
    });
    reply
}

/// The `august gateway` process, killed on drop.
pub struct Gateway {
    child: Child,
    pub workspace: PathBuf,
    /// `AUGUST_HOME`.
    pub home: PathBuf,
    /// How it was started, to start it again (`restart`).
    command: Vec<(String, String)>,
    _dir: tempfile::TempDir,
}

impl Drop for Gateway {
    fn drop(&mut self) {
        self.child.kill().ok();
        self.child.wait().ok();
    }
}

/// How August starts in a test.
#[derive(Default, Clone, Copy)]
pub struct Setup<'a> {
    /// Files put into the workspace.
    pub seed: &'a [(&'a str, &'a [u8])],
    /// Files put into `AUGUST_HOME` (relative paths).
    pub home: &'a [(&'a str, &'a str)],
    /// Extra environment for August.
    pub env: &'a [(&'a str, &'a str)],
    /// Also run the Telegram messenger, against the fake Bot API.
    pub telegram: bool,
}

/// `august gateway` against the fake LLM, with a fresh home and workspace; ready once it
/// listens for chats. Killed on drop.
pub async fn august(fake: &Fake, setup: Setup<'_>) -> Gateway {
    let gw = spawn(fake, setup);
    let socket = gw.home.join("august.sock");
    let start = Instant::now();
    while tokio::net::UnixStream::connect(&socket).await.is_err() {
        assert!(start.elapsed() < Duration::from_secs(20), "August did not listen on {}", socket.display());
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    gw
}

fn spawn(fake: &Fake, setup: Setup) -> Gateway {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    for (path, text) in setup.home {
        let path = home.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }
    let workspace = dir.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    for (name, bytes) in setup.seed {
        std::fs::write(workspace.join(name), bytes).unwrap();
    }
    let mut env: Vec<(String, String)> = vec![
        ("PATH".into(), std::env::var("PATH").unwrap_or_default()),
        ("HOME".into(), dir.path().display().to_string()),
        ("AUGUST_HOME".into(), home.display().to_string()),
        ("AUGUST_WORKSPACE".into(), workspace.display().to_string()),
        ("AUGUST_PROVIDER".into(), "openai".into()),
        ("AUGUST_MODEL".into(), "fake-model".into()),
        ("OPENAI_API_KEY".into(), "test".into()),
        ("OPENAI_BASE_URL".into(), format!("{}/v1", fake.url)),
        // Sign-in links must not open a browser on the machine running the tests.
        ("AUGUST_OPEN_BROWSER".into(), "0".into()),
    ];
    if setup.telegram {
        env.push(("TELEGRAM_API_BASE".into(), fake.url.clone()));
        env.push(("TELEGRAM_BOT_TOKEN".into(), TOKEN.into()));
        env.push(("TELEGRAM_ALLOWED_USERS".into(), OWNER.to_string()));
    }
    env.extend(setup.env.iter().map(|(k, v)| (k.to_string(), v.to_string())));
    let child = start(&env, dir.path());
    Gateway { child, workspace, home, command: env, _dir: dir }
}

fn start(env: &[(String, String)], dir: &Path) -> Child {
    Command::new(env!("CARGO_BIN_EXE_august"))
        .arg("gateway")
        .env_clear()
        .current_dir(dir) // no project .env
        .envs(env.iter().map(|(k, v)| (k, v)))
        .stdout(Stdio::null())
        .stderr(if std::env::var_os("TEST_GATEWAY_LOG").is_some() { Stdio::inherit() } else { Stdio::null() })
        .spawn()
        .expect("start august gateway")
}

impl Gateway {
    /// Stops August and starts it again on the same home and workspace; ready when it listens.
    pub async fn restart(&mut self) {
        self.child.kill().ok();
        self.child.wait().ok();
        self.child = start(&self.command, self._dir.path());
        let socket = self.home.join("august.sock");
        let since = Instant::now();
        while tokio::net::UnixStream::connect(&socket).await.is_err() {
            assert!(since.elapsed() < Duration::from_secs(20), "August did not come back");
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
    }

    /// A new chat: a thread of the terminal messenger, spoken to over its socket.
    pub async fn chat(&self) -> Chat {
        let stream = tokio::net::UnixStream::connect(self.home.join("august.sock")).await.unwrap();
        let (read, mut write) = stream.into_split();
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
        write.write_all(b"{\"type\":\"hello\"}\n").await.unwrap();
        let mut lines = tokio::io::BufReader::new(read).lines();
        let hello: Value = serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
        let seen: Arc<Mutex<Seen>> = Arc::default();
        let sink = seen.clone();
        tokio::spawn(async move {
            while let Ok(Some(line)) = lines.next_line().await {
                sink.lock().unwrap().apply(serde_json::from_str(&line).unwrap());
            }
        });
        Chat { write, seen, thread: hello["thread"].as_str().unwrap().to_string() }
    }

    /// Files in `workspace/inbox`.
    pub fn inbox(&self) -> Vec<PathBuf> {
        let mut v: Vec<PathBuf> = std::fs::read_dir(self.workspace.join("inbox"))
            .map(|d| d.map(|e| e.unwrap().path()).collect())
            .unwrap_or_default();
        v.sort();
        v
    }
}

/// A message August sent to a chat, as it reads now (after edits).
#[derive(Debug, Clone)]
pub struct Msg {
    pub id: String,
    pub text: String,
    /// `(id, label)` of its buttons.
    pub buttons: Vec<(String, String)>,
    /// Edited after it was sent (a question then is settled).
    pub edited: bool,
}

impl Msg {
    /// The id of the button labelled `label`.
    pub fn button(&self, label: &str) -> String {
        self.buttons.iter().find(|(_, l)| l.contains(label)).unwrap_or_else(|| panic!("no button {label:?} in {self:?}")).0.clone()
    }
}

#[derive(Default)]
struct Seen {
    msgs: Vec<Msg>,
    files: Vec<(String, String)>,
    idle: usize,
    /// Questions this chat answered.
    answered: Vec<String>,
    /// Every text shown, including each edit while a reply streamed.
    history: Vec<String>,
}

impl Seen {
    fn apply(&mut self, ev: Value) {
        match ev["type"].as_str().unwrap_or("") {
            "send" => {
                for f in ev["files"].as_array().into_iter().flatten() {
                    self.files.push((f.as_str().unwrap().into(), ev["text"].as_str().unwrap_or("").into()));
                }
                let buttons = ev["buttons"].as_array().into_iter().flatten().flat_map(|row| row.as_array().cloned().unwrap_or_default())
                    .map(|b| (b["id"].as_str().unwrap().to_string(), b["label"].as_str().unwrap().to_string())).collect();
                self.msgs.push(Msg { id: ev["id"].as_str().unwrap().into(), text: ev["text"].as_str().unwrap().into(), buttons, edited: false });
                self.history.push(ev["text"].as_str().unwrap().into());
            }
            "edit" => {
                if let Some(m) = self.msgs.iter_mut().find(|m| m.id == ev["id"]) {
                    m.text = ev["text"].as_str().unwrap().into();
                    m.buttons = ev["buttons"].as_array().into_iter().flatten().flat_map(|row| row.as_array().cloned().unwrap_or_default())
                        .map(|b| (b["id"].as_str().unwrap().to_string(), b["label"].as_str().unwrap().to_string())).collect();
                    m.edited = m.buttons.is_empty();
                }
                self.history.push(ev["text"].as_str().unwrap().into());
            }
            "idle" => self.idle += 1,
            _ => {}
        }
    }
}

pub const TIMEOUT: Duration = Duration::from_secs(30);

/// One thread with August, as a messenger client would see it.
pub struct Chat {
    write: tokio::net::unix::OwnedWriteHalf,
    seen: Arc<Mutex<Seen>>,
    pub thread: String,
}

impl Chat {
    async fn put(&mut self, msg: Value) {
        use tokio::io::AsyncWriteExt;
        self.write.write_all(format!("{msg}\n").as_bytes()).await.unwrap();
    }

    /// Sends a message (or a `/command`).
    pub async fn say(&mut self, text: &str) {
        self.put(json!({"type": "text", "text": text})).await;
    }

    pub async fn press(&mut self, button: &str) {
        let mut seen = self.seen.lock().unwrap();
        if let Some(m) = seen.msgs.iter().find(|m| m.buttons.iter().any(|(id, _)| id == button)).map(|m| m.id.clone()) {
            seen.answered.push(m);
        }
        drop(seen);
        self.put(json!({"type": "press", "button": button})).await;
    }

    pub fn messages(&self) -> Vec<Msg> {
        self.seen.lock().unwrap().msgs.clone()
    }

    /// Every message's current text, in the order they were sent.
    pub fn texts(&self) -> Vec<String> {
        self.messages().into_iter().map(|m| m.text).collect()
    }

    /// Every text shown so far, including each edit of a streaming reply.
    pub fn history(&self) -> Vec<String> {
        self.seen.lock().unwrap().history.clone()
    }

    /// Files sent to the chat: `(path, caption)`.
    pub fn files(&self) -> Vec<(String, String)> {
        self.seen.lock().unwrap().files.clone()
    }

    /// How often August said it has nothing more to say (a turn or command ended).
    pub fn idles(&self) -> usize {
        self.seen.lock().unwrap().idle
    }

    fn transcript(&self) -> String {
        self.texts().iter().map(|t| format!("» {t}")).collect::<Vec<_>>().join("\n")
    }

    /// Waits until `done` holds; panics with the transcript after `TIMEOUT`.
    pub async fn wait_until(&self, what: &str, done: impl Fn(&Chat) -> bool) {
        let start = Instant::now();
        while !done(self) {
            if start.elapsed() > TIMEOUT {
                panic!("timed out waiting for {what}; the chat so far:\n{}", self.transcript());
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// Says `text`, then waits for a message after it that contains `expect`; returns it.
    pub async fn ask(&mut self, text: &str, expect: &str) -> String {
        let n = self.messages().len();
        self.say(text).await;
        self.wait_until(&format!("{expect:?} after {text:?}"), |c| c.texts()[n..].iter().any(|t| t.contains(expect))).await;
        self.texts()[n..].iter().find(|t| t.contains(expect)).unwrap().clone()
    }

    /// Waits for a message whose text contains `needle`.
    pub async fn wait_for(&self, needle: &str) -> Msg {
        self.wait_until(&format!("{needle:?}"), |c| c.texts().iter().any(|t| t.contains(needle))).await;
        self.messages().into_iter().find(|m| m.text.contains(needle)).unwrap()
    }

    /// Waits for a question (a message with buttons) this chat hasn't answered and that
    /// isn't settled.
    pub async fn question(&self) -> Msg {
        let open = |c: &Chat| {
            let seen = c.seen.lock().unwrap();
            seen.msgs.iter().find(|m| !m.buttons.is_empty() && !m.edited && !seen.answered.contains(&m.id)).cloned()
        };
        self.wait_until("a question", |c| open(c).is_some()).await;
        open(self).unwrap()
    }

    /// Allows every approval asked until a message contains `needle`; returns that message.
    pub async fn allow_until(&mut self, needle: &str) -> Msg {
        let start = Instant::now();
        loop {
            if let Some(m) = self.messages().into_iter().find(|m| m.text.contains(needle)) {
                return m;
            }
            let open = {
                let seen = self.seen.lock().unwrap();
                seen.msgs.iter().find(|m| !m.buttons.is_empty() && !m.edited && !seen.answered.contains(&m.id)).cloned()
            };
            if let Some(q) = open {
                self.press(&q.button("Allow")).await;
            }
            if start.elapsed() > TIMEOUT {
                panic!("timed out waiting for {needle:?}; the chat so far:\n{}", self.transcript());
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// Waits until August has gone idle `n` times in this chat.
    pub async fn idle(&self, n: usize) {
        self.wait_until(&format!("idle #{n}"), |c| c.idles() >= n).await;
    }
}

pub fn fixture(name: &str) -> Vec<u8> {
    std::fs::read(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name)).unwrap()
}

/// A terminal (`august` without arguments) connected to a gateway running against the
/// fake LLM; lines go in with `send`, everything it prints is collected. Both processes
/// are killed on drop.
pub struct Terminal {
    child: Child,
    stdin: std::process::ChildStdin,
    out: Arc<Mutex<String>>,
    pub workspace: PathBuf,
    home: PathBuf,
    /// The gateway this terminal started (`None` for another window on it).
    gateway: Option<Gateway>,
}

impl Drop for Terminal {
    fn drop(&mut self) {
        self.child.kill().ok();
        self.child.wait().ok();
    }
}

pub async fn spawn_terminal(fake: &Fake) -> Terminal {
    // Connect only once August listens, so the terminal doesn't start one of its own.
    let gateway = august(fake, Setup::default()).await;
    let (workspace, home) = (gateway.workspace.clone(), gateway.home.clone());
    let mut term = open_terminal(workspace, home);
    term.gateway = Some(gateway);
    term
}

fn open_terminal(workspace: PathBuf, home: PathBuf) -> Terminal {
    let mut child = Command::new(env!("CARGO_BIN_EXE_august"))
        .env_clear()
        .current_dir(&workspace)
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("HOME", home.parent().unwrap())
        .env("AUGUST_HOME", &home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("start the terminal");
    let out: Arc<Mutex<String>> = Arc::default();
    let mut stdout = child.stdout.take().unwrap();
    let sink = out.clone();
    std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        while let Ok(n) = std::io::Read::read(&mut stdout, &mut buf) {
            if n == 0 {
                break;
            }
            sink.lock().unwrap().push_str(&String::from_utf8_lossy(&buf[..n]));
        }
    });
    let stdin = child.stdin.take().unwrap();
    Terminal { child, stdin, out, workspace, home, gateway: None }
}

impl Terminal {
    /// Another terminal window on the same August.
    pub fn another(&self) -> Terminal {
        open_terminal(self.workspace.clone(), self.home.clone())
    }

    pub fn send(&mut self, line: &str) {
        use std::io::Write;
        writeln!(self.stdin, "{line}").unwrap();
    }

    pub fn output(&self) -> String {
        self.out.lock().unwrap().clone()
    }

    /// Waits until the output contains `needle`; panics with the output on timeout.
    pub async fn wait_for(&self, timeout: Duration, needle: &str) {
        let start = Instant::now();
        while !self.output().contains(needle) {
            if start.elapsed() > timeout {
                panic!("timed out waiting for {needle:?}; output so far:\n{}", self.output());
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// Waits until `needle` appears `n` times in the output.
    pub async fn wait_for_count(&self, timeout: Duration, needle: &str, n: usize) {
        let start = Instant::now();
        while self.output().matches(needle).count() < n {
            if start.elapsed() > timeout {
                panic!("timed out waiting for {n}× {needle:?}; output so far:\n{}", self.output());
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// Waits for the process to end (after `/exit`).
    pub async fn exited(&mut self, timeout: Duration) -> bool {
        let start = Instant::now();
        while start.elapsed() < timeout {
            if self.child.try_wait().unwrap().is_some() {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        false
    }
}
