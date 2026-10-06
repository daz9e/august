//! Connects channels to agents: one agent session per chat, slash commands,
//! streamed replies rendered as live-edited messages, and button approvals.

mod approval;
mod commands;
mod media;
mod render;
mod subtasks;
mod turn;

use crate::agent::{self, Agent};
use crate::channels::bus::Bus;
use crate::db::Db;
use crate::extensions::{self, Extensions};
use crate::channels::{Channel, ChatId, Inbound, InboundKind};
use crate::llm::LlmProvider;
use crate::llm::providers;
use crate::mcp::Mcp;
use crate::scheduler::TaskRunner;
use crate::tools::ToolRegistry;
use anyhow::Result;
use async_trait::async_trait;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex as StdMutex, RwLock, Weak};
use tokio::sync::{Mutex, Notify};

const SURFACE: &str = "The user reads your replies in a chat app that renders Markdown \
    (bold, italic, `code`, fenced code blocks, lists, links). Avoid tables and headings \
    unless they really help.";

struct Chat {
    agent: Mutex<Agent>,
    /// Messages sent while a turn runs.
    inbox: Arc<agent::Inbox>,
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
    ext: Arc<Extensions>,
    mcp: Arc<Mcp>,
    /// Numbers subtasks.
    subtasks: std::sync::atomic::AtomicU64,
}

/// What extensions can do in the gateway: message chats and start turns.
struct ExtCore(Weak<Gateway>);

#[async_trait]
impl extensions::Core for ExtCore {
    async fn send(&self, channel: &str, chat: &str, text: &str) -> Result<()> {
        let gw = self.0.upgrade().ok_or_else(|| anyhow::anyhow!("August is shutting down"))?;
        let ch = gw.channels.get(channel).ok_or_else(|| anyhow::anyhow!("channel `{channel}` is not running"))?;
        ch.send(chat, text, &[]).await.map(|_| ())
    }

    async fn prompt(&self, channel: &str, chat: &str, text: &str) -> Result<()> {
        let gw = self.0.upgrade().ok_or_else(|| anyhow::anyhow!("August is shutting down"))?;
        let ch = gw.channels.get(channel).cloned().ok_or_else(|| anyhow::anyhow!("channel `{channel}` is not running"))?;
        let (id, chat, text) = (ChatId { channel: channel.into(), chat: chat.into() }, chat.to_string(), text.to_string());
        // Queued, not awaited: the caller may be inside a turn of that very chat.
        tokio::spawn(async move {
            if let Err(e) = gw.turn(ch, id, &chat, &text, Vec::new(), false).await {
                eprintln!("extension prompt: {e:#}");
            }
        });
        Ok(())
    }
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
        ext: Arc<Extensions>,
        mcp: Arc<Mcp>,
    ) -> Arc<Self> {
        let gw = Arc::new(Self {
            channels: channels.into_iter().map(|c| (c.id().to_string(), c)).collect(),
            chats: Mutex::new(HashMap::new()),
            pending: Arc::default(),
            provider: RwLock::new(provider),
            provider_label: RwLock::new(label),
            workspace,
            db,
            ext,
            mcp,
            subtasks: Default::default(),
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
        eprintln!("{}", self.mcp.status());
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
            ToolRegistry::with_defaults().with_extensions(self.ext.clone()).with_mcp(self.mcp.clone()),
            agent::system_prompt(&self.workspace, SURFACE),
            self.db.clone(),
            &format!("{}:{}", id.channel, id.chat),
        )?;
        let chat = Arc::new(Chat { agent: Mutex::new(agent), inbox: Arc::default(), cancel: StdMutex::new(None) });
        chats.insert(id.clone(), chat.clone());
        Ok(chat)
    }

    /// Runs a due scheduled task as a turn in the chat it was created in.
    async fn run_task_in_chat(self: &Arc<Self>, task: crate::db::Task) -> Result<()> {
        let Some(channel) = self.channels.get(&task.channel).cloned() else {
            anyhow::bail!("task #{}: channel `{}` is not running", task.id, task.channel);
        };
        eprintln!("task #{} fires in {}:{}", task.id, task.channel, task.chat);
        let mut text = format!(
            "[Scheduled task #{} fired; your reply goes to the user, or reply exactly [SILENT] if \
             there is nothing worth telling them]\n{}",
            task.id, task.prompt
        );
        for name in &task.skills {
            match crate::skills::load(name) {
                Ok(body) => text += &format!("\n\n[Skill `{name}`]\n{body}"),
                Err(e) => text += &format!("\n\n[Skill `{name}` could not be loaded: {e:#}]"),
            }
        }
        if let Some(script) = &task.script {
            text += &format!("\n\n[Output of the task's script `{script}`]\n{}", self.run_script(script).await);
        }
        // An isolated task gets a fresh conversation of its own on every run.
        let mut id = ChatId { channel: task.channel.clone(), chat: task.chat.clone() };
        if task.isolated {
            id.chat = format!("{}#task{}", task.chat, task.id);
            self.chat(&id).await?.agent.lock().await.reset()?;
        }
        self.turn(channel, id, &task.chat, &text, Vec::new(), true).await
    }

    async fn run_script(&self, script: &str) -> String {
        let run = tokio::process::Command::new("sh")
            .arg("-c")
            .arg(script)
            .current_dir(&self.workspace)
            .kill_on_drop(true)
            .output();
        match tokio::time::timeout(std::time::Duration::from_secs(120), run).await {
            Ok(Ok(out)) => {
                let mut s = String::from_utf8_lossy(&out.stdout).to_string();
                if !out.status.success() {
                    s += &format!("\n[exit status {}] {}", out.status, String::from_utf8_lossy(&out.stderr));
                }
                crate::tools::truncate(s, 20_000)
            }
            Ok(Err(e)) => format!("[could not run the script: {e}]"),
            Err(_) => "[the script timed out after 120 s]".into(),
        }
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
            InboundKind::Message { text, files } if text.is_empty() && files.is_empty() => {}
            InboundKind::Message { text, files } => {
                let (note, images) = self.receive(&*channel, &files).await;
                let mut text = [text, note].into_iter().filter(|s| !s.is_empty()).collect::<Vec<_>>().join("\n");
                if self.ext.listens("message_in") {
                    let origin = Some((ev.chat.channel.clone(), chat.clone()));
                    let data = self.ext.emit("message_in", serde_json::json!({"text": text}), &origin).await;
                    if data["handled"] == true {
                        if let Some(reply) = data["reply"].as_str().filter(|r| !r.is_empty()) {
                            channel.send(&chat, reply, &[]).await?;
                        }
                        return Ok(());
                    }
                    if let Some(t) = data["text"].as_str() {
                        text = t.to_string();
                    }
                }
                // While a turn runs, plain text goes to it instead of waiting for it to end.
                if images.is_empty() && self.chat(&ev.chat).await?.inbox.offer(&text) {
                    channel.send(&chat, "↪️ Got it, I'll take this into account.", &[]).await?;
                    return Ok(());
                }
                self.turn(channel, ev.chat, &chat, &text, images, false).await?
            }
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
    let ext = Extensions::new(extensions::dir());
    Gateway::new(chans, provider, label, workspace, Db::open()?, ext, Mcp::start().await).run().await
}
