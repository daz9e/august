//! Tools the agent can call, plus the approval hook for risky actions.

mod extensions;
mod fs;
mod media;
mod memory;
mod search;
mod shell;
mod skills;


use crate::db::Db;
use crate::extensions::Extensions;
use crate::llm::ToolSpec;
use anyhow::Result;
use async_trait::async_trait;
use serde_json::{Value, json};
use std::collections::HashSet;
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

#[derive(Clone)]
pub struct ToolCtx {
    pub workspace: PathBuf,
    pub approver: Arc<dyn Approver>,
    pub db: Arc<Db>,
    /// The thread and turn this runs for.
    pub origin: crate::extensions::Origin,
    /// Where `send_file` delivers files; `None` when there is no thread to send to.
    pub files: Option<Arc<dyn FileSink>>,
    /// Loaded extensions, for `save_extension`; `None` when they are off.
    pub extensions: Option<Arc<Extensions>>,
    /// Messages the user sends while the turn runs.
    pub inbox: Option<Arc<crate::agent::Inbox>>,
    /// Who asks for tools: `model`, or `ext:<name>` for an extension's `callTool`.
    pub caller: String,
}

#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &'static str;
    fn description(&self) -> &'static str;
    fn input_schema(&self) -> Value;
    async fn call(&self, input: &Value, ctx: &ToolCtx) -> Result<String>;
}

/// Approves everything (a `tool_call` hook already did).
struct Approved;

#[async_trait]
impl Approver for Approved {
    async fn approve(&self, _action: &str) -> bool {
        true
    }
}

#[derive(Clone)]
pub struct ToolRegistry {
    tools: Vec<Arc<dyn Tool>>,
    /// Extension tools and the `tool_call` / `tool_result` hooks.
    ext: Option<Arc<Extensions>>,
    /// Tools (of any kind) the model is not offered and may not call.
    hidden: HashSet<String>,
    /// When set, only these tools are offered.
    only: Option<HashSet<String>>,
}

impl ToolRegistry {
    pub fn with_defaults() -> Self {
        Self {
            tools: vec![
                Arc::new(shell::Shell),
                Arc::new(fs::ReadFile),
                Arc::new(fs::WriteFile),
                Arc::new(fs::EditFile),
                Arc::new(fs::ListDir),
                Arc::new(media::SendFile),
                Arc::new(search::Grep),
                Arc::new(search::Glob),
                Arc::new(memory::Remember),
                Arc::new(memory::Forget),
                Arc::new(memory::SearchHistory),
                Arc::new(skills::LoadSkill),
                Arc::new(skills::SaveSkill),
                Arc::new(skills::EditSkill),
                Arc::new(extensions::SaveExtension),
            ],
            ext: None,
            hidden: HashSet::new(),
            only: None,
        }
    }

    /// Hides tools by name, built-in or not (e.g. what a sub-agent may not use).
    pub fn without<S: AsRef<str>>(mut self, names: &[S]) -> Self {
        self.hidden.extend(names.iter().map(|n| n.as_ref().to_string()));
        self
    }

    /// Offers only these tools.
    pub fn only<S: AsRef<str>>(mut self, names: &[S]) -> Self {
        self.only = Some(names.iter().map(|n| n.as_ref().to_string()).collect());
        self
    }

    fn offered(&self, name: &str) -> bool {
        !self.hidden.contains(name) && self.only.as_ref().is_none_or(|o| o.contains(name))
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
        let ext = self.ext.as_ref().map(|e| e.tool_specs()).unwrap_or_default();
        let mut specs: Vec<ToolSpec> = self
            .tools
            .iter()
            .filter(|t| !ext.iter().any(|e| e.name == t.name())) // replaced by an extension
            .map(|t| ToolSpec {
                name: t.name().to_string(),
                description: t.description().to_string(),
                input_schema: t.input_schema(),
            })
            .collect();
        specs.extend(ext);
        let mut seen = HashSet::new();
        specs.retain(|s| self.offered(&s.name) && seen.insert(s.name.clone()));
        specs
    }

    /// Runs a tool through the extension hooks; errors become `(message, true)` so the
    /// model can react to them.
    /// `id` is the model's id of the call (`None` for calls from elsewhere).
    pub async fn call(&self, id: Option<&str>, name: &str, input: &Value, ctx: &ToolCtx) -> (String, bool) {
        let (output, is_error) = self.call_hooked(id, name, input, ctx).await;
        let mut e = crate::db::Entry::new("tool", json!({"tool": name, "id": id, "input": input, "output": output, "isError": is_error}));
        e.session = ctx.origin.thread.as_ref().and_then(|t| ctx.db.current_session(&t.key()).ok().flatten());
        e.turn = ctx.origin.turn.as_ref().map(|t| t.id);
        e.caller = Some(ctx.caller.clone());
        crate::agent::SessionStore::journal(&*ctx.db, &e);
        (output, is_error)
    }

    async fn call_hooked(&self, id: Option<&str>, name: &str, input: &Value, ctx: &ToolCtx) -> (String, bool) {
        if !self.offered(name) {
            return (format!("unknown tool: {name}"), true);
        }
        let Some(ext) = &self.ext else {
            return self.run(name, input, ctx).await;
        };
        let mut input = input.clone();
        let mut ctx = std::borrow::Cow::Borrowed(ctx);
        if ext.listens("tool_call") {
            let data = ext.emit("tool_call", json!({"tool": name, "input": input, "id": id, "caller": ctx.caller}), &ctx.origin).await;
            match &data["block"] {
                Value::String(reason) if !reason.is_empty() => {
                    return (format!("blocked by an extension: {reason}"), true);
                }
                Value::Bool(true) => return ("blocked by an extension".into(), true),
                _ => {}
            }
            input = data["input"].clone();
            // The hook may decide about approval: `approve: true` runs without asking,
            // `ask: "<question>"` asks first even where the tool wouldn't.
            if data["approve"] == true {
                ctx.to_mut().approver = Arc::new(Approved);
            } else if let Some(q) = data["ask"].as_str().filter(|q| !q.is_empty())
                && !ctx.approver.approve(q).await
            {
                return ("the user denied this".into(), true);
            }
        }
        let (mut output, mut is_error) = self.run(name, &input, &ctx).await;
        if ext.listens("tool_result") {
            let data = json!({"tool": name, "input": input, "id": id, "caller": ctx.caller, "output": output, "isError": is_error});
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
        if let Some(ext) = &self.ext
            && ext.has_tool(name)
            && let Some(r) = ext.call_tool(name, input, &ctx.origin).await
        {
            return match r {
                Ok(out) => (out, false),
                Err(e) => (format!("error: {e}"), true),
            };
        }
        match self.tools.iter().find(|t| t.name() == name) {
            Some(tool) => match tool.call(input, ctx).await {
                Ok(out) => (out, false),
                Err(e) => (format!("error: {e:#}"), true),
            },
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
