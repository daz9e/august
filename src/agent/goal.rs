//! Standing goals (`/goal`): after each turn a separate judging call decides whether
//! the goal is reached; if not, the chat keeps working on it.

use super::Agent;
use crate::llm::Message;
use anyhow::Result;

const JUDGE: &str = "You check whether an AI assistant has reached a goal the user set. You get \
    the goal and the assistant's latest reply. Answer `DONE` if the reply shows the goal is fully \
    achieved (or can't be achieved and the assistant explained why), otherwise `CONTINUE: ` and \
    one sentence on what is still missing. Output only that line.";

impl Agent {
    /// `None` when `goal` is reached, else what is still missing.
    pub async fn judge_goal(&mut self, goal: &str, reply: &str) -> Result<Option<String>> {
        let ask = vec![Message::user_text(format!("Goal:\n{goal}\n\nThe assistant's latest reply:\n{reply}"))];
        let c = self.provider.complete(&self.session, JUDGE, &ask, &[]).await?;
        self.record_usage(&c.usage);
        let verdict = c.message.text();
        let verdict = verdict.trim();
        if verdict.starts_with("DONE") {
            return Ok(None);
        }
        let missing = verdict.strip_prefix("CONTINUE").unwrap_or(verdict).trim_start_matches([':', ' ']).trim();
        Ok(Some(if missing.is_empty() { "the goal isn't reached yet".into() } else { missing.to_string() }))
    }
}
