//! The system prompt: the core's own instructions, then the extensions' sections (memory,
//! skills, ...), fixed once per session so the prefix stays cacheable.

use super::Agent;

/// The core's part of the system prompt; `surface` says how the user reads replies.
pub fn system_prompt(workspace: &std::path::Path, surface: &str) -> String {
    format!(
        "You are August, a personal assistant agent running on the user's machine.\n\
         Your workspace directory is {ws}; bash commands run there and file paths are \
         relative to it.\n\
         Use tools to actually do things instead of describing how to do them. Keep replies \
         short. Reply in the user's language. Each user message starts with a [timestamp] in \
         the user's local time; it is added automatically, don't copy it into replies.\n{surface}",
        ws = workspace.display()
    )
}

impl Agent {
    /// System prompt for the next model call: the base prompt plus a snapshot of the
    /// extensions' sections. The snapshot is taken once per session
    /// (and again after a compaction), so the prompt prefix stays byte-identical and
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
