//! What the agent needs from storage, so it can run against SQLite or a test double.

use crate::db::Fact;
use crate::llm::Message;
use anyhow::Result;

pub trait SessionStore: Send + Sync {
    /// Latest session of a chat with its live messages, or a fresh session.
    fn resume_session(&self, chat_key: &str) -> Result<(String, Vec<Message>)>;
    fn new_session(&self, chat_key: &str) -> Result<String>;
    fn append(&self, session: &str, msgs: &[Message], index: bool) -> Result<()>;
    /// Replaces the live messages (after a rollback or compaction); old ones stay searchable.
    fn replace_live(&self, session: &str, msgs: &[Message]) -> Result<()>;
    /// Long-term facts shown in the system prompt.
    fn facts(&self) -> Result<Vec<Fact>>;
}
