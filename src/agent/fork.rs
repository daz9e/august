//! A fork: one more exchange on a copy of a conversation. It runs on the same system
//! prompt, history and tool list as the conversation (so the provider's prompt cache
//! covers almost all of it), may only call the tools it is allowed, and keeps nothing:
//! the conversation itself doesn't change. Extensions use it to look back at a
//! conversation (e.g. a review that saves what is worth remembering).

use super::Agent;
use crate::llm::{Block, LlmProvider, Message, Role, StopReason, ToolSpec};
use crate::tools::{ToolCtx, ToolRegistry};
use serde_json::{Value, json};
use std::sync::Arc;

/// Model calls one fork may make.
const MAX_STEPS: usize = 12;

pub struct Fork {
    provider: Arc<dyn LlmProvider>,
    session: String,
    system: String,
    history: Vec<Message>,
    specs: Vec<ToolSpec>,
    tools: ToolRegistry,
}

impl Agent {
    /// The conversation as it is now, to fork.
    pub fn fork(&mut self) -> Fork {
        Fork {
            provider: self.provider.clone(),
            session: self.session.clone(),
            system: self.system_now(),
            history: self.history.clone(),
            specs: self.tools.specs(),
            tools: self.tools.clone(),
        }
    }
}

impl Fork {
    /// Adds `task` as a user message and runs to the end; tools outside `allow` (when
    /// given) answer with an error. Returns the final reply and every tool call
    /// (`{name, input, output, isError}`).
    pub async fn run(mut self, task: &str, allow: Option<&[String]>, ctx: &ToolCtx) -> anyhow::Result<(String, Vec<Value>)> {
        self.history.push(Message::user_text(task));
        let mut calls = Vec::new();
        for _ in 0..MAX_STEPS {
            let completion = self.provider.complete(&self.session, &self.system, &self.history, &self.specs).await?;
            if let Err(e) = ctx.db.record_usage(&self.session, &completion.usage) {
                eprintln!("memory: could not record token usage: {e:#}");
            }
            let reply = completion.message;
            self.history.push(reply.clone());
            if !matches!(completion.stop_reason, StopReason::ToolUse) || reply.tool_uses().next().is_none() {
                return Ok((reply.text(), calls));
            }
            let mut results = Vec::new();
            for (id, name, input) in reply.tool_uses() {
                let (output, is_error) = if allow.is_none_or(|a| a.iter().any(|n| n == name)) {
                    self.tools.call(Some(id), name, input, ctx).await
                } else {
                    (format!("`{name}` is not available here"), true)
                };
                calls.push(json!({"name": name, "input": input, "output": output, "isError": is_error}));
                results.push(Block::ToolResult { tool_use_id: id.to_string(), content: output, is_error });
            }
            self.history.push(Message { role: Role::User, content: results });
        }
        Ok((self.history.last().map(Message::text).unwrap_or_default(), calls))
    }
}
