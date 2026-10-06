//! Learning after a turn: from time to time a background pass replays the conversation
//! and decides what is worth keeping — facts about the user (`remember`) and
//! procedures (`save_skill` / `edit_skill`). It runs after the reply was delivered, on
//! the same system prompt, history and tool list as the conversation, so the provider's
//! prompt cache covers almost all of it.

use super::Agent;
use crate::llm::{Block, LlmProvider, Message, Role, StopReason, ToolSpec};
use crate::tools::{Approver, ToolCtx, ToolRegistry};
use async_trait::async_trait;
use std::sync::{Arc, Mutex};

/// User turns between memory reviews (override: `AUGUST_REVIEW_MEMORY_EVERY`, 0 = off).
const MEMORY_EVERY: usize = 10;
/// Tool calls after which the skills are reviewed (override: `AUGUST_REVIEW_SKILLS_AFTER`, 0 = off).
const SKILLS_AFTER: usize = 15;
/// Model calls one review may make.
const MAX_STEPS: usize = 8;
/// What the review may run; other tools answer with an error.
const ALLOWED: &[&str] = &[
    "remember", "forget", "search_history", "load_skill", "save_skill", "edit_skill", "read_file", "list_dir", "grep", "glob",
];

const MEMORY_PROMPT: &str = "Memory: durable facts that matter in every future conversation — who \
    the user is, their preferences and corrections, their environment, people and projects. Save \
    with `remember` (one short self-contained sentence each); update or merge stale facts via \
    `replaces`; `forget` facts that turned out wrong. Skip temporary details, things easy to look \
    up again, and anything already in the Memory section.";

const SKILLS_PROMPT: &str = "Skills: how to do a class of task for this user — the steps in \
    order, the commands and tools that work, how the result should look, and the pitfalls that \
    cost time. Signals worth acting on: the user corrected your approach, format or tone; a \
    non-trivial technique, fix or workaround emerged; a skill you used was wrong, incomplete or \
    outdated. Prefer, in order: fixing a skill used in this conversation (`load_skill` it first, \
    then `edit_skill` patch), extending an existing skill that covers the class of task, adding \
    a supporting file to one, and only then a new skill (`save_skill`) named for the class of \
    task, never for today's incident. Write lessons, not logs: a pitfall is a general rule plus \
    why, with no dates, ticket numbers or quotes. Do not record environment problems the user \
    can fix (missing tools or keys) as rules, do not claim a tool \"doesn't work\", and do not \
    write up attempts that never worked as a recipe. Fix wrong text in place instead of \
    appending corrections. Built-in skills can't be changed.";

/// Counts turns and tool calls between reviews.
#[derive(Default)]
pub(super) struct Counters {
    turns: usize,
    tool_calls: usize,
}

fn setting(var: &str, default: usize) -> usize {
    std::env::var(var).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

impl Counters {
    /// Records a finished turn; returns `(review memory, review skills)`.
    pub(super) fn after_turn(&mut self, tool_calls: usize) -> (bool, bool) {
        self.turns += 1;
        self.tool_calls += tool_calls;
        let (every, after) = (setting("AUGUST_REVIEW_MEMORY_EVERY", MEMORY_EVERY), setting("AUGUST_REVIEW_SKILLS_AFTER", SKILLS_AFTER));
        let memory = every > 0 && self.turns >= every;
        let skills = after > 0 && self.tool_calls >= after;
        if memory {
            self.turns = 0;
        }
        if skills {
            self.tool_calls = 0;
        }
        (memory, skills)
    }
}

/// Nobody is there to ask in the background: writes go through, and the last
/// approved action is kept to describe the change.
#[derive(Default)]
struct Recorder(Mutex<Option<String>>);

#[async_trait]
impl Approver for Recorder {
    async fn approve(&self, action: &str) -> bool {
        *self.0.lock().unwrap() = Some(action.lines().next().unwrap_or(action).trim_end_matches(':').to_string());
        true
    }
}

pub(super) struct Review {
    provider: Arc<dyn LlmProvider>,
    session: String,
    system: String,
    history: Vec<Message>,
    specs: Vec<ToolSpec>,
    ctx: ToolCtx,
    recorder: Arc<Recorder>,
    memory: bool,
    skills: bool,
}

impl Agent {
    /// A review of the conversation as it is now, or `None` when none is due.
    pub(super) fn review_due(&mut self, ctx: &ToolCtx, tool_calls: usize) -> Option<Review> {
        if std::env::var("AUGUST_REVIEW").is_ok_and(|v| v == "off") {
            return None;
        }
        let (memory, skills) = self.review_counters.after_turn(tool_calls);
        if !memory && !skills {
            return None;
        }
        let recorder = Arc::new(Recorder::default());
        Some(Review {
            provider: self.provider.clone(),
            session: self.session.clone(),
            system: self.system_now(),
            history: self.history.clone(),
            specs: self.tools.specs(),
            ctx: ToolCtx {
                workspace: ctx.workspace.clone(),
                approver: recorder.clone(),
                db: ctx.db.clone(),
                origin: ctx.origin.clone(),
                files: None,
                extensions: None,
                review: false,
            },
            recorder,
            memory,
            skills,
        })
    }
}

impl Review {
    /// Runs the review to the end; returns what it changed.
    pub async fn run(mut self) -> anyhow::Result<Vec<String>> {
        let mut ask = String::from(
            "[Background review] The conversation above is over for now. Review it and save what \
             is worth keeping for future conversations; the user doesn't see this exchange. If \
             nothing stands out, answer \"Nothing to save.\" and stop.\n\n",
        );
        if self.memory {
            ask += MEMORY_PROMPT;
            ask += "\n\n";
        }
        if self.skills {
            ask += SKILLS_PROMPT;
        }
        self.history.push(Message::user_text(ask.trim_end()));
        let registry = ToolRegistry::with_defaults();
        let mut changes = Vec::new();
        for _ in 0..MAX_STEPS {
            let completion = self.provider.complete(&self.session, &self.system, &self.history, &self.specs).await?;
            let reply = completion.message;
            self.history.push(reply.clone());
            if !matches!(completion.stop_reason, StopReason::ToolUse) || reply.tool_uses().next().is_none() {
                break;
            }
            let mut results = Vec::new();
            for (id, name, input) in reply.tool_uses() {
                let (content, is_error) = if ALLOWED.contains(&name) {
                    let (out, err) = registry.call(name, input, &self.ctx).await;
                    let approved = self.recorder.0.lock().unwrap().take();
                    if !err {
                        match name {
                            "remember" => changes.push(format!("remembered: {}", input["fact"].as_str().unwrap_or(""))),
                            "forget" => changes.push(format!("forgot #{}", input["id"])),
                            _ => changes.extend(approved),
                        }
                    }
                    (out, err)
                } else {
                    (format!("`{name}` is not available during the review"), true)
                };
                results.push(Block::ToolResult { tool_use_id: id.to_string(), content, is_error });
            }
            self.history.push(Message { role: Role::User, content: results });
        }
        Ok(changes)
    }
}
