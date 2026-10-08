//! What the agent needs from storage, so it can run against SQLite or a test double.

use crate::db::Fact;
use crate::llm::{Message, Usage};
use anyhow::Result;

pub trait SessionStore: Send + Sync {
    /// Latest session of a chat with its live messages, or a fresh session.
    fn resume_session(&self, chat_key: &str) -> Result<(String, Vec<Message>)>;
    fn new_session(&self, chat_key: &str) -> Result<String>;
    /// A session's live messages.
    fn live(&self, session: &str) -> Result<Vec<Message>>;
    /// Makes `session` the conversation of `chat_key`.
    fn bind(&self, chat_key: &str, session: &str) -> Result<()>;
    /// A session's settings: `{model, system, tools}`, each optional.
    fn session_settings(&self, session: &str) -> Result<serde_json::Value>;
    fn append(&self, session: &str, msgs: &[Message], index: bool) -> Result<()>;
    /// Replaces the live messages (after a rollback or compaction); old ones stay searchable.
    fn replace_live(&self, session: &str, msgs: &[Message]) -> Result<()>;
    /// Long-term facts shown in the system prompt.
    fn facts(&self) -> Result<Vec<Fact>>;
    /// Tokens of one model call made in the session.
    fn record_usage(&self, session: &str, usage: &Usage) -> Result<()>;
}
