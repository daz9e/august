//! The system prompt: the core's own instructions, then sections from the modules (memory,
//! skills) and extensions, fixed once per session so the prefix stays cacheable.

use super::Agent;

/// The core's part of the system prompt; `surface` says how the user reads replies.
pub fn system_prompt(workspace: &std::path::Path, surface: &str) -> String {
    format!(
        "You are August, a personal assistant agent running on the user's machine.\n\
         Your workspace directory is {ws}; shell commands run there and file paths are \
         relative to it.\n\
         Use tools to actually do things instead of describing how to do them. Keep replies \
         short. Reply in the user's language. Each user message starts with a [timestamp] in \
         the user's local time; it is added automatically, don't copy it into replies.\n{surface}",
        ws = workspace.display()
    )
}

impl Agent {
    /// System prompt for the next model call: the base prompt plus a snapshot of the
    /// sections (saved facts, skills, extensions'). The snapshot is taken once per session
    /// (and again after a compaction), so the prompt prefix stays byte-identical and
    /// provider caching works; changes show up in the next session.
    pub(super) fn system_now(&mut self) -> String {
        if self.snapshot.is_none() {
            let extensions = self.tools.extensions().map(|e| e.prompt_sections()).unwrap_or_default();
            self.snapshot = Some(memory_section(&*self.db) + &crate::skills::prompt_section() + &extensions);
        }
        self.turn_system.as_ref().unwrap_or(&self.system).clone() + self.snapshot.as_deref().unwrap_or_default()
    }
}

/// How memory works, and the facts saved so far.
fn memory_section(db: &dyn super::SessionStore) -> String {
    let mut s = String::from(
        "\n\n## Memory\nYou keep durable facts about the user with `remember`/`forget` and can \
         search every past conversation with `search_history`. Save facts proactively when the \
         user shares lasting preferences or details, and look things up instead of asking again.",
    );
    if let Ok(facts) = db.facts()
        && !facts.is_empty()
    {
        s += "\nFacts you saved earlier (delete outdated ones with `forget`; facts saved during this \
              conversation appear here from the next one):\n";
        for f in facts {
            s += &format!("- #{} {}\n", f.id, f.text);
        }
    }
    s
}
