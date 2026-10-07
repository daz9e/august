//! Connects channels to agents: one agent session per chat, slash commands,
//! streamed replies rendered as live-edited messages, and button approvals.

mod approval;
mod commands;
mod media;
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
use crate::llm::{LlmProvider, Message};
use crate::llm::providers;
use crate::tools::{ToolCtx, ToolRegistry};
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
    provider_label: RwLock<String>,
    workspace: PathBuf,
    pub db: Arc<Db>,
    ext: Arc<Extensions>,
    /// Numbers sub-agents.
    subagents: std::sync::atomic::AtomicU64,
}

/// The core's primitives as extensions call them.
struct ExtCore(Weak<Gateway>);

impl ExtCore {
    fn gateway(&self) -> Result<Arc<Gateway>> {
        self.0.upgrade().ok_or_else(|| anyhow::anyhow!("August is shutting down"))
    }

    fn messenger(&self, thread: &Thread) -> Result<(Arc<Gateway>, Arc<dyn Messenger>)> {
        let gw = self.gateway()?;
        let m = gw.channels.get(&thread.messenger).cloned();
        let m = m.ok_or_else(|| anyhow::anyhow!("messenger `{}` is not running", thread.messenger))?;
        Ok((gw, m))
    }
}

#[async_trait]
impl extensions::Core for ExtCore {
    async fn messengers(&self) -> Result<Value> {
        let gw = self.gateway()?;
        let seen = gw.activity.lock().unwrap().clone();
        let active = seen.iter().max_by_key(|(_, at)| **at).map(|(t, _)| t.clone());
        let mut all: Vec<&Arc<dyn Messenger>> = gw.channels.values().collect();
        all.sort_by_key(|m| m.id().to_string());
        let mut out = Vec::new();
        for m in all {
            let d = m.describe();
            let mut ids = m.threads().await;
            for t in seen.keys().filter(|t| t.messenger == d.id) {
                if !ids.contains(&t.id) {
                    ids.push(t.id.clone());
                }
            }
            let threads: Vec<Value> = ids
                .into_iter()
                .map(|id| {
                    let thread = Thread::new(&d.id, &id);
                    serde_json::json!({"id": id, "active": active.as_ref() == Some(&thread), "last_seen": seen.get(&thread)})
                })
                .collect();
            out.push(serde_json::json!({"id": d.id, "name": d.name, "capabilities": d.capabilities, "extra": d.extra, "threads": threads}));
        }
        Ok(Value::Array(out))
    }

    async fn send(&self, thread: &Thread, message: OutMessage) -> Result<String> {
        let (_, m) = self.messenger(thread)?;
        m.send(&thread.id, &message).await
    }

    async fn edit(&self, thread: &Thread, id: &str, message: OutMessage) -> Result<()> {
        let (_, m) = self.messenger(thread)?;
        m.edit(&thread.id, id, &message).await
    }

    async fn delete(&self, thread: &Thread, id: &str) -> Result<()> {
        let (_, m) = self.messenger(thread)?;
        m.delete(&thread.id, id).await
    }

    async fn react(&self, thread: &Thread, id: &str, emoji: &str) -> Result<()> {
        let (_, m) = self.messenger(thread)?;
        m.react(&thread.id, id, emoji).await
    }

    fn listen(&self, thread: &Thread, buttons: Vec<String>, text: bool, ttl: std::time::Duration) -> Result<u64> {
        let gw = self.gateway()?;
        let (id, rx) = gw.waits.add(thread.clone(), waits::Accept { buttons, text });
        gw.listeners.lock().unwrap().insert(id, rx);
        // A listener nobody collects mustn't keep taking the user's messages.
        let me = Arc::downgrade(&gw);
        tokio::spawn(async move {
            tokio::time::sleep(ttl).await;
            if let Some(gw) = me.upgrade()
                && gw.listeners.lock().unwrap().remove(&id).is_some()
            {
                gw.waits.remove(id);
            }
        });
        Ok(id)
    }

    async fn next(&self, listener: u64, timeout: std::time::Duration) -> Result<Value> {
        let gw = self.gateway()?;
        let rx = gw.listeners.lock().unwrap().remove(&listener);
        let rx = rx.ok_or_else(|| anyhow::anyhow!("no listener #{listener} (it gave its event, or its time ran out)"))?;
        let reply = tokio::time::timeout(timeout, rx).await.ok().and_then(Result::ok);
        gw.waits.remove(listener);
        Ok(match reply {
            Some(waits::Reply::Press(button)) => serde_json::json!({"press": button}),
            Some(waits::Reply::Text(text)) => serde_json::json!({"text": text}),
            Some(waits::Reply::Cancelled(why)) => serde_json::json!({"cancelled": why}),
            None => serde_json::json!({"timeout": true}),
        })
    }

    async fn prompt(&self, thread: &Thread, text: &str) -> Result<()> {
        let (gw, m) = self.messenger(thread)?;
        let (thread, text) = (thread.clone(), text.to_string());
        // Not awaited: the caller may be inside a turn of that very thread.
        tokio::spawn(async move { gw.deliver(m, thread, &text).await });
        Ok(())
    }

    fn start_turn(&self, thread: &Thread, request: Value) -> Result<u64> {
        let gw = self.gateway()?;
        let mut req: turns::TurnRequest = serde_json::from_value(request)?;
        req.thread = Some(thread.clone());
        gw.start_turn(req)
    }

    async fn wait_turn(&self, id: u64, timeout: std::time::Duration) -> Result<Value> {
        self.gateway()?.wait_turn(id, timeout).await
    }

    fn cancel_turn(&self, id: u64) -> bool {
        self.gateway().is_ok_and(|gw| gw.turns.cancel(id))
    }

    fn turns(&self, thread: Option<&Thread>) -> Value {
        self.gateway().map(|gw| gw.turns.list(thread)).unwrap_or_default()
    }

    async fn approve(&self, thread: &Thread, action: &str) -> Result<bool> {
        let (gw, m) = self.messenger(thread)?;
        let approver = gw.approver(m, thread.clone(), None);
        Ok(crate::tools::Approver::approve(&approver, action).await)
    }

    async fn call_tool(&self, thread: &Thread, name: &str, input: &Value) -> Result<(String, bool)> {
        let (gw, m) = self.messenger(thread)?;
        let files = turn::ThreadFiles { messenger: m.clone(), thread: thread.id.clone() };
        let ctx = ToolCtx {
            workspace: gw.workspace.clone(),
            approver: Arc::new(gw.approver(m, thread.clone(), None)),
            db: gw.db.clone(),
            origin: extensions::Origin::thread(thread.clone()),
            files: Some(Arc::new(files)),
            extensions: Some(gw.ext.clone()),
            inbox: None,
        };
        Ok(gw.tools().call(name, input, &ctx).await)
    }

    fn changed(&self) {
        if let Ok(gw) = self.gateway() {
            tokio::spawn(async move { gw.publish_commands().await });
        }
    }

    fn store(&self) -> Result<Arc<Db>> {
        Ok(self.gateway()?.db.clone())
    }

    async fn llm(&self, prompt: &str, system: &str) -> Result<String> {
        let provider = self.gateway()?.provider.read().unwrap().clone();
        let c = provider.complete(&crate::util::new_uuid(), system, &[Message::user_text(prompt)], &[]).await?;
        Ok(c.message.text())
    }
}

impl Gateway {
    pub fn new(
        channels: Vec<Arc<dyn Messenger>>,
        provider: Arc<dyn LlmProvider>,
        label: String,
        workspace: PathBuf,
        db: Arc<Db>,
        ext: Arc<Extensions>,
    ) -> Arc<Self> {
        let gw = Arc::new(Self {
            channels: channels.into_iter().map(|c| (c.id().to_string(), c)).collect(),
            chats: Mutex::new(HashMap::new()),
            waits: Arc::default(),
            turns: Default::default(),
            listeners: Default::default(),
            activity: Default::default(),
            provider: RwLock::new(provider),
            provider_label: RwLock::new(label),
            workspace,
            db,
            ext,
            subagents: Default::default(),
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

    /// Approvals asked in `thread`; with `inbox`, a text answer also reaches the running turn.
    fn approver(&self, messenger: Arc<dyn Messenger>, thread: Thread, inbox: Option<Arc<agent::Inbox>>) -> approval::ChatApprover {
        approval::ChatApprover { messenger, thread, waits: self.waits.clone(), inbox }
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
                if self.ext.listens("message_in") {
                    let data = self.ext.emit("message_in", serde_json::json!({"id": message, "text": text, "files": saved}), &extensions::Origin::thread(ev.thread.clone())).await;
                    if data["handled"] == true {
                        if let Some(reply) = data["reply"].as_str().filter(|r| !r.is_empty()) {
                            channel.send(&chat, &OutMessage::text(reply)).await?;
                        }
                        channel.presence(&chat, false).await;
                        return Ok(());
                    }
                    if let Some(t) = data["text"].as_str() {
                        text = t.to_string();
                    }
                }
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
    let label = format!("{} · {}", selection.provider.id, selection.model.clone().unwrap_or_default());
    let provider = providers::build(selection).await?;
    println!(
        "august serving {} · {label} · workspace {}",
        chans.iter().map(|c| c.id().to_string()).collect::<Vec<_>>().join(", "),
        workspace.display()
    );
    let ext = Extensions::new(extensions::dir());
    Gateway::new(chans, provider, label, workspace, Db::open()?, ext).run().await
}
