//! Connects channels to agents: one agent session per chat, its turns, and the operations
//! extensions call; drawing replies is an extension's job (`render`).

mod commands;
mod login;
mod outbound;
pub(crate) mod ops;
mod turn;
mod turns;
mod waits;

use crate::agent::{self, Agent};
use crate::messengers::bus::Bus;
use crate::db::Db;
use crate::extensions::{self, Extensions};
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
    provider: RwLock<Arc<dyn LlmProvider>>,
    /// `(provider, model)` in use.
    model: RwLock<(String, String)>,
    workspace: PathBuf,
    pub db: Arc<Db>,
    ext: Arc<Extensions>,
    /// Numbers sub-agents.
    /// The renderer and event stream of each thread's reply in progress, so what else is sent
    /// there lands in order.
    live: StdMutex<HashMap<Thread, (String, tokio::sync::mpsc::UnboundedSender<turn::Live>)>>,
    /// Sign-ins in progress, by session id.
    logins: StdMutex<HashMap<u64, login::Session>>,
    next_login: std::sync::atomic::AtomicU64,
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
        workspace: PathBuf,
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
            provider: RwLock::new(provider),
            model: RwLock::new(model),
            workspace,
            db,
            ext,
            live: Default::default(),
            logins: Default::default(),
            next_login: Default::default(),
        });
        gw.ext.set_core(Arc::new(ExtCore(Arc::downgrade(&gw))));
        gw
    }

    /// Runs every channel and dispatches their events until all channels stop.
    pub async fn run(self: Arc<Self>) -> Result<()> {
        let bus: Bus<Inbound> = Bus::new(256);
        let mut events = bus.subscribe();
        let mut tasks = tokio::task::JoinSet::new();
        eprintln!("{}", self.ext.start_all().await);
        self.publish_commands().await;
        for ch in self.channels.values() {
            let (ch, bus) = (ch.clone(), bus.clone());
            tasks.spawn(async move {
                let r = ch.run(bus).await;
                (ch.id().to_string(), r)
            });
        }
        drop(bus);

        let me = self.clone();
        let dispatcher = tokio::spawn(async move {
            while let Some(ev) = events.recv().await {
                let me = me.clone();
                tokio::spawn(async move {
                    if let Err(e) = me.handle(ev).await {
                        eprintln!("gateway: {e:#}");
                    }
                });
            }
        });

        while let Some(done) = tasks.join_next().await {
            let (id, r) = done?;
            match r {
                Ok(()) => eprintln!("{id}: stopped"),
                Err(e) => eprintln!("{id}: stopped: {e:#}"),
            }
        }
        dispatcher.await.ok();
        Ok(())
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

    /// Hands the chat a message from `source` (`user`, or e.g. `ext:goal`) after the
    /// `message_in` hook, which may change its text, add `images` (`[{path, mime}]`) or its
    /// `deliver`: `steer` joins the running turn or starts one, `followUp` runs as its own
    /// turn after the current one, `nextTurn` waits for the next turn without starting one.
    /// `files` are the messenger's attachments, for hooks to `download`.
    pub(super) async fn deliver(self: &Arc<Self>, channel: Arc<dyn Messenger>, thread: Thread, message: Option<String>, text: String, files: Value, source: &str, deliver: &str) -> Result<()> {
        let state = self.chat(&thread).await?;
        let intake = state.intake.lock().await;
        // ponytail: the turn may end while `message_in` runs; then a message marked as
        // steering runs as the next turn. Hold the turn's end on intake if that matters.
        let steer = deliver == "steer" && state.inbox.steers();
        let data = serde_json::json!({"id": message, "text": text, "files": files, "source": source, "deliver": deliver, "steer": steer});
        let Some(data) = self.message_in(&channel, &thread, data).await? else {
            return Ok(());
        };
        let text = data["text"].as_str().unwrap_or_default().to_string();
        let images: Vec<Block> = data["images"].as_array().into_iter().flatten().filter_map(|i| {
            Some(Block::Image { media_type: i["mime"].as_str()?.into(), path: i["path"].as_str()?.into() })
        }).collect();
        if text.is_empty() && images.is_empty() {
            return Ok(());
        }
        match data["deliver"].as_str().unwrap_or(deliver) {
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
        self.activity.lock().unwrap().insert(ev.thread.clone(), chrono::Utc::now().timestamp_millis());
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
            InboundKind::Message { text, files, .. } if text.is_empty() && files.is_empty() => {}
            InboundKind::Message { id: message, text, files } => {
                let files = serde_json::to_value(files)?;
                self.deliver(channel, ev.thread, Some(message), text, files, "user", "steer").await?;
            }
        }
        Ok(())
    }

}

/// Runs the agent behind the built-in messengers and those of extensions (foreground).
pub async fn serve() -> Result<()> {
    start(crate::messengers::builtin()).await
}

/// Runs the agent behind `chans` until they all stop.
pub async fn start(chans: Vec<Arc<dyn Messenger>>) -> Result<()> {
    let workspace = crate::config::workspace()?;
    let selection = providers::selection()?;
    let model = (selection.provider.clone(), selection.model.clone());
    let label = if model.0.is_empty() { "no model provider yet (/login)".to_string() } else { format!("{} · {}", model.0, model.1) };
    let provider = providers::build(selection);
    println!(
        "august serving {} (and the extensions' messengers) · {label} · workspace {}",
        chans.iter().map(|c| c.id().to_string()).collect::<Vec<_>>().join(", "),
        workspace.display()
    );
    let ext = Extensions::new(extensions::dir());
    crate::llm::remote::use_extensions(&ext);
    Gateway::new(chans, provider, model, workspace, Db::open()?, ext).run().await
}
