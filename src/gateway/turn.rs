//! One user turn: runs the agent, streams its events to the chat, handles `/stop`.

use super::Gateway;
use super::render::{Ui, render, tool_line};
use crate::agent::Event;
use crate::messengers::{Messenger, Thread};
use crate::llm::Block;
use crate::tools::{FileSink, ToolCtx};
use anyhow::Result;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};

/// `send_file` from a chat turn: goes through the renderer so the file lands
/// after the text streamed so far.
struct ChatFiles(mpsc::UnboundedSender<Ui>);

#[async_trait::async_trait]
impl FileSink for ChatFiles {
    async fn send_file(&self, path: &std::path::Path, caption: &str) -> Result<()> {
        let (done, result) = oneshot::channel();
        self.0
            .send(Ui::File { path: path.to_path_buf(), caption: caption.to_string(), done })
            .map_err(|_| anyhow::anyhow!("the chat is gone"))?;
        result.await.map_err(|_| anyhow::anyhow!("the chat is gone"))?
    }
}

/// `send_file` outside a turn (a tool an extension runs): straight to the thread.
pub(super) struct ThreadFiles {
    pub(super) messenger: Arc<dyn Messenger>,
    pub(super) thread: String,
}

#[async_trait::async_trait]
impl FileSink for ThreadFiles {
    async fn send_file(&self, path: &std::path::Path, caption: &str) -> Result<()> {
        self.messenger.send_file(&self.thread, path, caption).await
    }
}

impl Gateway {
    /// Runs a turn once the chat is free, then whatever the user sent meanwhile that the
    /// turn didn't pick up.
    pub(super) async fn turn(
        self: &Arc<Self>,
        channel: Arc<dyn Messenger>,
        id: Thread,
        chat: &str,
        text: &str,
        images: Vec<Block>,
        scheduled: bool,
    ) -> Result<()> {
        let state = self.chat(&id).await?;
        let mut agent = state.agent.lock().await; // turns in one chat run in order
        state.inbox.start();
        let (mut text, mut images) = (text.to_string(), images);
        loop {
            let r = self.turn_once(&state, &mut agent, channel.clone(), &id, chat, &text, images, scheduled).await;
            let left = state.inbox.finish();
            if !left.is_empty() {
                (text, images) = (left.join("\n"), Vec::new());
                continue;
            }
            channel.idle(chat).await;
            return r.map(|_| ());
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn turn_once(
        self: &Arc<Self>,
        state: &super::Chat,
        agent: &mut crate::agent::Agent,
        channel: Arc<dyn Messenger>,
        id: &Thread,
        chat: &str,
        text: &str,
        images: Vec<Block>,
        scheduled: bool,
    ) -> Result<Option<String>> {
        let mode = if scheduled { crate::agent::TurnMode::Quiet } else { crate::agent::TurnMode::Visible };
        let source = scheduled.then(|| "scheduler".to_string());
        let (tag, cancel) = self.turns.begin(id, mode, source, None);
        agent.set_provider(self.provider.read().unwrap().clone());

        let typing = {
            let (ch, chat) = (channel.clone(), chat.to_string());
            let shows = ch.describe().capabilities.typing;
            tokio::spawn(async move {
                while shows {
                    ch.typing(&chat).await.ok();
                    tokio::time::sleep(Duration::from_secs(4)).await;
                }
            })
        };
        let (tx, rx) = mpsc::unbounded_channel();
        let renderer = tokio::spawn(render(channel.clone(), chat.to_string(), rx));

        let approver = Arc::new(self.approver(channel.clone(), id.clone(), Some(state.inbox.clone())));
        let ctx = ToolCtx {
            workspace: self.workspace.clone(),
            approver,
            db: self.db.clone(),
            origin: crate::extensions::Origin { thread: Some(id.clone()), turn: Some(tag.clone()) },
            files: Some(Arc::new(ChatFiles(tx.clone()))),
            extensions: Some(self.ext.clone()),
            unattended: scheduled,
            inbox: Some(state.inbox.clone()),
        };
        let streamed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let streamed2 = streamed.clone();
        let tx2 = tx.clone();
        let mut on_event = move |e: Event| {
            if scheduled {
                return; // only the final reply of a task reaches the chat
            }
            let ui = match e {
                Event::Text(t) => {
                    streamed2.store(true, std::sync::atomic::Ordering::Relaxed);
                    Ui::Text(t.to_string())
                }
                Event::Step => Ui::Step,
                Event::ToolCall { name, input } => Ui::Tool(tool_line(name, input)),
                Event::Compacted => Ui::Tool("🗜 Older messages summarised to free up context".into()),
            };
            tx2.send(ui).ok();
        };

        let outcome = tokio::select! {
            r = agent.run_turn_with(text, images, &ctx, &mut on_event) => Some(r),
            _ = cancel.notified() => None,
        };
        let result = outcome.as_ref().and_then(|r| r.as_ref().ok()).cloned();
        let ended = super::turns::Outcome::of(
            outcome.as_ref().map(|r| r.as_ref().map(String::clone).map_err(|e| anyhow::anyhow!("{e:#}"))),
            vec![serde_json::Value::Null; agent.tool_calls()],
        );
        match outcome {
            Some(Ok(reply)) if scheduled && reply.trim_start().starts_with("[SILENT]") => {}
            Some(Ok(reply)) => {
                if !streamed.load(std::sync::atomic::Ordering::Relaxed) {
                    tx.send(Ui::Text(if reply.is_empty() { "(empty reply)".into() } else { reply })).ok();
                }
            }
            Some(Err(e)) => {
                tx.send(Ui::Step).ok();
                tx.send(Ui::Text(format!("⚠️ Error: {e:#}"))).ok();
            }
            None => {
                agent.rollback_turn();
                tx.send(Ui::Step).ok();
                tx.send(Ui::Text("⏹ Stopped.".into())).ok();
            }
        }
        self.turns.end(tag.id);
        self.turn_ended(id, &tag, text, &ended);
        typing.abort();
        drop(tx);
        drop(ctx);
        drop(on_event);
        renderer.await.ok();
        Ok(result)
    }
}
