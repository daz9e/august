//! Connects channels to agents: one agent session per chat, its turns, and the operations
//! extensions call; drawing replies is an extension's job (`render`).

mod commands;
pub mod control;
mod login;
mod outbound;
pub(crate) mod ops;
mod turn;
mod turns;
mod waits;

use crate::agent::{self, Agent};
use crate::messengers::bus::Bus;
use crate::config::Root;
use crate::db::Db;
use crate::extensions::{self, Extensions};
pub use crate::extensions::{Io, Link};
use crate::messengers::{Inbound, InboundKind, Messenger, OutMessage, Thread};
use crate::llm::{Block, LlmProvider};
use crate::llm::providers;
use crate::tools::ToolRegistry;
use anyhow::Result;
use async_trait::async_trait;
use serde_json::Value;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex as StdMutex, RwLock, Weak};
use tokio::sync::Mutex;

struct Chat {
    agent: Mutex<Agent>,
    /// Messages sent while a turn runs.
    inbox: Arc<agent::Inbox>,
    /// Held while a message is prepared (files, `message_in`), so messages keep their order.
    intake: Mutex<()>,
}

pub struct Gateway {
    channels: HashMap<String, Arc<dyn Messenger>>,
    chats: Mutex<HashMap<Thread, Arc<Chat>>>,
    /// Waits for what threads send next (answers to questions).
    waits: Arc<waits::Waits>,
    /// Every running turn.
    turns: turns::Turns,
    /// Extensions' listeners (`listen`) until they take their event with `next`.
    listeners: StdMutex<HashMap<u64, tokio::sync::oneshot::Receiver<waits::Reply>>>,
    /// When each thread last sent something (Unix milliseconds).
    activity: StdMutex<HashMap<Thread, i64>>,
    /// Where each thread is, as its messenger last told.
    places: StdMutex<HashMap<Thread, crate::messengers::Place>>,
    provider: RwLock<Arc<dyn LlmProvider>>,
    /// `(provider, model)` in use.
    model: RwLock<(String, String)>,
    pub db: Arc<Db>,
    ext: Arc<Extensions>,
    /// Numbers sub-agents.
    /// The renderer and event stream of each thread's reply in progress, so what else is sent
    /// there lands in order.
    live: StdMutex<HashMap<Thread, (String, tokio::sync::mpsc::UnboundedSender<turn::Live>)>>,
    /// Sign-ins in progress, by session id.
    logins: StdMutex<HashMap<u64, login::Session>>,
    next_login: std::sync::atomic::AtomicU64,
    /// Programs `august` started for extensions (`control.rs`): their tokens' extensions.
    tokens: StdMutex<HashMap<String, String>>,
    /// Notified once August is to stop.
    stopping: tokio::sync::Notify,
}

/// The core's operations as extensions call them.
struct ExtCore(Weak<Gateway>);

#[async_trait]
impl extensions::Core for ExtCore {
    async fn call(&self, ext: &str, op: &str, params: &Value) -> Result<Value> {
        let gw = self.0.upgrade().ok_or_else(|| anyhow::anyhow!("August is shutting down"))?;
        gw.op(ext, op, params).await
    }

    fn changed(&self) {
        if let Some(gw) = self.0.upgrade() {
            tokio::spawn(async move { gw.publish_commands().await });
        }
    }
}

impl Gateway {
    pub fn new(
        channels: Vec<Arc<dyn Messenger>>,
        provider: Arc<dyn LlmProvider>,
        model: (String, String),
        db: Arc<Db>,
        ext: Arc<Extensions>,
    ) -> Arc<Self> {
        let gw = Arc::new(Self {
            channels: channels.into_iter().map(|inner| (inner.id().to_string(), Arc::new(outbound::Hooked { inner, ext: ext.clone() }) as Arc<dyn Messenger>)).collect(),
            chats: Mutex::new(HashMap::new()),
            waits: Arc::default(),
            turns: Default::default(),
            listeners: Default::default(),
            activity: Default::default(),
            places: Default::default(),
            provider: RwLock::new(provider),
            model: RwLock::new(model),
            db,
            ext,
            live: Default::default(),
            logins: Default::default(),
            next_login: Default::default(),
            tokens: Default::default(),
            stopping: Default::default(),
        });
        gw.ext.set_core(Arc::new(ExtCore(Arc::downgrade(&gw))));
        gw
    }

    /// Runs the extensions, the messengers and the control socket and dispatches what comes
    /// in, until `shutdown`.
    pub async fn run(self: Arc<Self>) -> Result<()> {
        let control = control::bind(self.root().home()).await?;
        let bus: Bus<Inbound> = Bus::new(256);
        let mut events = bus.subscribe();
        let mut tasks = tokio::task::JoinSet::new();
        eprintln!("{}", self.ext.start_all().await);
        self.publish_commands().await;
        tasks.spawn(self.clone().serve_control(control));
        for ch in self.channels.values() {
            let (ch, bus) = (ch.clone(), bus.clone());
            tasks.spawn(async move {
                match ch.run(bus).await {
                    Ok(()) => eprintln!("{}: stopped", ch.id()),
                    Err(e) => eprintln!("{}: stopped: {e:#}", ch.id()),
                }
            });
        }
        drop(bus);
        let me = self.clone();
        tasks.spawn(async move {
            while let Some(ev) = events.recv().await {
                let me = me.clone();
                tokio::spawn(async move {
                    if let Err(e) = me.handle(ev).await {
                        eprintln!("gateway: {e:#}");
                    }
                });
            }
        });
        self.stopping.notified().await;
        std::fs::remove_file(control::socket_in(self.root().home())).ok();
        Ok(())
    }

    /// Stops every extension, telling them `reason` (`stop`, `restart`, `signal`), and ends `run`.
    pub async fn shutdown(&self, reason: &str) {
        eprintln!("august: stopping ({reason})");
        self.ext.stop_all(reason).await;
        self.stopping.notify_one();
    }

    fn root(&self) -> &Root {
        self.ext.root()
    }

    fn workspace(&self) -> &std::path::Path {
        self.root().workspace()
    }

    /// Messenger `id`: a built-in one, or one an extension offers.
    fn channel(&self, id: &str) -> Option<Arc<dyn Messenger>> {
        if let Some(m) = self.channels.get(id) {
            return Some(m.clone());
        }
        let (_, d) = self.ext.messengers().into_iter().find(|(_, d)| d.id == id)?;
        let inner = Arc::new(crate::messengers::remote::Remote::new(d, self.ext.clone()));
        Some(Arc::new(outbound::Hooked { inner, ext: self.ext.clone() }))
    }

    /// Every messenger running now.
    fn all_channels(&self) -> Vec<Arc<dyn Messenger>> {
        let mut all: Vec<Arc<dyn Messenger>> = self.channels.values().cloned().collect();
        for (_, d) in self.ext.messengers() {
            if !all.iter().any(|m| m.id() == d.id) {
                all.extend(self.channel(&d.id));
            }
        }
        all
    }

    fn tools(&self) -> ToolRegistry {
        ToolRegistry::default().with_extensions(self.ext.clone())
    }

    async fn chat(&self, id: &Thread) -> Result<Arc<Chat>> {
        let mut chats = self.chats.lock().await;
        if let Some(chat) = chats.get(id) {
            return Ok(chat.clone());
        }
        let agent = Agent::new(
            self.provider.read().unwrap().clone(),
            self.tools(),
            String::new(),
            self.db.clone(),
            &format!("{}:{}", id.messenger, id.id),
        )?;
        let chat = Arc::new(Chat { agent: Mutex::new(agent), inbox: Arc::default(), intake: Mutex::new(()) });
        chats.insert(id.clone(), chat.clone());
        Ok(chat)
    }

    /// Runs the `message_in` hook on a message for `thread`: its data as the hooks left it, or
    /// `None` when one handled it (its reply, if any, is sent).
    async fn message_in(&self, channel: &Arc<dyn Messenger>, thread: &Thread, data: Value) -> Result<Option<Value>> {
        if !self.ext.listens("message_in") {
            return Ok(Some(data));
        }
        let data = self.ext.emit("message_in", data, &extensions::Origin::thread(thread.clone())).await;
        if data["handled"] == true {
            if let Some(reply) = data["reply"].as_str().filter(|r| !r.is_empty()) {
                channel.send(&thread.id, &OutMessage::text(reply)).await?;
            }
            channel.presence(&thread.id, false).await;
            return Ok(None);
        }
        Ok(Some(data))
    }

    /// Hands the chat `message` after the `message_in` hook: `{text, source, deliver}` and
    /// optionally the messenger's `id`, `files` (attachments, for hooks to `download`),
    /// `reply_to` (the message it answers), `addressed` (false: not meant for August, so
    /// nothing runs unless a hook sets it) and `place`. `source` is `user`, or e.g.
    /// `ext:goal`. The hook may change its text, add `images` (`[{path, mime}]`) or its
    /// `deliver`: `steer` joins the running turn or starts one, `followUp` runs as its own
    /// turn after the current one, `nextTurn` waits for the next turn without starting one.
    pub(super) async fn deliver(self: &Arc<Self>, channel: Arc<dyn Messenger>, thread: Thread, mut message: Value) -> Result<()> {
        let state = self.chat(&thread).await?;
        let intake = state.intake.lock().await;
        let deliver = message["deliver"].as_str().unwrap_or("steer").to_string();
        // ponytail: the turn may end while `message_in` runs; then a message marked as
        // steering runs as the next turn. Hold the turn's end on intake if that matters.
        message["steer"] = (deliver == "steer" && state.inbox.steers()).into();
        if message["addressed"].is_null() {
            message["addressed"] = true.into();
        }
        if message["files"].is_null() {
            message["files"] = serde_json::json!([]);
        }
        let Some(data) = self.message_in(&channel, &thread, message).await? else {
            return Ok(());
        };
        // ponytail: what isn't addressed is dropped, not kept as context; stash it for the
        // next turn if group chats need the conversation around a question.
        if data["addressed"] == false {
            return Ok(());
        }
        let text = data["text"].as_str().unwrap_or_default().to_string();
        let images: Vec<Block> = data["images"].as_array().into_iter().flatten().filter_map(|i| {
            Some(Block::Image { media_type: i["mime"].as_str()?.into(), path: i["path"].as_str()?.into() })
        }).collect();
        if text.is_empty() && images.is_empty() {
            return Ok(());
        }
        match data["deliver"].as_str().unwrap_or(&deliver) {
            "nextTurn" => return Ok(state.inbox.stash(&text)),
            // While a turn runs, plain text goes to it instead of waiting for it to end.
            "steer" if images.is_empty() && state.inbox.offer(&text) => return Ok(()),
            _ => {}
        }
        // Busy from here, so the next message joins this turn instead of racing it.
        state.inbox.start();
        drop(intake);
        let chat = thread.id.clone();
        self.turn(channel, thread, &chat, &text, images, None).await?;
        Ok(())
    }

    async fn handle(self: Arc<Self>, ev: Inbound) -> Result<()> {
        let Some(channel) = self.channel(&ev.thread.messenger) else {
            return Ok(());
        };
        let chat = ev.thread.id.clone();
        self.places.lock().unwrap().insert(ev.thread.clone(), ev.place.clone());
        if !matches!(ev.kind, InboundKind::Message { addressed: false, .. }) {
            self.activity.lock().unwrap().insert(ev.thread.clone(), chrono::Utc::now().timestamp_millis());
        }
        // What something waits for (the answer to a question) goes there first.
        if let Some(secret) = self.waits.offer(&ev) {
            if secret && let InboundKind::Message { id, .. } = &ev.kind {
                channel.delete(&chat, id).await.ok();
            }
            return Ok(());
        }
        match ev.kind {
            // A button of a question nobody waits for any more.
            InboundKind::Press { .. } => {}
            InboundKind::Command { name, args } => self.command(&channel, &ev.thread, &name, &args).await?,
            InboundKind::Reaction { message, emoji } => {
                if self.ext.listens("reaction") {
                    let data = serde_json::json!({"message": message, "emoji": emoji});
                    self.ext.emit("reaction", data, &extensions::Origin::thread(ev.thread.clone())).await;
                }
            }
            InboundKind::Edited { id, text } => {
                if self.ext.listens("message_edited") {
                    let data = serde_json::json!({"id": id, "text": text});
                    self.ext.emit("message_edited", data, &extensions::Origin::thread(ev.thread.clone())).await;
                }
            }
            InboundKind::Message { text, files, .. } if text.is_empty() && files.is_empty() => {}
            InboundKind::Message { id: message, text, files, reply_to, addressed } => {
                let message = serde_json::json!({
                    "id": message, "text": text, "files": files, "source": "user", "deliver": "steer",
                    "reply_to": reply_to, "addressed": addressed, "place": ev.place,
                });
                self.deliver(channel, ev.thread, message).await?;
            }
        }
        Ok(())
    }

}

/// What a core is built from.
pub struct Options {
    pub root: Root,
    /// Messengers of the embedder's (extensions add theirs).
    pub messengers: Vec<Arc<dyn Messenger>>,
    /// Where the default extensions' binaries are; `None`: no default extensions.
    pub defaults: Option<PathBuf>,
    /// Extensions reached over links of the embedder's, by name.
    pub linked: Vec<(String, Link)>,
}

/// Runs August in the foreground until `august stop` or a signal.
pub async fn serve() -> Result<()> {
    let root = Root::from_env()?;
    // Default extensions are installed apart from the core: `august.defaults`, else next to `august`.
    let defaults = root.get("august.defaults").ok().and_then(|v| v.as_str().map(PathBuf::from)).unwrap_or_else(extensions::bin_dir);
    let gw = build(Options { root: root.clone(), messengers: Vec::new(), defaults: Some(defaults), linked: Vec::new() })?;
    let model = gw.model.read().unwrap().clone();
    let label = if model.0.is_empty() { "no model provider yet (/login)".to_string() } else { format!("{} · {}", model.0, model.1) };
    println!("august · {label} · workspace {}", root.workspace().display());
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let signals = gw.clone();
    tokio::spawn(async move {
        tokio::select! {
            _ = term.recv() => {}
            _ = tokio::signal::ctrl_c() => {}
        }
        signals.shutdown("signal").await;
    });
    gw.run().await
}

/// A core built from `opts`, ready to `run`: the model chosen in its settings, its database
/// in its home.
pub fn build(opts: Options) -> Result<Arc<Gateway>> {
    let selection = providers::selection(&opts.root)?;
    let model = (selection.provider.clone(), selection.model.clone());
    std::fs::create_dir_all(opts.root.home())?;
    let db = Db::open_at(&opts.root.home().join("august.db"))?;
    let ext = Extensions::new(opts.root, opts.defaults);
    for (name, link) in opts.linked {
        ext.link(&name, link);
    }
    let provider = providers::build(&ext, selection);
    Ok(Gateway::new(opts.messengers, provider, model, db, ext))
}
