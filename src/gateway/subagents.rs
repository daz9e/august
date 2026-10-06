//! Sub-agents for extensions (`ctx.agent`): a fresh conversation of their own in a chat,
//! with the chat's approvals, unattended, returning the final reply.

use super::Gateway;
use super::approval::ChatApprover;
use crate::agent::{self, Agent};
use crate::channels::{Channel, ChatId};
use crate::extensions::AgentOpts;
use crate::tools::ToolCtx;
use anyhow::Result;
use std::sync::Arc;
use std::sync::atomic::Ordering;

impl Gateway {
    pub(super) async fn subagent(&self, channel: Arc<dyn Channel>, id: ChatId, task: &str, opts: AgentOpts) -> Result<String> {
        let n = self.subagents.fetch_add(1, Ordering::Relaxed) + 1;
        let key = format!("{}:{}#agent{n}", id.channel, id.chat);
        let mut tools = self.tools().without(&opts.exclude);
        if let Some(only) = &opts.tools {
            tools = tools.only(only);
        }
        let provider = self.provider.read().unwrap().clone();
        let system = agent::system_prompt(&self.workspace, opts.system.as_deref().unwrap_or(""));
        let mut agent = Agent::new(provider, tools, system, self.db.clone(), &key)?;
        let ctx = ToolCtx {
            workspace: self.workspace.clone(),
            approver: Arc::new(ChatApprover { channel, chat: id.chat.clone(), pending: self.pending.clone() }),
            db: self.db.clone(),
            origin: Some((id.channel, id.chat)),
            files: None,
            extensions: Some(self.ext.clone()),
            unattended: true,
            notify: None,
            inbox: None,
        };
        eprintln!("sub-agent #{n} starts: {}", task.chars().take(80).collect::<String>());
        agent.run_turn(task, &ctx, &mut |_| {}).await
    }

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
