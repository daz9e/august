//! Sub-agents for extensions (`ctx.agent`): a fresh conversation of their own in a chat,
//! with the chat's approvals, unattended, returning the final reply.

use super::Gateway;
use crate::agent::{self, Agent};
use crate::messengers::{Messenger, Thread};
use crate::extensions::AgentOpts;
use crate::tools::ToolCtx;
use anyhow::Result;
use std::sync::Arc;
use std::sync::atomic::Ordering;

impl Gateway {
    pub(super) async fn subagent(&self, channel: Arc<dyn Messenger>, id: Thread, task: &str, opts: AgentOpts) -> Result<String> {
        let n = self.subagents.fetch_add(1, Ordering::Relaxed) + 1;
        let key = format!("{}:{}#agent{n}", id.messenger, id.id);
        let mut tools = self.tools().without(&opts.exclude);
        if let Some(only) = &opts.tools {
            tools = tools.only(only);
        }
        let provider = self.provider.read().unwrap().clone();
        let system = agent::system_prompt(&self.workspace, opts.system.as_deref().unwrap_or(""));
        let mut agent = Agent::new(provider, tools, system, self.db.clone(), &key)?;
        let ctx = ToolCtx {
            workspace: self.workspace.clone(),
            approver: Arc::new(self.approver(channel, id.clone(), None)),
            db: self.db.clone(),
            origin: Some(id),
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
