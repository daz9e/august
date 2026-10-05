//! Tools the agent can call, plus the approval hook for risky actions.

mod fs;
mod memory;
mod search;
mod shell;
mod skills;
mod tasks;
mod web;

pub use tasks::format_tasks;

use crate::db::Db;
use crate::llm::ToolSpec;
use anyhow::Result;
use async_trait::async_trait;
use serde_json::Value;
use std::path::PathBuf;
use std::sync::Arc;

/// Asks the human whether a risky action may run (CLI prompt, Telegram button, ...).
#[async_trait]
pub trait Approver: Send + Sync {
    async fn approve(&self, action: &str) -> bool;
}

pub struct ToolCtx {
    pub workspace: PathBuf,
    pub approver: Arc<dyn Approver>,
    pub db: Arc<Db>,
    /// `(channel, chat)` the turn runs in; `None` in the terminal REPL.
    pub origin: Option<(String, String)>,
}

#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &'static str;
    fn description(&self) -> &'static str;
    fn input_schema(&self) -> Value;
    async fn call(&self, input: &Value, ctx: &ToolCtx) -> Result<String>;
}

pub struct ToolRegistry {
    tools: Vec<Box<dyn Tool>>,
}

impl ToolRegistry {
    pub fn with_defaults() -> Self {
        Self {
            tools: vec![
                Box::new(shell::Shell),
                Box::new(fs::ReadFile),
                Box::new(fs::WriteFile),
                Box::new(fs::EditFile),
                Box::new(fs::ListDir),
                Box::new(search::Grep),
                Box::new(search::Glob),
                Box::new(web::WebFetch),
                Box::new(web::WebSearch),
                Box::new(memory::Remember),
                Box::new(memory::Forget),
                Box::new(memory::SearchHistory),
                Box::new(skills::LoadSkill),
                Box::new(skills::SaveSkill),
                Box::new(tasks::ScheduleTask),
                Box::new(tasks::ListTasks),
                Box::new(tasks::CancelTask),
            ],
        }
    }

    pub fn specs(&self) -> Vec<ToolSpec> {
        self.tools
            .iter()
            .map(|t| ToolSpec {
                name: t.name().to_string(),
                description: t.description().to_string(),
                input_schema: t.input_schema(),
            })
            .collect()
    }

    /// Runs a tool; errors become `(message, true)` so the model can react to them.
    pub async fn call(&self, name: &str, input: &Value, ctx: &ToolCtx) -> (String, bool) {
        let Some(tool) = self.tools.iter().find(|t| t.name() == name) else {
            return (format!("unknown tool: {name}"), true);
        };
        match tool.call(input, ctx).await {
            Ok(out) => (out, false),
            Err(e) => (format!("error: {e:#}"), true),
        }
    }
}

pub(crate) fn str_arg<'a>(input: &'a Value, key: &str) -> Result<&'a str> {
    input[key]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("missing string argument `{key}`"))
}

pub(crate) fn truncate(mut s: String, max: usize) -> String {
    if s.len() > max {
        let mut cut = max;
        while !s.is_char_boundary(cut) {
            cut -= 1;
        }
        let dropped = s.len() - cut;
        s.truncate(cut);
        s.push_str(&format!("\n... [truncated {dropped} bytes]"));
    }
    s
}
