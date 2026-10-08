//! Connects channels to agents: one agent session per chat, slash commands,
//! streamed replies rendered as live-edited messages.

mod commands;
mod media;
mod outbound;
pub(crate) mod ops;
mod render;
mod subagents;
mod turn;
mod turns;
mod waits;

use crate::agent::{self, Agent};
use crate::messengers::bus::Bus;
use crate::db::Db;
use crate::extensions::{self, Extensions};
use crate::messengers::{Inbound, InboundKind, Messenger, OutMessage, Thread};
use crate::llm::LlmProvider;
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
    subagents: std::sync::atomic::AtomicU64,
    /// The stream of each thread's reply in progress, so what else is sent there lands in order.
    live: StdMutex<HashMap<Thread, tokio::sync::mpsc::UnboundedSender<render::Ui>>>,
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
            channels: channels
                .into_iter()
                .map(|inner| (inner.id().to_string(), Arc::new(outbound::Hooked { inner, ext: ext.clone() }) as Arc<dyn Messenger>))
                .collect(),
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
            subagents: Default::default(),
            live: Default::default(),
        });
        gw.ext.set_core(Arc::new(ExtCore(Arc::downgrade(&gw))));
        gw
    }

    /// Runs every channel and dispatches their events until all channels stop.
    pub async fn run(self: Arc<Self>) -> Result<()> {
        let bus: Bus<Inbound> = Bus::new(256);
        let mut events = bus.subscribe();
        let mut tasks = tokio::task::JoinSet::new();
        eprintln!("{}", self.ext.reload().await);
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

    fn tools(&self) -> ToolRegistry {
        ToolRegistry::with_defaults().with_extensions(self.ext.clone())
    }

    async fn chat(&self, id: &Thread) -> Result<Arc<Chat>> {
        let mut chats = self.chats.lock().await;
        if let Some(chat) = chats.get(id) {
            return Ok(chat.clone());
        }
        let agent = Agent::new(
            self.provider.read().unwrap().clone(),
            self.tools(),
            agent::system_prompt(&self.workspace, &self.channels.get(&id.messenger).map(|m| crate::messengers::surface(&m.describe())).unwrap_or_default()),
            self.db.clone(),
            &format!("{}:{}", id.messenger, id.id),
        )?;
        let chat = Arc::new(Chat { agent: Mutex::new(agent), inbox: Arc::default(), intake: Mutex::new(()) });
        chats.insert(id.clone(), chat.clone());
        Ok(chat)
    }

    /// Runs the `message_in` hook on a message for `thread`: its text as the hooks left it, or
    /// `None` when one handled it (its reply, if any, is sent).
    async fn message_in(&self, channel: &Arc<dyn Messenger>, thread: &Thread, data: Value) -> Result<Option<String>> {
        let text = data["text"].as_str().unwrap_or_default().to_string();
        if !self.ext.listens("message_in") {
            return Ok(Some(text));
        }
        let data = self.ext.emit("message_in", data, &extensions::Origin::thread(thread.clone())).await;
        if data["handled"] == true {
            if let Some(reply) = data["reply"].as_str().filter(|r| !r.is_empty()) {
                channel.send(&thread.id, &OutMessage::text(reply)).await?;
            }
            channel.presence(&thread.id, false).await;
            return Ok(None);
        }
        Ok(Some(data["text"].as_str().map(String::from).unwrap_or(text)))
    }

    async fn handle(self: Arc<Self>, ev: Inbound) -> Result<()> {
        let Some(channel) = self.channels.get(&ev.thread.messenger).cloned() else {
            return Ok(());
        };
        let chat = ev.thread.id.clone();
        self.activity.lock().unwrap().insert(ev.thread.clone(), chrono::Utc::now().timestamp_millis());
        // What something waits for (the answer to a question) goes there first.
        if self.waits.offer(&ev) {
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
                let state = self.chat(&ev.thread).await?;
                let intake = state.intake.lock().await;
                let (note, images, saved) = self.receive(&*channel, &files).await;
                let mut text = [text, note].into_iter().filter(|s| !s.is_empty()).collect::<Vec<_>>().join("\n");
                let data = serde_json::json!({"id": message, "text": text, "files": saved, "source": "user"});
                let Some(t) = self.message_in(&channel, &ev.thread, data).await? else {
                    return Ok(());
                };
                text = t;
                // While a turn runs, plain text goes to it instead of waiting for it to end.
                if images.is_empty() && state.inbox.offer(&text) {
                    drop(intake);
                    channel.send(&chat, &OutMessage::text("↪️ Got it, I'll take this into account.")).await?;
                    return Ok(());
                }
                // Busy from here, so the next message joins this turn instead of racing it.
                state.inbox.start();
                drop(intake);
                self.turn(channel, ev.thread, &chat, &text, images).await?
            }
        }
        Ok(())
    }

}

/// Runs the agent behind every configured messenger (foreground).
pub async fn serve() -> Result<()> {
    start(crate::messengers::build_configured()?).await
}

/// Runs the agent behind `chans` until they all stop.
pub async fn start(chans: Vec<Arc<dyn Messenger>>) -> Result<()> {
    let workspace = crate::config::workspace()?;
    let selection = providers::selection()?;
    let model = (selection.provider.id.to_string(), selection.model.clone().unwrap_or_default());
    let label = format!("{} · {}", model.0, model.1);
    let provider = providers::build(selection).await?;
    println!(
        "august serving {} · {label} · workspace {}",
        chans.iter().map(|c| c.id().to_string()).collect::<Vec<_>>().join(", "),
        workspace.display()
    );
    let ext = Extensions::new(extensions::dir());
    Gateway::new(chans, provider, model, workspace, Db::open()?, ext).run().await
}
