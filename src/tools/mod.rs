//! Tools the agent can call: all of them come from extensions (the default `tools` one has
//! `bash`, `read`, `write`, `edit`) and run through the `tool_call` / `tool_result` hooks.

use crate::db::Db;
use crate::extensions::Extensions;
use crate::llm::ToolSpec;
use serde_json::{Value, json};
use std::collections::HashSet;
use std::sync::Arc;

#[derive(Clone)]
pub struct ToolCtx {
    pub db: Arc<Db>,
    /// The thread and turn this runs for.
    pub origin: crate::extensions::Origin,
    /// Messages the user sends while the turn runs.
    pub inbox: Option<Arc<crate::agent::Inbox>>,
    /// Who asks for tools: `model`, or `ext:<name>` for an extension's `callTool`.
    pub caller: String,
}

#[derive(Clone, Default)]
pub struct ToolRegistry {
    /// Extension tools and the `tool_call` / `tool_result` hooks.
    ext: Option<Arc<Extensions>>,
    /// Tools the model is not offered and may not call.
    hidden: HashSet<String>,
    /// When set, only these tools are offered.
    only: Option<HashSet<String>>,
}

impl ToolRegistry {
    /// Hides tools by name (e.g. what a sub-agent may not use).
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

    pub fn specs(&self) -> Vec<ToolSpec> {
        let mut specs = self.ext.as_ref().map(|e| e.tool_specs()).unwrap_or_default();
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
        if ctx.origin.turn.as_ref().and_then(|t| t.callable.as_ref()).is_some_and(|c| !c.contains(name)) {
            return (format!("`{name}` is not available here"), true);
        }
        let Some(ext) = self.ext.as_ref().filter(|_| self.offered(name)) else {
            return (format!("unknown tool: {name}"), true);
        };
        let mut input = input.clone();
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
        }
        let (mut output, mut is_error) = match ext.has_tool(name).then(|| ext.call_tool(name, &input, &ctx.origin)) {
            Some(call) => match call.await {
                Some(Ok(out)) => (out, false),
                Some(Err(e)) => (format!("error: {e}"), true),
                None => (format!("unknown tool: {name}"), true),
            },
            None => (format!("unknown tool: {name}"), true),
        };
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
}
