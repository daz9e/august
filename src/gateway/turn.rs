//! One user turn: runs the agent, streams its events to the chat, handles `/stop`.

use super::Gateway;
use super::approval::ChatApprover;
use super::render::{Ui, render, tool_line};
use crate::agent::Event;
use crate::channels::{Channel, ChatId};
use crate::llm::Block;
use crate::tools::{FileSink, ToolCtx};
use anyhow::Result;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Notify, mpsc, oneshot};

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

/// Background notes (what the review saved) go straight to the chat.
struct ChatNotes {
    channel: Arc<dyn Channel>,
    chat: String,
}

#[async_trait::async_trait]
impl crate::tools::Notifier for ChatNotes {
    async fn notify(&self, text: &str) {
        if let Err(e) = self.channel.send(&self.chat, text, &[]).await {
            eprintln!("could not post a note to the chat: {e:#}");
        }
    }
}

impl Gateway {
    /// Runs a turn once the chat is free, then whatever the user sent meanwhile that the
    /// turn didn't pick up.
    pub(super) async fn turn(
        self: &Arc<Self>,
        channel: Arc<dyn Channel>,
        id: ChatId,
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
            if left.is_empty() {
                return r;
            }
            (text, images) = (left.join("\n"), Vec::new());
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn turn_once(
        self: &Arc<Self>,
        state: &super::Chat,
        agent: &mut crate::agent::Agent,
        channel: Arc<dyn Channel>,
        id: &ChatId,
        chat: &str,
        text: &str,
        images: Vec<Block>,
        scheduled: bool,
    ) -> Result<()> {
        let cancel = Arc::new(Notify::new());
        *state.cancel.lock().unwrap() = Some(cancel.clone());
        agent.set_provider(self.provider.read().unwrap().clone());

        let typing = {
            let (ch, chat) = (channel.clone(), chat.to_string());
            tokio::spawn(async move {
                loop {
                    ch.typing(&chat).await.ok();
                    tokio::time::sleep(Duration::from_secs(4)).await;
                }
            })
        };
        let (tx, rx) = mpsc::unbounded_channel();
        let renderer = tokio::spawn(render(channel.clone(), chat.to_string(), rx));

        let ctx = ToolCtx {
            workspace: self.workspace.clone(),
            approver: Arc::new(ChatApprover {
                channel: channel.clone(),
                chat: chat.to_string(),
                pending: self.pending.clone(),
            }),
            db: self.db.clone(),
            origin: Some((id.channel.clone(), chat.to_string())),
            files: Some(Arc::new(ChatFiles(tx.clone()))),
            extensions: Some(self.ext.clone()),
            scheduled,
            notify: Some(Arc::new(ChatNotes { channel: channel.clone(), chat: chat.to_string() })),
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
                Event::Compacted { .. } => Ui::Tool("🗜 Older messages summarised to free up context".into()),
                _ => return,
            };
            tx2.send(ui).ok();
        };

        let outcome = tokio::select! {
            r = agent.run_turn_with(text, images, &ctx, &mut on_event) => Some(r),
            _ = cancel.notified() => None,
        };
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
        *state.cancel.lock().unwrap() = None;
        typing.abort();
        drop(tx);
        drop(ctx);
        drop(on_event);
        renderer.await.ok();
        Ok(())
    }
}
