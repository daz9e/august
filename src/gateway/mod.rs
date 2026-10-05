//! Connects channels to agents: one agent session per chat, slash commands,
//! streamed replies rendered as live-edited messages, and button approvals.

mod approval;
mod commands;
mod render;
mod turn;

use crate::agent::{self, Agent};
use crate::channels::bus::Bus;
use crate::db::Db;
use crate::channels::{Channel, ChatId, Inbound, InboundKind};
use crate::llm::LlmProvider;
use crate::llm::providers;
use crate::scheduler::TaskRunner;
use crate::tools::ToolRegistry;
use anyhow::Result;
use async_trait::async_trait;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex as StdMutex, RwLock};
use tokio::sync::{Mutex, Notify};

const SURFACE: &str = "The user reads your replies in a chat app that renders Markdown \
    (bold, italic, `code`, fenced code blocks, lists, links). Avoid tables and headings \
    unless they really help.";

struct Chat {
    agent: Mutex<Agent>,
    /// Set while a turn runs; `/stop` notifies it.
    cancel: StdMutex<Option<Arc<Notify>>>,
}

pub struct Gateway {
    channels: HashMap<String, Arc<dyn Channel>>,
    chats: Mutex<HashMap<ChatId, Arc<Chat>>>,
    pending: approval::Pending,
    provider: RwLock<Arc<dyn LlmProvider>>,
    provider_label: RwLock<String>,
    workspace: PathBuf,
    pub db: Arc<Db>,
}

#[async_trait]
impl TaskRunner for Arc<Gateway> {
    async fn run_task(&self, task: crate::db::Task) -> Result<()> {
        self.run_task_in_chat(task).await
    }
}

impl Gateway {
    pub fn new(
        channels: Vec<Arc<dyn Channel>>,
        provider: Arc<dyn LlmProvider>,
        label: String,
        workspace: PathBuf,
        db: Arc<Db>,
    ) -> Arc<Self> {
        Arc::new(Self {
            channels: channels.into_iter().map(|c| (c.id().to_string(), c)).collect(),
            chats: Mutex::new(HashMap::new()),
            pending: Arc::default(),
            provider: RwLock::new(provider),
            provider_label: RwLock::new(label),
            workspace,
            db,
        })
    }

    /// Runs every channel and dispatches their events until all channels stop.
    pub async fn run(self: Arc<Self>) -> Result<()> {
        let bus: Bus<Inbound> = Bus::new(256);
        let mut events = bus.subscribe();
        let mut tasks = tokio::task::JoinSet::new();
        for ch in self.channels.values() {
            let (ch, bus) = (ch.clone(), bus.clone());
            if let Err(e) = ch.set_commands(commands::COMMANDS).await {
                eprintln!("{}: could not register commands: {e:#}", ch.id());
            }
            tasks.spawn(async move {
                let r = ch.run(bus).await;
                (ch.id().to_string(), r)
            });
        }
        drop(bus);

        let me = self.clone();
        let scheduler = tokio::spawn(crate::scheduler::run(self.db.clone(), Arc::new(self.clone())));
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
        scheduler.abort();
        dispatcher.await.ok();
        Ok(())
    }

    async fn chat(&self, id: &ChatId) -> Result<Arc<Chat>> {
        let mut chats = self.chats.lock().await;
        if let Some(chat) = chats.get(id) {
            return Ok(chat.clone());
        }
        let agent = Agent::new(
            self.provider.read().unwrap().clone(),
            ToolRegistry::with_defaults(),
            agent::system_prompt(&self.workspace, SURFACE),
            self.db.clone(),
            &format!("{}:{}", id.channel, id.chat),
        )?;
        let chat = Arc::new(Chat { agent: Mutex::new(agent), cancel: StdMutex::new(None) });
        chats.insert(id.clone(), chat.clone());
        Ok(chat)
    }

    /// Runs a due scheduled task as a turn in the chat it was created in.
    async fn run_task_in_chat(self: &Arc<Self>, task: crate::db::Task) -> Result<()> {
        let Some(channel) = self.channels.get(&task.channel).cloned() else {
            anyhow::bail!("task #{}: channel `{}` is not running", task.id, task.channel);
        };
        eprintln!("task #{} fires in {}:{}", task.id, task.channel, task.chat);
        let id = ChatId { channel: task.channel.clone(), chat: task.chat.clone() };
        let text = format!("[Scheduled task #{} fired] {}", task.id, task.prompt);
        self.turn(channel, id, &task.chat, &text).await
    }

    async fn handle(self: Arc<Self>, ev: Inbound) -> Result<()> {
        let Some(channel) = self.channels.get(&ev.chat.channel).cloned() else {
            return Ok(());
        };
        let chat = ev.chat.chat.clone();
        eprintln!("{} · {} ({}): {:?}", ev.chat.channel, ev.user.name, ev.user.id, ev.kind);
        match ev.kind {
            InboundKind::Action { id, data } => {
                channel.ack_action(&id).await.ok();
                // "ap:<approval id>:<y|n>"
                let mut parts = data.splitn(3, ':');
                if let (Some("ap"), Some(key), Some(answer)) = (parts.next(), parts.next(), parts.next()) {
                    if let Some(tx) = self.pending.lock().unwrap().remove(key) {
                        tx.send(answer == "y").ok();
                    }
                }
            }
            InboundKind::Command { name, args } => self.command(&channel, &ev.chat, &name, &args).await?,
            InboundKind::Text(text) if text.is_empty() => {}
            InboundKind::Text(text) => self.turn(channel, ev.chat, &chat, &text).await?,
        }
        Ok(())
    }

}

/// Runs the agent behind every configured messenger (foreground).
pub async fn serve() -> Result<()> {
    let chans = crate::channels::build_configured()?;
    let workspace = crate::config::workspace()?;
    let selection = providers::selection()?;
    let label = format!("{} · {}", selection.provider.id, selection.model.clone().unwrap_or_default());
    let provider = providers::build(selection).await?;
    println!(
        "august serving {} · {label} · workspace {}",
        chans.iter().map(|c| c.id().to_string()).collect::<Vec<_>>().join(", "),
        workspace.display()
    );
    Gateway::new(chans, provider, label, workspace, Db::open()?).run().await
}
