//! Sub-agents (fresh turns): a conversation of their own for a thread, with the thread's
//! approvals, returning the final reply; and handing a thread a message.

use super::Gateway;
use crate::agent::{self, Agent};
use crate::messengers::{Messenger, Thread};
use crate::extensions::AgentOpts;
use crate::tools::ToolCtx;
use anyhow::Result;
use std::sync::Arc;
use std::sync::atomic::Ordering;

impl Gateway {
    pub(super) async fn subagent(&self, id: Thread, task: &str, opts: AgentOpts, ctx: ToolCtx) -> Result<String> {
        let n = self.subagents.fetch_add(1, Ordering::Relaxed) + 1;
        // A key of its own, so it starts fresh instead of resuming an older sub-agent's session.
        let key = format!("{}#agent-{}", id.key(), &crate::util::new_uuid()[..8]);
        let mut tools = self.tools().without(&opts.exclude);
        if let Some(only) = &opts.tools {
            tools = tools.only(only);
        }
        let provider = self.provider.read().unwrap().clone();
        let system = agent::system_prompt(&self.workspace, opts.system.as_deref().unwrap_or(""));
        let mut agent = Agent::new(provider, tools, system, self.db.clone(), &key)?;
        eprintln!("sub-agent #{n} starts: {}", task.chars().take(80).collect::<String>());
        agent.run_turn(task, &ctx, &mut |_| {}).await
    }

    /// Hands a message to the chat as if the user sent it: into the running turn, or as
    /// a new one.
    pub(super) async fn deliver(self: &Arc<Self>, channel: Arc<dyn Messenger>, id: Thread, text: &str) {
        let offered = match self.chat(&id).await {
            Ok(state) => state.inbox.offer(text),
            Err(e) => return eprintln!("gateway: {e:#}"),
        };
        if !offered {
            let chat = id.id.clone();
            if let Err(e) = self.turn(channel, id, &chat, text, Vec::new(), false).await {
                eprintln!("gateway: {e:#}");
            }
        }
    }
}
