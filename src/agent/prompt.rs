//! The system prompt: static instructions plus per-call facts and skills.

use super::Agent;

/// System prompt; `surface` describes where the user is reading replies.
pub fn system_prompt(workspace: &std::path::Path, surface: &str) -> String {
    format!(
        "You are August, a personal assistant agent running on the user's machine.\n\
         Your workspace directory is {ws}; shell commands run there and file paths are \
         relative to it.\n\
         Use tools to actually do things instead of describing how to do them. Keep replies \
         short. Reply in the user's language.\n\
         \n\
         Memory: you keep durable facts about the user with `remember`/`forget` (they appear \
         under Memory below) and can search every past conversation with `search_history`. \
         Save facts proactively when the user shares lasting preferences or details, and look \
         things up instead of asking again. Each user message starts with a [timestamp] in the \
         user's local time; it is added automatically, don't copy it into replies.\n\
         Tasks: use `schedule_task` for reminders and recurring jobs.\n\
         Skills: after solving a non-trivial, repeatable task, offer to save the procedure with \
         `save_skill`.\n{surface}",
        ws = workspace.display()
    )
}

impl Agent {
    /// System prompt plus what changes between calls: saved facts and available skills.
    pub(super) fn system_now(&self) -> String {
        let mut s = self.system.clone();
        match self.db.facts() {
            Ok(facts) if !facts.is_empty() => {
                s += "\n\n## Memory\nFacts you saved earlier (delete outdated ones with `forget`):\n";
                for f in facts {
                    s += &format!("- #{} {}\n", f.id, f.text);
                }
            }
            _ => {}
        }
        s + &crate::skills::prompt_section()
    }
}
