//! Connects channels to agents: one agent session per chat, slash commands,
//! streamed replies rendered as live-edited messages, and button approvals.

mod approval;
mod commands;
mod media;
mod render;
mod subagents;
mod turn;
mod waits;

use crate::agent::{self, Agent};
use crate::messengers::bus::Bus;
use crate::db::Db;
use crate::extensions::{self, Extensions};
use crate::messengers::{Inbound, InboundKind, Messenger, OutMessage, Thread};
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
    channels: HashMap<String, Arc<dyn Messenger>>,
    chats: Mutex<HashMap<Thread, Arc<Chat>>>,
    /// Waits for what threads send next (answers to questions).
    waits: Arc<waits::Waits>,
    /// Extensions' listeners (`listen`) until they take their event with `next`.
    listeners: StdMutex<HashMap<u64, tokio::sync::oneshot::Receiver<waits::Reply>>>,
    /// When each thread last sent something (Unix seconds).
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

    fn listen(&self, thread: &Thread, buttons: Vec<String>, text: bool) -> u64 {
        let Ok(gw) = self.gateway() else { return 0 };
        let (id, rx) = gw.waits.add(thread.clone(), waits::Accept { buttons, text });
        gw.listeners.lock().unwrap().insert(id, rx);
        id
    }

    async fn next(&self, listener: u64, timeout: std::time::Duration) -> Result<Value> {
        let gw = self.gateway()?;
        let rx = gw.listeners.lock().unwrap().remove(&listener);
        let rx = rx.ok_or_else(|| anyhow::anyhow!("no listener #{listener} (each one gives one event)"))?;
        let reply = tokio::time::timeout(timeout, rx).await.ok().and_then(Result::ok);
        gw.waits.remove(listener);
        Ok(match reply {
            Some(waits::Reply::Press(button)) => serde_json::json!({"press": button}),
            Some(waits::Reply::Text(text)) => serde_json::json!({"text": text}),
            None => Value::Null,
        })
    }

    async fn prompt(&self, thread: &Thread, text: &str) -> Result<()> {
        let (gw, m) = self.messenger(thread)?;
        let (thread, text) = (thread.clone(), text.to_string());
        // Not awaited: the caller may be inside a turn of that very thread.
        tokio::spawn(async move { gw.deliver(m, thread, &text).await });
        Ok(())
    }

    async fn agent(&self, thread: &Thread, task: &str, opts: extensions::AgentOpts) -> Result<String> {
        let (gw, m) = self.messenger(thread)?;
        gw.subagent(m, thread.clone(), task, opts).await
    }

    async fn approve(&self, thread: &Thread, action: &str) -> Result<bool> {
        let (gw, m) = self.messenger(thread)?;
        let approver = gw.approver(m, thread.clone(), None);
        Ok(crate::tools::Approver::approve(&approver, action).await)
    }

    async fn call_tool(&self, thread: &Thread, name: &str, input: &Value) -> Result<(String, bool)> {
        let (gw, m) = self.messenger(thread)?;
        let ctx = ToolCtx {
            workspace: gw.workspace.clone(),
            approver: Arc::new(gw.approver(m, thread.clone(), None)),
            db: gw.db.clone(),
            origin: Some(thread.clone()),
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
        let mut id = Thread { messenger: task.channel.clone(), id: task.chat.clone() };
        if task.isolated {
            id.id = format!("{}#task{}", task.chat, task.id);
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
        let Some(channel) = self.channels.get(&ev.thread.messenger).cloned() else {
            return Ok(());
        };
        let chat = ev.thread.id.clone();
        self.activity.lock().unwrap().insert(ev.thread.clone(), chrono::Utc::now().timestamp());
        // What something waits for (the answer to a question) goes there first.
        if self.waits.offer(&ev) {
            if let InboundKind::Press { ack, .. } = &ev.kind {
                channel.ack(ack).await.ok();
            }
            return Ok(());
        }
        match ev.kind {
            // A button of a question nobody waits for any more.
            InboundKind::Press { ack, .. } => {
                channel.ack(&ack).await.ok();
            }
            InboundKind::Command { name, args } => self.command(&channel, &ev.thread, &name, &args).await?,
            InboundKind::Message { text, files } if text.is_empty() && files.is_empty() => {}
            InboundKind::Message { text, files } => {
                let state = self.chat(&ev.thread).await?;
                let intake = state.intake.lock().await;
                let (note, images, saved) = self.receive(&*channel, &files).await;
                let mut text = [text, note].into_iter().filter(|s| !s.is_empty()).collect::<Vec<_>>().join("\n");
                if self.ext.listens("message_in") {
                    let data = self.ext.emit("message_in", serde_json::json!({"text": text, "files": saved}), &Some(ev.thread.clone())).await;
                    if data["handled"] == true {
                        if let Some(reply) = data["reply"].as_str().filter(|r| !r.is_empty()) {
                            channel.send(&chat, &OutMessage::text(reply)).await?;
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
                    channel.send(&chat, &OutMessage::text("↪️ Got it, I'll take this into account.")).await?;
                    return Ok(());
                }
                // Busy from here, so the next message joins this turn instead of racing it.
                state.inbox.start();
                drop(intake);
                self.turn(channel, ev.thread, &chat, &text, images, false).await?
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
