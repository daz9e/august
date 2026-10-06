//! Tools the agent can call, plus the approval hook for risky actions.

mod extensions;
mod fs;
mod media;
mod memory;
mod search;
mod shell;
mod skills;
mod tasks;
mod web;

pub use tasks::format_tasks;

use crate::db::Db;
use crate::extensions::Extensions;
use crate::llm::ToolSpec;
use anyhow::Result;
use async_trait::async_trait;
use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::Arc;

/// Asks the human whether a risky action may run (CLI prompt, Telegram button, ...).
#[async_trait]
pub trait Approver: Send + Sync {
    async fn approve(&self, action: &str) -> bool;
}

/// Delivers a file from the workspace to the chat the turn runs in.
#[async_trait]
pub trait FileSink: Send + Sync {
    async fn send_file(&self, path: &std::path::Path, caption: &str) -> Result<()>;
}

pub struct ToolCtx {
    pub workspace: PathBuf,
    pub approver: Arc<dyn Approver>,
    pub db: Arc<Db>,
    /// `(channel, chat)` the turn runs in; `None` in the terminal REPL.
    pub origin: Option<(String, String)>,
    /// Where `send_file` delivers files; `None` when there is no chat (terminal REPL).
    pub files: Option<Arc<dyn FileSink>>,
    /// Loaded extensions, for `save_extension`; `None` when they are off.
    pub extensions: Option<Arc<Extensions>>,
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
    /// Extension tools and the `tool_call` / `tool_result` hooks.
    ext: Option<Arc<Extensions>>,
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
                Box::new(media::SendFile),
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
                Box::new(extensions::SaveExtension),
            ],
            ext: None,
        }
    }

    pub fn with_extensions(mut self, ext: Arc<Extensions>) -> Self {
        self.ext = Some(ext);
        self
    }

    pub fn extensions(&self) -> Option<&Arc<Extensions>> {
        self.ext.as_ref()
    }

    pub fn builtin_names() -> Vec<&'static str> {
        Self::with_defaults().tools.iter().map(|t| t.name()).collect()
    }

    pub fn specs(&self) -> Vec<ToolSpec> {
        let mut specs: Vec<ToolSpec> = self
            .tools
            .iter()
            .map(|t| ToolSpec {
                name: t.name().to_string(),
                description: t.description().to_string(),
                input_schema: t.input_schema(),
            })
            .collect();
        if let Some(ext) = &self.ext {
            specs.extend(ext.tool_specs());
        }
        specs
    }

    /// Runs a tool through the extension hooks; errors become `(message, true)` so the
    /// model can react to them.
    pub async fn call(&self, name: &str, input: &Value, ctx: &ToolCtx) -> (String, bool) {
        let Some(ext) = &self.ext else {
            return self.run(name, input, ctx).await;
        };
        let mut input = input.clone();
        if ext.listens("tool_call") {
            let data = ext.emit("tool_call", json!({"tool": name, "input": input}), &ctx.origin).await;
            match &data["block"] {
                Value::String(reason) if !reason.is_empty() => {
                    return (format!("blocked by an extension: {reason}"), true);
                }
                Value::Bool(true) => return ("blocked by an extension".into(), true),
                _ => {}
            }
            input = data["input"].clone();
        }
        let (mut output, mut is_error) = self.run(name, &input, ctx).await;
        if ext.listens("tool_result") {
            let data = json!({"tool": name, "input": input, "output": output, "isError": is_error});
            let data = ext.emit("tool_result", data, &ctx.origin).await;
            if let Some(o) = data["output"].as_str() {
                output = o.to_string();
            }
            if let Some(e) = data["isError"].as_bool() {
                is_error = e;
            }
        }
        (output, is_error)
    }

    async fn run(&self, name: &str, input: &Value, ctx: &ToolCtx) -> (String, bool) {
        if let Some(tool) = self.tools.iter().find(|t| t.name() == name) {
            return match tool.call(input, ctx).await {
                Ok(out) => (out, false),
                Err(e) => (format!("error: {e:#}"), true),
            };
        }
        let found = match &self.ext {
            Some(ext) => ext.call_tool(name, input, &ctx.origin).await,
            None => None,
        };
        match found {
            Some(Ok(out)) => (out, false),
            Some(Err(e)) => (format!("error: {e}"), true),
            None => (format!("unknown tool: {name}"), true),
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
