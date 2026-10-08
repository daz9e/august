//! Sub-agents (fresh turns): a conversation of their own for a thread, with the thread's
//! hooks, returning the final reply; and handing a thread a message.

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

    /// Hands the chat a message from `source` (`user`, or e.g. `ext:goal`) after the
    /// `message_in` hook. `deliver`: `steer` joins the running turn or starts one, `followUp`
    /// runs as its own turn after the current one, `nextTurn` waits for the next turn
    /// without starting one. The model sees who sent it unless it is the user.
    pub(super) async fn deliver(self: &Arc<Self>, channel: Arc<dyn Messenger>, id: Thread, text: &str, source: &str, deliver: &str) -> Result<()> {
        let data = serde_json::json!({"id": null, "text": text, "files": [], "source": source});
        let Some(text) = self.message_in(&channel, &id, data).await? else {
            return Ok(());
        };
        let text = if source == "user" { text } else { format!("[from {source}] {text}") };
        let state = self.chat(&id).await?;
        let chat = id.id.clone();
        match deliver {
            "nextTurn" => state.inbox.stash(&text),
            "followUp" => self.turn(channel, id, &chat, &text, Vec::new()).await?,
            _ if state.inbox.offer(&text) => {}
            _ => self.turn(channel, id, &chat, &text, Vec::new()).await?,
        }
        Ok(())
    }
}
