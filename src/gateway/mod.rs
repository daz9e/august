//! Connects channels to agents: one agent session per chat, slash commands,
//! streamed replies rendered as live-edited messages, and button approvals.

mod approval;
mod commands;
mod media;
mod render;
mod subagents;
mod turn;

use crate::agent::{self, Agent};
use crate::channels::bus::Bus;
use crate::db::Db;
use crate::extensions::{self, Extensions};
use crate::channels::{Channel, ChatId, Inbound, InboundKind};
use crate::llm::{LlmProvider, Message};
use crate::llm::providers;
use crate::scheduler::TaskRunner;
use crate::tools::{ToolCtx, ToolRegistry};
use anyhow::Result;
use async_trait::async_trait;
use serde_json::Value;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex as StdMutex, RwLock, Weak};
use tokio::sync::{Mutex, Notify};

struct Chat {
    agent: Mutex<Agent>,
    /// Messages sent while a turn runs.
    inbox: Arc<agent::Inbox>,
    /// Set while a turn runs; `/stop` notifies it.
    cancel: StdMutex<Option<Arc<Notify>>>,
    /// Held while a message is prepared (files, `message_in`), so messages keep their order.
    intake: Mutex<()>,
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
    /// Numbers sub-agents.
    subagents: std::sync::atomic::AtomicU64,
}

/// What extensions can do in the gateway: message chats, start turns, run tools, ask the model.
struct ExtCore(Weak<Gateway>);

impl ExtCore {
    fn gateway(&self) -> Result<Arc<Gateway>> {
        self.0.upgrade().ok_or_else(|| anyhow::anyhow!("August is shutting down"))
    }

    fn channel(&self, channel: &str) -> Result<(Arc<Gateway>, Arc<dyn Channel>)> {
        let gw = self.gateway()?;
        let ch = gw.channels.get(channel).cloned().ok_or_else(|| anyhow::anyhow!("channel `{channel}` is not running"))?;
        Ok((gw, ch))
    }
}

#[async_trait]
impl extensions::Core for ExtCore {
    async fn send(&self, channel: &str, chat: &str, text: &str) -> Result<()> {
        let (_, ch) = self.channel(channel)?;
        ch.send(chat, text, &[]).await.map(|_| ())
    }

    async fn prompt(&self, channel: &str, chat: &str, text: &str) -> Result<()> {
        let (gw, ch) = self.channel(channel)?;
        let (id, text) = (ChatId { channel: channel.into(), chat: chat.into() }, text.to_string());
        // Not awaited: the caller may be inside a turn of that very chat.
        tokio::spawn(async move { gw.deliver(ch, id, &text).await });
        Ok(())
    }

    async fn agent(&self, channel: &str, chat: &str, task: &str, opts: extensions::AgentOpts) -> Result<String> {
        let (gw, ch) = self.channel(channel)?;
        gw.subagent(ch, ChatId { channel: channel.into(), chat: chat.into() }, task, opts).await
    }

    async fn approve(&self, channel: &str, chat: &str, action: &str) -> Result<bool> {
        let (gw, ch) = self.channel(channel)?;
        let approver = approval::ChatApprover { channel: ch, chat: chat.into(), pending: gw.pending.clone() };
        Ok(crate::tools::Approver::approve(&approver, action).await)
    }

    async fn ask(&self, channel: &str, chat: &str, question: &str, options: &[String]) -> Result<Option<String>> {
        let (gw, ch) = self.channel(channel)?;
        let asker = approval::ChatApprover { channel: ch, chat: chat.into(), pending: gw.pending.clone() };
        let values: Vec<String> = (0..options.len()).map(|i| i.to_string()).collect();
        let pairs: Vec<(&str, &str)> = options.iter().zip(&values).map(|(o, v)| (o.as_str(), v.as_str())).collect();
        let done = |label: Option<&str>| format!("❓ {question}\n→ {}", label.unwrap_or("⌛ no answer"));
        let answer = asker.ask(&format!("❓ {question}"), &pairs, done).await;
        Ok(answer.and_then(|i| options.get(i.parse::<usize>().ok()?).cloned()))
    }

    async fn call_tool(&self, channel: &str, chat: &str, name: &str, input: &Value) -> Result<(String, bool)> {
        let (gw, ch) = self.channel(channel)?;
        let ctx = ToolCtx {
            workspace: gw.workspace.clone(),
            approver: Arc::new(approval::ChatApprover { channel: ch, chat: chat.into(), pending: gw.pending.clone() }),
            db: gw.db.clone(),
            origin: Some((channel.into(), chat.into())),
            files: None,
            extensions: Some(gw.ext.clone()),
            unattended: false,
            notify: None,
            inbox: None,
        };
        Ok(gw.tools().call(name, input, &ctx).await)
    }

    async fn llm(&self, prompt: &str, system: &str) -> Result<String> {
        let provider = self.gateway()?.provider.read().unwrap().clone();
        let c = provider.complete(&crate::util::new_uuid(), system, &[Message::user_text(prompt)], &[]).await?;
        Ok(c.message.text())
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
        let channels = self.channels.keys().cloned().collect();
        let scheduler = tokio::spawn(crate::scheduler::run(self.db.clone(), Arc::new(self.clone()), channels));
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

    fn tools(&self) -> ToolRegistry {
        ToolRegistry::with_defaults().with_extensions(self.ext.clone())
    }

    async fn chat(&self, id: &ChatId) -> Result<Arc<Chat>> {
        let mut chats = self.chats.lock().await;
        if let Some(chat) = chats.get(id) {
            return Ok(chat.clone());
        }
        let agent = Agent::new(
            self.provider.read().unwrap().clone(),
            self.tools(),
            agent::system_prompt(&self.workspace, self.channels.get(&id.channel).map_or(crate::channels::CHAT_SURFACE, |c| c.surface())),
            self.db.clone(),
            &format!("{}:{}", id.channel, id.chat),
        )?;
        let chat = Arc::new(Chat { agent: Mutex::new(agent), inbox: Arc::default(), cancel: StdMutex::new(None), intake: Mutex::new(()) });
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
        match ev.kind {
            InboundKind::Action { id, data } => {
                channel.ack_action(&id).await.ok();
                // "ap:<approval id>:<y|n>"
                let mut parts = data.splitn(3, ':');
                if let (Some("ap"), Some(key), Some(answer)) = (parts.next(), parts.next(), parts.next()) {
                    if let Some(tx) = self.pending.lock().unwrap().remove(key) {
                        tx.send(answer.to_string()).ok();
                    }
                }
            }
            InboundKind::Command { name, args } => self.command(&channel, &ev.chat, &name, &args).await?,
            InboundKind::Message { text, files } if text.is_empty() && files.is_empty() => {}
            InboundKind::Message { text, files } => {
                let state = self.chat(&ev.chat).await?;
                let intake = state.intake.lock().await;
                let (note, images, saved) = self.receive(&*channel, &files).await;
                let mut text = [text, note].into_iter().filter(|s| !s.is_empty()).collect::<Vec<_>>().join("\n");
                if self.ext.listens("message_in") {
                    let origin = Some((ev.chat.channel.clone(), chat.clone()));
                    let data = self.ext.emit("message_in", serde_json::json!({"text": text, "files": saved}), &origin).await;
                    if data["handled"] == true {
                        if let Some(reply) = data["reply"].as_str().filter(|r| !r.is_empty()) {
                            channel.send(&chat, reply, &[]).await?;
                        }
                        channel.idle(&chat).await;
                        return Ok(());
                    }
                    if let Some(t) = data["text"].as_str() {
                        text = t.to_string();
                    }
                }
                // While a turn runs, plain text goes to it instead of waiting for it to end.
                if images.is_empty() && state.inbox.offer(&text) {
                    drop(intake);
                    channel.send(&chat, "↪️ Got it, I'll take this into account.", &[]).await?;
                    return Ok(());
                }
                // Busy from here, so the next message joins this turn instead of racing it.
                state.inbox.start();
                drop(intake);
                self.turn(channel, ev.chat, &chat, &text, images, false).await?
            }
        }
        Ok(())
    }

}

/// Runs the agent behind every configured messenger (foreground).
pub async fn serve() -> Result<()> {
    start(crate::channels::build_configured()?).await
}

/// Runs the agent behind `chans` until they all stop.
pub async fn start(chans: Vec<Arc<dyn Channel>>) -> Result<()> {
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
