//! `delegate_task` in a chat: the sub-agent runs in the background with a fresh
//! conversation of its own, and its report comes back to the chat as a new message.

use super::Gateway;
use crate::agent::{self, Agent};
use crate::channels::{Channel, ChatId};
use crate::tools::{Approver, Delegate, ToolCtx, ToolRegistry};
use anyhow::Result;
use async_trait::async_trait;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Weak};

/// What a sub-agent may not do: talk to the user, change memory or skills, schedule or
/// delegate more work.
const BLOCKED: &[&str] = &[
    "delegate_task", "schedule_task", "list_tasks", "cancel_task", "remember", "forget", "send_file",
    "save_skill", "edit_skill", "save_extension",
];

const SURFACE: &str = "You are a sub-agent doing one task for the main agent, which talks to \
    the user. You see only the task below, not their conversation, and nobody can answer \
    questions: work autonomously and make reasonable assumptions. Finish with a concise report \
    for the main agent: what you did, what you found, files you changed, and anything left \
    open or uncertain.";

pub(super) struct Subtasks {
    pub(super) gw: Weak<Gateway>,
    pub(super) channel: Arc<dyn Channel>,
    pub(super) id: ChatId,
    pub(super) approver: Arc<dyn Approver>,
}

#[async_trait]
impl Delegate for Subtasks {
    async fn delegate(&self, goal: &str, context: &str) -> Result<String> {
        let gw = self.gw.upgrade().ok_or_else(|| anyhow::anyhow!("August is shutting down"))?;
        let n = gw.subtasks.fetch_add(1, Ordering::Relaxed) + 1;
        let key = format!("{}:{}#sub{n}", self.id.channel, self.id.chat);
        let tools = ToolRegistry::with_defaults().without(BLOCKED).with_extensions(gw.ext.clone()).with_mcp(gw.mcp.clone());
        let provider = gw.provider.read().unwrap().clone();
        let mut agent = Agent::new(provider, tools, agent::system_prompt(&gw.workspace, SURFACE), gw.db.clone(), &key)?;
        let ctx = ToolCtx {
            workspace: gw.workspace.clone(),
            approver: self.approver.clone(),
            db: gw.db.clone(),
            origin: Some((self.id.channel.clone(), self.id.chat.clone())),
            files: None,
            extensions: Some(gw.ext.clone()),
            unattended: true,
            notify: None,
            inbox: None,
            delegate: None,
        };
        let task = if context.trim().is_empty() { goal.to_string() } else { format!("{goal}\n\nContext:\n{context}") };
        let (channel, id, title) = (self.channel.clone(), self.id.clone(), goal.chars().take(80).collect::<String>());
        eprintln!("subtask #{n} starts in {}:{}: {title}", id.channel, id.chat);
        // ponytail: subtasks are not cancelled by /stop and not capped in number; add both if they get used heavily.
        tokio::spawn(async move {
            let report = match agent.run_turn(&task, &ctx, &mut |_| {}).await {
                Ok(text) => format!("[Subtask #{n} finished: {title}]\n{text}"),
                Err(e) => format!("[Subtask #{n} failed: {title}]\n{e:#}"),
            };
            drop(ctx);
            gw.deliver(channel, id, &report).await;
        });
        Ok(format!(
            "Started subtask #{n}. Its report will arrive as a new message; carry on with other work or end your turn."
        ))
    }
}

impl Gateway {
    /// Hands a message to the chat as if the user sent it: into the running turn, or as
    /// a new one.
    pub(super) async fn deliver(self: &Arc<Self>, channel: Arc<dyn Channel>, id: ChatId, text: &str) {
        let offered = match self.chat(&id).await {
            Ok(state) => state.inbox.offer(text),
            Err(e) => return eprintln!("gateway: {e:#}"),
        };
        if !offered {
            let chat = id.chat.clone();
            if let Err(e) = self.turn(channel, id, &chat, text, Vec::new(), false).await {
                eprintln!("gateway: {e:#}");
            }
        }
    }
}
