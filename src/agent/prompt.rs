//! The system prompt: what the turn's hooks (`before_turn`) made of the conversation's own
//! base, then the extensions' sections (memory, skills, ...), fixed once per session so the
//! prefix stays cacheable.

use super::Agent;

impl Agent {
    /// System prompt for the next model call: the base prompt plus a snapshot of the
    /// extensions' sections. The snapshot is taken once per session
    /// (and again after the history is replaced), so the prompt prefix stays byte-identical and
    /// provider caching works; changes show up in the next session.
    pub(super) fn system_now(&mut self) -> String {
        if self.snapshot.is_none() {
            let extensions = self.tools.extensions().map(|e| e.prompt_sections()).unwrap_or_default();
            let own = self.settings["system"].as_str().map(|s| format!("\n\n{}", s.trim())).unwrap_or_default();
            self.snapshot = Some(own + &extensions);
        }
        self.turn_system.as_ref().unwrap_or(&self.system).clone() + self.snapshot.as_deref().unwrap_or_default()
    }
}
