//! A fork: a copy of a conversation that runs turns like any other and keeps nothing. It
//! starts from the same system prompt, history and tool list (so the provider's prompt cache
//! covers almost all of it), and goes through the same loop and hooks; what it adds is
//! dropped with it. Extensions use it to look back at a conversation (e.g. a review that
//! saves what is worth remembering).

use super::{Agent, SessionStore};
use crate::llm::Message;
use anyhow::Result;
use std::sync::Arc;

impl Agent {
    /// A copy of the conversation as it is now; nothing it does is stored.
    pub fn fork(&mut self) -> Agent {
        let system = self.system_now();
        Agent {
            provider: self.provider.clone(),
            tools: self.tools.clone(),
            system,
            turn_system: None,
            snapshot: Some(String::new()),
            history: self.history.clone(),
            db: Arc::new(Dropped(self.db.clone())),
            chat_key: self.chat_key.clone(),
            session: self.session.clone(),
            stored: self.history.len(),
            turn_start: self.history.len(),
            last_input_tokens: self.last_input_tokens,
            turn_calls: Vec::new(),
            settings: self.settings.clone(),
            session_provider: self.session_provider.clone(),
            turn_provider: None,
            starting: None,
        }
    }
}

/// A store that reads from the conversation's own and writes nowhere.
struct Dropped(Arc<dyn SessionStore>);

impl SessionStore for Dropped {
    fn resume_session(&self, chat_key: &str) -> Result<(String, Vec<Message>)> {
        self.0.resume_session(chat_key)
    }
    fn live(&self, session: &str) -> Result<Vec<Message>> {
        self.0.live(session)
    }
    fn bind(&self, _: &str, _: &str) -> Result<()> {
        Ok(())
    }
    fn update_session_settings(&self, _: &str, _: &serde_json::Value) -> Result<()> {
        Ok(())
    }
    fn session_settings(&self, session: &str) -> Result<serde_json::Value> {
        self.0.session_settings(session)
    }
    fn append(&self, _: &str, _: &[Message], _: bool) -> Result<()> {
        Ok(())
    }
    fn replace_live(&self, _: &str, _: &[Message]) -> Result<()> {
        Ok(())
    }
    fn journal(&self, _: &crate::db::Entry) {}
}
