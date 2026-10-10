//! What the agent needs from storage, so it can run against SQLite or a test double.

use crate::llm::Message;
use anyhow::Result;

pub trait SessionStore: Send + Sync {
    /// Latest session of a chat with its live messages, or a fresh session.
    fn resume_session(&self, chat_key: &str) -> Result<(String, Vec<Message>)>;
    fn new_session(&self, chat_key: &str) -> Result<String>;
    /// A session's live messages.
    fn live(&self, session: &str) -> Result<Vec<Message>>;
    /// Makes `session` the conversation of `chat_key`.
    fn bind(&self, chat_key: &str, session: &str) -> Result<()>;
    /// Merges `change` into a session's settings (a null field deletes it).
    fn update_session_settings(&self, session: &str, change: &serde_json::Value) -> Result<()>;
    /// A session's settings: `{model, system, tools}`, each optional.
    fn session_settings(&self, session: &str) -> Result<serde_json::Value>;
    fn append(&self, session: &str, msgs: &[Message], index: bool) -> Result<()>;
    /// Replaces the live messages (after a rollback or compaction); old ones stay searchable.
    fn replace_live(&self, session: &str, msgs: &[Message]) -> Result<()>;
    /// Appends to the journal; never fails the caller.
    fn journal(&self, entry: &crate::db::Entry);
}
