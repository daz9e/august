//! The core in-process, for core tests: built as a library on a temp home and workspace,
//! with a fake messenger (`Messenger`), a fake model provider and fake extensions, all
//! written against the core's own contracts. The model and the extensions are written with
//! the SDK and linked to the core in memory (`Options::linked`), so they speak the extension
//! protocol as any extension does. No binary, no sockets, no default extensions.

#![allow(dead_code)]

use august::Root;
use august::gateway::{self, Gateway, Link, Options};
use august::messengers::bus::Bus;
use august::messengers::{Attachment, Button, Capabilities, CommandSpec, Description, Inbound, InboundKind, Messenger, OutMessage, Thread, User};
use august_ext::August;
use august_ext::llm::error::{ErrorKind, ProviderError};
use august_ext::llm::{Block, Completion, Message, ModelInfo, Request, Role, StopReason, Usage};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// The fake messenger's id.
pub const MESSENGER: &str = "test";
/// The fake model provider and its model.
pub const PROVIDER: &str = "fake";
pub const MODEL: &str = "fake-model";
/// Every permission there is, for the harness's own extension (`Core::call`).
const ALL: &[&str] = &["messaging", "turns", "tools", "llm", "models", "sessions", "config", "admin", "user"];
const WAIT: Duration = Duration::from_secs(10);

// ---- the model ---------------------------------------------------------------

/// What the fake model answers one request with.
pub struct Reply {
    result: Result<Completion, (ErrorKind, String)>,
    /// Pieces streamed before the completion returns.
    stream: Vec<String>,
    delay: Duration,
    /// Answers only once this lets it (a permit each).
    gate: Option<Arc<tokio::sync::Semaphore>>,
}

fn completion(content: Vec<Block>, stop: StopReason) -> Reply {
    let message = Message { role: Role::Assistant, content };
    Reply { result: Ok(Completion { message, stop_reason: stop, usage: Usage { input_tokens: 10, output_tokens: 5, ..Default::default() } }), stream: Vec::new(), delay: Duration::ZERO, gate: None }
}

/// A text answer.
pub fn text(text: &str) -> Reply {
    completion(vec![Block::Text(text.into())], StopReason::EndTurn)
}

/// A call of tool `name`, id `call_1`.
pub fn tool(name: &str, input: Value) -> Reply {
    completion(vec![Block::ToolUse { id: "call_1".into(), name: name.into(), input }], StopReason::ToolUse)
}

/// A failed call of kind `kind`.
pub fn fail(kind: ErrorKind, message: &str) -> Reply {
    Reply { result: Err((kind, message.into())), stream: Vec::new(), delay: Duration::ZERO, gate: None }
}

impl Reply {
    /// Answers only after `delay` (a slow model).
    pub fn after(mut self, delay: Duration) -> Self {
        self.delay = delay;
        self
    }

    /// Answers only when `gate` gives it a permit.
    pub fn gated(mut self, gate: &Arc<tokio::sync::Semaphore>) -> Self {
        self.gate = Some(gate.clone());
        self
    }

    /// Streams `pieces` before answering.
    pub fn streamed(mut self, pieces: &[&str]) -> Self {
        self.stream = pieces.iter().map(|p| p.to_string()).collect();
        self
    }

    /// With these token counts.
    pub fn usage(mut self, input: u64, output: u64, cache_read: u64) -> Self {
        if let Ok(c) = &mut self.result {
            c.usage = Usage { input_tokens: input, output_tokens: output, cache_read_tokens: cache_read, cache_write_tokens: 0 };
        }
        self
    }
}

type Script = Arc<dyn Fn(&Request) -> Reply + Send + Sync>;

/// The text of the last user message of a request (its text blocks).
pub fn last_user_text(req: &Request) -> String {
    let m = req.messages.iter().rev().find(|m| m.role == Role::User && m.content.iter().any(|b| matches!(b, Block::Text(_))));
    m.map(Message::text).unwrap_or_default()
}

/// The output of the tool result the request ends with, if it does.
pub fn tool_result(req: &Request) -> Option<String> {
    req.messages.last()?.content.iter().find_map(|b| match b {
        Block::ToolResult { content, .. } => Some(content.clone()),
        _ => None,
    })
}

/// Every message's text of a request.
pub fn all_text(req: &Request) -> String {
    req.messages.iter().map(Message::text).collect::<Vec<_>>().join("\n")
}

/// A closed gate (`Reply::gated`); `add_permits` opens it.
pub fn gate() -> Arc<tokio::sync::Semaphore> {
    Arc::new(tokio::sync::Semaphore::new(0))
}

/// Names of the tools a request offers.
pub fn tool_names(req: &Request) -> Vec<String> {
    req.tools.iter().map(|t| t.name.clone()).collect()
}

// ---- the messenger -----------------------------------------------------------

/// A message the core sent, as it reads now.
#[derive(Debug, Clone)]
pub struct Msg {
    pub thread: String,
    pub id: String,
    pub text: String,
    pub buttons: Vec<Button>,
    pub files: Vec<PathBuf>,
    pub reply_to: Option<String>,
    /// Edited after it was sent.
    pub edited: bool,
    pub deleted: bool,
}

impl Msg {
    /// The id of the button labelled `label`.
    pub fn button(&self, label: &str) -> String {
        self.buttons.iter().find(|b| b.label.contains(label)).unwrap_or_else(|| panic!("no button {label:?} in {self:?}")).id.clone()
    }
}

#[derive(Default)]
struct Seen {
    msgs: Vec<Msg>,
    /// `(thread, text)` of every send and edit, in order.
    history: Vec<(String, String)>,
    /// How often the core said it is done in each thread (`presence` off).
    idle: HashMap<String, usize>,
    /// `(thread, message, emoji)` the core reacted with.
    reactions: Vec<(String, String, String)>,
    /// Command menus the core published.
    menus: Vec<Vec<String>>,
    /// Buttons pressed.
    pressed: Vec<String>,
    /// Ids of every message the core deleted (the user's included).
    deleted: Vec<String>,
    /// `(action, thread, args)` of actions run and threads opened.
    actions: Vec<(String, String, Value)>,
}

/// A messenger whose threads tests speak in; it records everything the core does to it.
pub struct FakeMessenger {
    bus: Mutex<Option<Bus<Inbound>>>,
    seen: Mutex<Seen>,
    next: AtomicU64,
    /// Contents of attachments by id, for `download`.
    files: Mutex<HashMap<String, Vec<u8>>>,
    caps: Capabilities,
}

impl FakeMessenger {
    fn new(caps: Capabilities) -> Self {
        Self { bus: Mutex::default(), seen: Mutex::default(), next: AtomicU64::new(1), files: Mutex::default(), caps }
    }

    /// Everything it can do.
    pub fn full() -> Capabilities {
        Capabilities {
            markdown: true,
            max_len: 4000,
            buttons: 8,
            edit: true,
            edit_interval_ms: 0,
            files_in: true,
            files_out: true,
            images: true,
            audio_in: true,
            commands: true,
            presence: true,
            delete: true,
            reactions: true,
            reply: true,
            threads: true,
            open_thread: true,
        }
    }

    fn publish(&self, thread: &str, kind: InboundKind) {
        self.publish_at(thread, Default::default(), kind);
    }

    fn publish_at(&self, thread: &str, place: august::messengers::Place, kind: InboundKind) {
        let bus = self.bus.lock().unwrap().clone().expect("the core runs");
        let user = User { id: "owner".into(), name: "Owner".into() };
        bus.publish(Inbound { thread: Thread::new(MESSENGER, thread), place, user, kind });
    }

    fn put(&self, thread: &str, id: &str, m: &OutMessage, edit: bool) {
        let mut seen = self.seen.lock().unwrap();
        seen.history.push((thread.into(), m.text.clone()));
        let buttons: Vec<Button> = m.all_buttons().cloned().collect();
        if edit {
            if let Some(old) = seen.msgs.iter_mut().find(|x| x.id == id) {
                old.text = m.text.clone();
                old.buttons = buttons;
                old.edited = true;
            }
            return;
        }
        let msg = Msg { thread: thread.into(), id: id.into(), text: m.text.clone(), buttons, files: m.files.clone(), reply_to: m.reply_to.clone(), edited: false, deleted: false };
        seen.msgs.push(msg);
    }

    /// Command menus published so far.
    pub fn menus(&self) -> Vec<Vec<String>> {
        self.seen.lock().unwrap().menus.clone()
    }

    /// Reactions the core set: `(thread, message, emoji)`.
    pub fn reactions(&self) -> Vec<(String, String, String)> {
        self.seen.lock().unwrap().reactions.clone()
    }

    /// `(action, thread, args)` the core ran (`open_thread` included).
    pub fn actions(&self) -> Vec<(String, String, Value)> {
        self.seen.lock().unwrap().actions.clone()
    }

    /// Ids of the messages the core deleted.
    pub fn deleted(&self) -> Vec<String> {
        self.seen.lock().unwrap().deleted.clone()
    }

    /// Every message sent to any thread.
    pub fn all(&self) -> Vec<Msg> {
        self.seen.lock().unwrap().msgs.clone()
    }
}

#[async_trait::async_trait]
impl Messenger for FakeMessenger {
    fn id(&self) -> &str {
        MESSENGER
    }

    fn describe(&self) -> Description {
        let pin = august::messengers::Action {
            name: "pin".into(),
            description: "Pin a message".into(),
            input_schema: json!({"type": "object", "required": ["message"], "properties": {"message": {"type": "string"}}}),
        };
        Description { id: MESSENGER.into(), name: "Test".into(), capabilities: self.caps.clone(), notes: "A messenger for tests.".into(), actions: vec![pin] }
    }

    async fn run(&self, bus: Bus<Inbound>) -> anyhow::Result<()> {
        *self.bus.lock().unwrap() = Some(bus);
        std::future::pending().await
    }

    async fn send(&self, thread: &str, message: &OutMessage) -> anyhow::Result<String> {
        let id = format!("m{}", self.next.fetch_add(1, Ordering::SeqCst));
        self.put(thread, &id, message, false);
        Ok(id)
    }

    async fn edit(&self, thread: &str, id: &str, message: &OutMessage) -> anyhow::Result<()> {
        self.put(thread, id, message, true);
        Ok(())
    }

    async fn presence(&self, thread: &str, busy: bool) {
        if !busy {
            *self.seen.lock().unwrap().idle.entry(thread.into()).or_default() += 1;
        }
    }

    async fn delete(&self, _thread: &str, id: &str) -> anyhow::Result<()> {
        let mut seen = self.seen.lock().unwrap();
        seen.deleted.push(id.into());
        if let Some(m) = seen.msgs.iter_mut().find(|m| m.id == id) {
            m.deleted = true;
        }
        Ok(())
    }

    async fn react(&self, thread: &str, id: &str, emoji: &str) -> anyhow::Result<()> {
        self.seen.lock().unwrap().reactions.push((thread.into(), id.into(), emoji.into()));
        Ok(())
    }

    async fn set_commands(&self, commands: &[CommandSpec]) -> anyhow::Result<()> {
        self.seen.lock().unwrap().menus.push(commands.iter().map(|c| c.name.clone()).collect());
        Ok(())
    }

    async fn open_thread(&self, parent: &str, title: &str) -> anyhow::Result<String> {
        self.seen.lock().unwrap().actions.push(("open_thread".into(), parent.into(), json!({"title": title})));
        Ok(format!("{parent}/topic"))
    }

    async fn action(&self, thread: &str, name: &str, args: Value) -> anyhow::Result<Value> {
        self.seen.lock().unwrap().actions.push((name.into(), thread.into(), args));
        Ok(json!("pinned"))
    }

    async fn download(&self, file: &Attachment) -> anyhow::Result<Vec<u8>> {
        self.files.lock().unwrap().get(&file.id).cloned().ok_or_else(|| anyhow::anyhow!("no file {}", file.id))
    }
}

// ---- building the core -------------------------------------------------------

type Setup = Arc<dyn Fn(&August) + Send + Sync>;

/// How a core starts in a test.
pub struct Builder {
    model: Script,
    exts: Vec<(String, Setup)>,
    env: Vec<(String, String)>,
    home: Vec<(String, String)>,
    seed: Vec<(String, Vec<u8>)>,
    caps: Capabilities,
    /// Default extensions: `(name, sh script)`.
    defaults: Vec<(String, String)>,
}

/// A core being set up: `core().model(..).ext(..).start().await`.
pub fn core() -> Builder {
    Builder {
        model: Arc::new(|_| text("ok")),
        exts: Vec::new(),
        env: Vec::new(),
        home: Vec::new(),
        seed: Vec::new(),
        caps: FakeMessenger::full(),
        defaults: Vec::new(),
    }
}

impl Builder {
    /// The model answers each request with `script`.
    pub fn model(mut self, script: impl Fn(&Request) -> Reply + Send + Sync + 'static) -> Self {
        self.model = Arc::new(script);
        self
    }

    /// A fake extension `name`, set up by `setup` at each start (it may register anything).
    pub fn ext(mut self, name: &str, setup: impl Fn(&August) + Send + Sync + 'static) -> Self {
        self.exts.push((name.into(), Arc::new(setup)));
        self
    }

    /// An environment variable the core reads.
    pub fn env(mut self, key: &str, value: &str) -> Self {
        self.env.push((key.into(), value.into()));
        self
    }

    /// A file in the home (relative path).
    pub fn home(mut self, path: &str, text: &str) -> Self {
        self.home.push((path.into(), text.into()));
        self
    }

    /// A file in the workspace.
    pub fn seed(mut self, name: &str, bytes: &[u8]) -> Self {
        self.seed.push((name.into(), bytes.to_vec()));
        self
    }

    /// What the fake messenger can do.
    pub fn capabilities(mut self, caps: Capabilities) -> Self {
        self.caps = caps;
        self
    }

    pub async fn start(self) -> Core {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let workspace = dir.path().join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        let workspace = workspace.canonicalize().unwrap();
        for (path, text) in &self.home {
            let path = home.join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }
        for (name, bytes) in &self.seed {
            std::fs::write(workspace.join(name), bytes).unwrap();
        }
        // The fake model is the one chosen, unless the test picks another.
        let app = home.join("config/august.json");
        if !app.exists() {
            std::fs::create_dir_all(app.parent().unwrap()).unwrap();
            std::fs::write(&app, json!({"provider": PROVIDER, "model": MODEL}).to_string()).unwrap();
        }
        let root = Root::new(home.clone(), workspace.clone(), self.env.into_iter().collect());
        let defaults = (!self.defaults.is_empty()).then(|| {
            let bin = dir.path().join("bin");
            std::fs::create_dir_all(&bin).unwrap();
            for (name, script) in &self.defaults {
                let exe = bin.join(format!("august-ext-{name}"));
                std::fs::write(&exe, format!("#!/bin/sh\n{script}\n")).unwrap();
                std::fs::set_permissions(&exe, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
            }
            bin
        });
        let requests: Arc<Mutex<Vec<Request>>> = Arc::default();
        let probe: Arc<Mutex<Option<August>>> = Arc::default();
        let mut exts = vec![("model".to_string(), model_setup(self.model, requests.clone())), ("probe".to_string(), probe_setup(probe.clone()))];
        exts.extend(self.exts);
        let parts = Parts { root, exts, caps: self.caps, defaults };
        let pumps: Pumps = Arc::default();
        let (gw, messenger, run) = parts.launch(&pumps);
        let core = Core { gw, home, workspace, messenger, requests, probe, pumps, parts, _run: run, _dir: dir };
        core.wait_until("the core to start", |c| c.messenger.bus.lock().unwrap().is_some()).await;
        core
    }
}

/// What a core is built from, to build it again (`Core::restart`).
struct Parts {
    root: Root,
    exts: Vec<(String, Setup)>,
    caps: Capabilities,
    defaults: Option<PathBuf>,
}

impl Parts {
    fn launch(&self, pumps: &Pumps) -> (Arc<Gateway>, Arc<FakeMessenger>, Aborts) {
        let messenger = Arc::new(FakeMessenger::new(self.caps.clone()));
        let messengers: Vec<Arc<dyn Messenger>> = vec![messenger.clone()];
        let linked = self.exts.iter().map(|(name, setup)| (name.clone(), link(&self.root, name, setup.clone(), pumps.clone()))).collect();
        let gw = gateway::build(Options { root: self.root.clone(), messengers, defaults: self.defaults.clone(), linked }).unwrap();
        let run = tokio::spawn(gw.clone().run());
        (gw, messenger, Aborts(run))
    }
}

fn model_setup(script: Script, requests: Arc<Mutex<Vec<Request>>>) -> Setup {
    Arc::new(move |a: &August| {
        let (script, requests) = (script.clone(), requests.clone());
        let models = |_| async { Ok(vec![ModelInfo { id: MODEL.into(), context_window: Some(100_000) }]) };
        a.register_provider(PROVIDER, "Fake", Some(MODEL), models, move |req, stream| {
            requests.lock().unwrap().push(req.clone());
            let reply = script(&req);
            async move {
                // Like real providers, it streams what it says (all at once unless told).
                let whole = reply.result.as_ref().map(|c| vec![c.message.text()]).unwrap_or_default();
                for piece in if reply.stream.is_empty() { &whole } else { &reply.stream } {
                    stream.text(piece);
                }
                tokio::time::sleep(reply.delay).await;
                if let Some(gate) = &reply.gate {
                    gate.acquire().await.unwrap().forget();
                }
                reply.result.map_err(|(kind, message)| ProviderError { kind, message }.into())
            }
        });
    })
}

fn probe_setup(slot: Arc<Mutex<Option<August>>>) -> Setup {
    Arc::new(move |a: &August| {
        a.needs(ALL);
        *slot.lock().unwrap() = Some(a.clone());
    })
}

/// The pump of each linked extension's latest link, to cut it (`Core::crash`).
type Pumps = Arc<Mutex<HashMap<String, tokio::task::AbortHandle>>>;

/// Each start runs `setup` on a fresh SDK `August` and links it to the core through an
/// in-memory pipe.
fn link(root: &Root, name: &str, setup: Setup, pumps: Pumps) -> Link {
    let dir = root.home().join("linked").join(name);
    let env: HashMap<String, String> = [
        ("AUGUST_HOME", root.home().display().to_string()),
        ("AUGUST_WORKSPACE", root.workspace().display().to_string()),
        ("AUGUST_EXTENSION_DIR", dir.display().to_string()),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v))
    .collect();
    let name = name.to_string();
    Arc::new(move || {
        std::fs::create_dir_all(&dir).ok();
        let (ext_end, mut near) = tokio::io::duplex(1 << 20);
        let (core_end, mut far) = tokio::io::duplex(1 << 20);
        let (read, write) = tokio::io::split(ext_end);
        let august = August::over(env.clone(), read, write);
        setup(&august);
        tokio::spawn(august.run());
        let pump = tokio::spawn(async move {
            tokio::io::copy_bidirectional(&mut near, &mut far).await.ok();
        });
        pumps.lock().unwrap().insert(name.clone(), pump.abort_handle());
        let (read, write) = tokio::io::split(core_end);
        (Box::new(read), Box::new(write))
    })
}

struct Aborts(tokio::task::JoinHandle<anyhow::Result<()>>);

impl Drop for Aborts {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// A running core and its fakes.
pub struct Core {
    pub gw: Arc<Gateway>,
    pub home: PathBuf,
    pub workspace: PathBuf,
    pub messenger: Arc<FakeMessenger>,
    requests: Arc<Mutex<Vec<Request>>>,
    probe: Arc<Mutex<Option<August>>>,
    pumps: Pumps,
    parts: Parts,
    _run: Aborts,
    _dir: tempfile::TempDir,
}

impl Core {
    /// Stops the core and builds it again on the same home and workspace (a restart of
    /// August); chats made before it talk to the old one.
    pub async fn restart(&mut self) {
        // The old one lets go of its control socket first, or the new one won't start.
        self._run.0.abort();
        let socket = august::gateway::control::socket_in(&self.home);
        self.wait_until("the core to stop", |_| std::os::unix::net::UnixStream::connect(&socket).is_err()).await;
        let (gw, messenger, run) = self.parts.launch(&self.pumps);
        (self.gw, self.messenger, self._run) = (gw, messenger, run);
        self.wait_until("the core to start again", |c| c.messenger.bus.lock().unwrap().is_some()).await;
    }

    /// A thread of the fake messenger.
    pub fn chat(&self, id: &str) -> Chat {
        Chat { m: self.messenger.clone(), thread: id.into(), next: Arc::new(AtomicU64::new(1)) }
    }

    /// Calls operation `op` of the core as an extension with every permission would.
    pub async fn call(&self, op: &str, params: Value) -> anyhow::Result<Value> {
        let probe = self.probe.lock().unwrap().clone().expect("the probe runs");
        probe.call(op, params).await
    }

    /// `call` in thread `thread` of the fake messenger.
    pub async fn call_in(&self, thread: &str, op: &str, mut params: Value) -> anyhow::Result<Value> {
        params["thread"] = json!({"messenger": MESSENGER, "id": thread});
        self.call(op, params).await
    }

    /// Every request the model got, in order.
    pub fn requests(&self) -> Vec<Request> {
        self.requests.lock().unwrap().clone()
    }

    /// Cuts the link of extension `name`, as if its process died.
    pub fn crash(&self, name: &str) {
        if let Some(pump) = self.pumps.lock().unwrap().remove(name) {
            pump.abort();
        }
    }

    /// The extension's entry in `extensions`.
    pub async fn extension(&self, name: &str) -> Value {
        let all = self.call("extensions", json!({})).await.unwrap();
        all.as_array().unwrap().iter().find(|e| e["name"] == name).cloned().unwrap_or(Value::Null)
    }

    pub fn path(&self, rel: &str) -> PathBuf {
        self.home.join(rel)
    }

    /// Waits until `done` holds; panics after a while.
    pub async fn wait_until(&self, what: &str, done: impl Fn(&Core) -> bool) {
        wait(what, || done(self), String::new).await;
    }
}

async fn wait(what: &str, done: impl Fn() -> bool, context: impl Fn() -> String) {
    let start = tokio::time::Instant::now();
    while !done() {
        if start.elapsed() > WAIT {
            panic!("timed out waiting for {what}\n{}", context());
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// One thread of the fake messenger, as its user sees it.
#[derive(Clone)]
pub struct Chat {
    m: Arc<FakeMessenger>,
    pub thread: String,
    next: Arc<AtomicU64>,
}

impl Chat {
    pub fn thread(&self) -> Value {
        json!({"messenger": MESSENGER, "id": self.thread})
    }

    /// Sends `text`: a `/command`, or a message.
    pub fn say(&self, text: &str) -> String {
        if let Some(rest) = text.strip_prefix('/') {
            let (name, args) = rest.split_once(' ').unwrap_or((rest, ""));
            self.m.publish(&self.thread, InboundKind::Command { name: name.into(), args: args.into() });
            return String::new();
        }
        let id = format!("in{}", self.next.fetch_add(1, Ordering::SeqCst));
        self.send(InboundKind::Message { id: id.clone(), text: text.into(), files: Vec::new(), reply_to: None, addressed: true });
        id
    }

    /// Sends a message with attachments; `files` are `(attachment, contents)`.
    pub fn say_with(&self, text: &str, files: Vec<(Attachment, Vec<u8>)>) {
        let id = format!("in{}", self.next.fetch_add(1, Ordering::SeqCst));
        let mut list = Vec::new();
        for (a, bytes) in files {
            self.m.files.lock().unwrap().insert(a.id.clone(), bytes);
            list.push(a);
        }
        self.send(InboundKind::Message { id, text: text.into(), files: list, reply_to: None, addressed: true });
    }

    /// Hands the core anything that can come in.
    pub fn send(&self, kind: InboundKind) {
        self.m.publish(&self.thread, kind);
    }

    /// `send`, from a thread at `place`.
    pub fn send_at(&self, place: august::messengers::Place, kind: InboundKind) {
        self.m.publish_at(&self.thread, place, kind);
    }

    pub fn press(&self, button: &str) {
        self.m.seen.lock().unwrap().pressed.push(button.into());
        self.m.publish(&self.thread, InboundKind::Press { button: button.into() });
    }

    pub fn messages(&self) -> Vec<Msg> {
        self.m.seen.lock().unwrap().msgs.iter().filter(|m| m.thread == self.thread).cloned().collect()
    }

    /// Every message's current text, in the order they were sent.
    pub fn texts(&self) -> Vec<String> {
        self.messages().into_iter().map(|m| m.text).collect()
    }

    /// Every text shown, edits included.
    pub fn history(&self) -> Vec<String> {
        self.m.seen.lock().unwrap().history.iter().filter(|(t, _)| *t == self.thread).map(|(_, x)| x.clone()).collect()
    }

    /// How often the core said it is done here.
    pub fn idles(&self) -> usize {
        self.m.seen.lock().unwrap().idle.get(&self.thread).copied().unwrap_or(0)
    }

    fn transcript(&self) -> String {
        self.texts().iter().map(|t| format!("» {t}")).collect::<Vec<_>>().join("\n")
    }

    pub async fn wait_until(&self, what: &str, done: impl Fn(&Chat) -> bool) {
        wait(&format!("{what}; the chat so far:"), || done(self), || self.transcript()).await;
    }

    /// Waits for a message containing `needle`.
    pub async fn wait_for(&self, needle: &str) -> Msg {
        self.wait_until(&format!("{needle:?}"), |c| c.texts().iter().any(|t| t.contains(needle))).await;
        self.messages().into_iter().find(|m| m.text.contains(needle)).unwrap()
    }

    /// Says `text`, then waits for a message after it containing `expect`; returns it.
    pub async fn ask(&self, text: &str, expect: &str) -> String {
        let n = self.messages().len();
        self.say(text);
        self.wait_until(&format!("{expect:?} after {text:?}"), |c| c.texts().iter().skip(n).any(|t| t.contains(expect))).await;
        self.texts().into_iter().skip(n).find(|t| t.contains(expect)).unwrap()
    }

    /// Waits for a message with buttons none of which was pressed.
    pub async fn question(&self) -> Msg {
        let open = |c: &Chat| {
            let pressed = c.m.seen.lock().unwrap().pressed.clone();
            c.messages().into_iter().find(|m| !m.buttons.is_empty() && !m.buttons.iter().any(|b| pressed.contains(&b.id)))
        };
        self.wait_until("a question", |c| open(c).is_some()).await;
        open(self).unwrap()
    }

    /// Waits until the core went idle here `n` times.
    pub async fn idle(&self, n: usize) {
        self.wait_until(&format!("idle #{n}"), |c| c.idles() >= n).await;
    }
}

/// A fixture from `tests/fixtures`.
pub fn fixture(name: &str) -> Vec<u8> {
    std::fs::read(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name)).unwrap()
}

/// Stops after `ms`, for tests that check nothing more happens.
pub async fn settle(ms: u64) {
    tokio::time::sleep(Duration::from_millis(ms)).await;
}

/// A fake extension's tools for the workspace: `read {path}` and `list`.
pub fn files(a: &August) {
    let ws = a.workspace().clone();
    a.register_tool("read", "Read a file", json!({"type": "object", "properties": {"path": {"type": "string"}}}), move |input, _| {
        let path = ws.join(input["path"].as_str().unwrap_or_default());
        async move { Ok(std::fs::read_to_string(path)?) }
    });
    let ws = a.workspace().clone();
    a.register_tool("list", "List files", json!({"type": "object"}), move |_, _| {
        let ws = ws.clone();
        async move { Ok(std::fs::read_dir(ws)?.filter_map(|e| Some(e.ok()?.file_name().to_string_lossy().to_string())).collect::<Vec<_>>().join("\n")) }
    });
}

/// Files for an extension in any language: `extensions/<name>/extension.json` running `sh -c
/// script` in its folder.
pub fn sh_extension(name: &str, script: &str) -> (String, String) {
    (format!("extensions/{name}/extension.json"), json!({"command": ["sh", "-c", script]}).to_string())
}

/// A shell script that registers `manifest` (the `ready` message's params, protocol 2) and
/// then idles, answering nothing.
pub fn ready_script(mut manifest: Value) -> String {
    manifest["protocol"] = json!(2);
    let ready = json!({"method": "ready", "params": manifest}).to_string();
    format!("echo '{ready}'; exec cat >/dev/null")
}

impl Builder {
    /// An extension in any language (see `sh_extension`).
    pub fn sh(self, name: &str, script: &str) -> Self {
        let (path, text) = sh_extension(name, script);
        self.home(&path, &text)
    }

    /// A default extension `name` (a binary `august-ext-<name>`) running `sh` script `script`.
    pub fn default_ext(mut self, name: &str, script: &str) -> Self {
        self.defaults.push((name.into(), script.into()));
        self
    }
}
