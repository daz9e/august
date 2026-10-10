//! The agent loop: model -> tools -> model ... until the model stops calling tools.

mod fork;
mod inbox;
mod prompt;
mod store;

pub use inbox::Inbox;
pub use store::SessionStore;

use crate::extensions::Origin;
use crate::llm::{Block, Completion, LlmProvider, Message, Role, StopReason, ToolSpec};
use crate::tools::{ToolCtx, ToolRegistry};
use anyhow::Result;
use serde_json::{Value, json};
use std::sync::Arc;

/// `llm_result`'s data for a completion: `step` of a turn's (or a fork's) loop, `None` for a
/// single call; `session`: the conversation it is counted in.
pub fn llm_result(step: Option<usize>, session: Option<&str>, completion: &Completion) -> Value {
    let u = &completion.usage;
    let calls: Vec<Value> = completion.message.tool_uses().map(|(_, name, input)| json!({"name": name, "input": input})).collect();
    json!({
        "step": step,
        "session": session,
        "text": completion.message.text(),
        "toolCalls": calls,
        "usage": {"inputTokens": u.input_tokens, "outputTokens": u.output_tokens,
                  "cacheReadTokens": u.cache_read_tokens, "cacheWriteTokens": u.cache_write_tokens},
    })
}

/// Which conversation a turn runs in: the thread's own (`Thread`), a copy of it that is
/// dropped afterwards (`Copy`), or a new one of its own (`New`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Conversation {
    #[default]
    Thread,
    Copy,
    New,
}

/// The turn a hook, tool or command runs in.
#[derive(Debug, Clone, serde::Serialize)]
pub struct TurnTag {
    pub id: u64,
    pub conversation: Conversation,
    /// Whether it is shown in its thread as it runs.
    pub show: bool,
    /// Who started it, when not the user (an extension's name, `scheduler`, ...).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent: Option<u64>,
    /// What the starter attached for hooks to read; the core doesn't look inside.
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub meta: Value,
    /// When set, only these tools may be called in it (the others stay offered).
    #[serde(skip)]
    pub callable: Option<Arc<std::collections::HashSet<String>>>,
}

impl TurnTag {
    /// A turn of the user's: in the thread's conversation, shown there (its id comes when
    /// it is registered).
    pub fn shown() -> Self {
        Self { id: 0, conversation: Conversation::Thread, show: true, source: None, parent: None, meta: Value::Null, callable: None }
    }
}

pub enum Event<'a> {
    /// A fragment of the model's reply, as it streams in.
    Text(&'a str),
    /// A new model call starts after tool results (text from before it is complete).
    Step,
    ToolCall { name: &'a str, input: &'a Value },
    /// A line an extension asked to show (e.g. after it rewrote the history).
    Note(&'a str),
}

pub struct Agent {
    provider: Arc<dyn LlmProvider>,
    tools: ToolRegistry,
    system: String,
    /// The system prompt `before_turn` hooks set for the running (or latest) turn.
    turn_system: Option<String>,
    /// The session's own and the extensions' prompt sections, fixed for the session.
    snapshot: Option<String>,
    history: Vec<Message>,
    db: Arc<dyn SessionStore>,
    /// Which chat this is (`telegram:123`, `cli`); sessions are looked up by it.
    chat_key: String,
    /// Current DB session; also the conversation id sent to the provider.
    session: String,
    /// How many messages of `history` are already in the DB.
    stored: usize,
    /// Where the running turn began; a failed or cancelled turn is cut back to it.
    turn_start: usize,
    /// Input tokens the provider reported for the latest call.
    last_input_tokens: usize,
    /// Tool calls the running turn made: `{name, input, output, isError}`.
    turn_calls: Vec<Value>,
    /// The session's settings as of the running turn: `{model, system, tools}`.
    settings: Value,
    /// The provider for the session's own `model`, if it has one.
    session_provider: Option<Arc<dyn LlmProvider>>,
    /// The one an `llm_error` handler switched the running turn to.
    turn_provider: Option<Arc<dyn LlmProvider>>,
    /// A session that hasn't had its first turn: why it started and the one before it, for
    /// `session_start`.
    starting: Option<(&'static str, Option<String>)>,
}

impl Agent {
    /// Continues the chat's latest stored session, or starts one.
    pub fn new(
        provider: Arc<dyn LlmProvider>,
        tools: ToolRegistry,
        system: String,
        db: Arc<dyn SessionStore>,
        chat_key: &str,
    ) -> Result<Self> {
        let (session, history) = db.resume_session(chat_key)?;
        let stored = history.len();
        Ok(Self {
            provider,
            tools,
            system,
            turn_system: None,
            snapshot: None,
            history,
            db,
            chat_key: chat_key.to_string(),
            session,
            stored,
            turn_start: stored,
            last_input_tokens: 0,
            turn_calls: Vec::new(),
            settings: json!({}),
            session_provider: None,
            turn_provider: None,
            starting: (stored == 0).then_some(("start", None)),
        })
    }

    /// Tool calls the latest turn made: `{name, input, output, isError}`.
    pub fn tool_calls(&self) -> Vec<Value> {
        self.turn_calls.clone()
    }

    pub fn set_provider(&mut self, provider: Arc<dyn LlmProvider>) {
        self.provider = provider;
    }

    /// Starts a new conversation; the old one stays searchable.
    /// Starts `session`, created for this chat and still empty, as its conversation.
    pub fn begin(&mut self, session: String) {
        let previous = std::mem::replace(&mut self.session, session);
        self.log("session", json!({"reason": "new", "previous": previous}), None);
        self.notify_ext("session_changed", json!({"reason": "new", "previous": previous, "session": self.session}), self.chat_ref());
        self.starting = Some(("new", Some(previous)));
        self.history.clear();
        self.snapshot = None;
        self.stored = 0;
        self.turn_start = 0;
        self.last_input_tokens = 0;
    }

    /// Records `kind` in this session's journal, for turn `turn` if inside one.
    fn log(&self, kind: &str, data: Value, turn: Option<&TurnTag>) {
        let mut e = crate::db::Entry::new(kind, data);
        e.session = Some(self.session.clone());
        e.turn = turn.map(|t| t.id);
        e.source = turn.map(|t| t.source.clone().unwrap_or_else(|| "user".into()));
        self.db.journal(&e);
    }

    /// Continues another stored session in this chat (the chat is bound to it from now on).
    pub fn switch(&mut self, session: &str) -> Result<()> {
        let history = self.db.live(session)?;
        self.db.bind(&self.chat_key, session)?;
        let previous = std::mem::replace(&mut self.session, session.to_string());
        self.log("session", json!({"reason": "switch", "previous": previous}), None);
        self.notify_ext("session_changed", json!({"reason": "switch", "previous": previous, "session": session}), self.chat_ref());
        self.stored = history.len();
        self.turn_start = history.len();
        self.history = history;
        self.snapshot = None;
        self.last_input_tokens = 0;
        Ok(())
    }

    pub fn session(&self) -> &str {
        &self.session
    }

    /// Before a session's first turn: `session_start` may set its settings (`{model, system,
    /// tools}`), e.g. pick a model for conversations from one messenger.
    async fn session_start(&mut self, ctx: &ToolCtx) -> Result<()> {
        let Some((reason, previous)) = self.starting.take() else {
            return Ok(());
        };
        let Some(ext) = self.tools.extensions().filter(|e| e.listens("session_start")).cloned() else {
            return Ok(());
        };
        let data = json!({"session": self.session, "previous": previous, "reason": reason, "chat": self.chat_key});
        let data = ext.emit("session_start", data, &ctx.origin).await;
        let change: serde_json::Map<String, Value> =
            ["model", "system", "tools"].into_iter().filter_map(|k| data.get(k).filter(|v| !v.is_null()).map(|v| (k.to_string(), v.clone()))).collect();
        if !change.is_empty() {
            self.db.update_session_settings(&self.session, &Value::Object(change))?;
        }
        Ok(())
    }

    /// Picks up the session's settings; a change takes effect from this turn.
    async fn load_settings(&mut self) -> Result<()> {
        let settings = self.db.session_settings(&self.session)?;
        if settings == self.settings {
            return Ok(());
        }
        self.session_provider = match settings["model"].as_str() {
            Some(m) => Some(self.build_spec(m)?),
            None => None,
        };
        self.settings = settings;
        self.snapshot = None;
        Ok(())
    }

    /// A provider for model `spec` (`provider:model`, or a model of the active provider).
    fn build_spec(&self, spec: &str) -> Result<Arc<dyn LlmProvider>> {
        let ext = self.tools.extensions().ok_or_else(|| anyhow::anyhow!("no extensions offer models"))?;
        crate::llm::providers::build_spec(ext, spec)
    }

    /// The model this session talks to.
    fn provider(&self) -> Arc<dyn LlmProvider> {
        self.turn_provider.clone().or_else(|| self.session_provider.clone()).unwrap_or_else(|| self.provider.clone())
    }

    /// The tools offered in this session (all, or the ones its settings name).
    fn specs(&self) -> Vec<ToolSpec> {
        let mut specs = self.tools.specs();
        if let Some(keep) = self.settings["tools"].as_array() {
            specs.retain(|s| keep.iter().any(|k| k == s.name.as_str()));
        }
        specs
    }

    /// The thread for hooks outside a turn: `telegram:5#task1` -> `telegram:5`.
    fn chat_ref(&self) -> Origin {
        let thread = self.chat_key.split_once(':').map(|(m, id)| crate::messengers::Thread::new(m, id.split('#').next().unwrap_or(id)));
        Origin { thread, ..Default::default() }
    }

    /// Fires an observe-only extension event in the background.
    fn notify_ext(&self, event: &'static str, data: Value, chat: Origin) {
        if let Some(ext) = self.tools.extensions().filter(|e| e.listens(event)).cloned() {
            tokio::spawn(async move { ext.emit(event, data, &chat).await });
        }
    }

    /// The live history (what the next model call sees), and the context it takes: input
    /// tokens the provider last reported (0: not yet) and the model's window, if known.
    pub fn conversation(&self) -> (&[Message], usize, Option<usize>) {
        (&self.history, self.last_input_tokens, self.provider().context_window())
    }

    /// Replaces the live history (stored too; earlier messages stay searchable). The running
    /// turn's rollback point stays on the same message counted from the end.
    pub fn set_history(&mut self, history: Vec<Message>) -> Result<()> {
        let since_turn = self.history.len().saturating_sub(self.turn_start);
        self.turn_start = history.len().saturating_sub(since_turn);
        self.history = history;
        self.db.replace_live(&self.session, &self.history)?;
        self.stored = self.history.len();
        self.last_input_tokens = 0;
        self.snapshot = None; // the cached prefix is gone anyway; pick up what changed
        Ok(())
    }

    /// One model call with the `llm_call` / `context` / `llm_result` hooks; `step` counts
    /// from 0, `error`: why the previous try of this step failed.
    async fn call_model(
        &mut self,
        step: usize,
        specs: &[ToolSpec],
        ctx: &ToolCtx,
        error: Option<&Value>,
        on_event: &mut (dyn FnMut(Event) + Send),
    ) -> Result<Completion> {
        let mut system = self.system_now();
        let mut provider = self.provider();
        let mut specs = std::borrow::Cow::Borrowed(specs);
        if let Some(ext) = self.tools.extensions().filter(|e| e.listens("llm_call")) {
            let names: Vec<&str> = specs.iter().map(|s| s.name.as_str()).collect();
            let model = provider.name().to_string();
            let data = json!({"step": step, "system": system, "model": model, "tools": names});
            let data = ext.emit("llm_call", data, &ctx.origin).await;
            if let Some(s) = data["system"].as_str() {
                system = s.to_string();
            }
            if let Some(keep) = data["tools"].as_array() {
                specs.to_mut().retain(|s| keep.iter().any(|k| k == s.name.as_str()));
            }
            if let Some(m) = data["model"].as_str().filter(|m| *m != model) {
                // ponytail: builds the provider on every such call; cache by spec if it costs.
                provider = self.build_spec(m)?;
            }
            let effort = data["effort"].as_str();
            if (effort.is_some() || data["options"].is_object()) && let Some(p) = provider.tuned(effort, &data["options"]) {
                provider = p;
            }
        }
        // A `context` hook may change what the model sees for this one call (`messages`), or
        // the history itself (`history`, kept from then on), with a `note` to show.
        let mut rewritten = None;
        if let Some(ext) = self.tools.extensions().filter(|e| e.listens("context")).cloned() {
            let messages: Vec<Value> = self.history.iter().map(Message::to_json).collect();
            let data = json!({
                "step": step, "messages": messages, "system": system, "tokens": self.last_input_tokens,
                "window": provider.context_window(), "error": error,
            });
            let data = ext.emit("context", data, &ctx.origin).await;
            let parse = |key: &str| -> Option<Vec<Message>> {
                let list = data[key].as_array()?;
                let parsed: Option<Vec<Message>> = list.iter().map(Message::from_json).collect();
                if parsed.is_none() {
                    eprintln!("context hook returned malformed `{key}`; ignored");
                }
                parsed.filter(|_| list != &messages)
            };
            if let Some(history) = parse("history") {
                self.set_history(history)?;
                if let Some(note) = data["note"].as_str() {
                    on_event(Event::Note(note));
                }
            }
            rewritten = parse("messages");
        }
        let messages = rewritten.as_deref().unwrap_or(&self.history);
        let completion = {
            let mut on_text = |t: &str| on_event(Event::Text(t));
            provider.complete_stream(&self.session, &system, messages, &specs, &mut on_text).await?
        };
        self.last_input_tokens = completion.usage.context_tokens() as usize;
        let data = llm_result(Some(step), Some(&self.session), &completion);
        self.log("assistant", data.clone(), ctx.origin.turn.as_ref());
        self.notify_ext("llm_result", data, ctx.origin.clone());
        Ok(completion)
    }

    /// A model call; when it fails, `llm_error` handlers may have it tried again (after
    /// `delayMs`, on another `model`), e.g. once they freed up context or the service is back.
    async fn call_with_retries(
        &mut self,
        step: usize,
        specs: &[ToolSpec],
        ctx: &ToolCtx,
        on_event: &mut (dyn FnMut(Event) + Send),
    ) -> Result<Completion> {
        let mut error: Option<Value> = None;
        for attempt in 1.. {
            // Whether the failed try already showed part of a reply: another one repeats it.
            let mut streamed = false;
            let mut on_try = |e: Event| {
                streamed |= matches!(e, Event::Text(_));
                on_event(e)
            };
            let e = match self.call_model(step, specs, ctx, error.as_ref(), &mut on_try).await {
                Ok(c) => return Ok(c),
                Err(e) => e,
            };
            let Some(ext) = self.tools.extensions().filter(|x| x.listens("llm_error")).cloned() else { return Err(e) };
            let kind = crate::llm::error::ErrorKind::of(&e).as_str();
            let failed = json!({"kind": kind, "message": format!("{e:#}")});
            let data = json!({"step": step, "attempt": attempt, "model": self.provider().name(), "error": failed, "streamed": streamed});
            let data = ext.emit("llm_error", data, &ctx.origin).await;
            if data["retry"] != true {
                return Err(e);
            }
            if let Some(ms) = data["delayMs"].as_u64() {
                tokio::time::sleep(std::time::Duration::from_millis(ms)).await;
            }
            if let Some(m) = data["model"].as_str().filter(|m| *m != self.provider().name()) {
                self.turn_provider = Some(self.build_spec(m)?);
            }
            error = Some(failed);
        }
        unreachable!("the loop returns")
    }

    /// Drops what a failed or cancelled turn added, so the history stays consistent.
    pub fn rollback_turn(&mut self) {
        self.history.truncate(self.turn_start);
        // A compaction inside the turn can leave a half-finished tool exchange at the end.
        while self.history.last().is_some_and(|m| {
            m.tool_uses().next().is_some() || m.content.iter().any(|b| matches!(b, Block::ToolResult { .. }))
        }) {
            self.history.pop();
        }
        if self.stored > self.history.len() {
            if self.db.replace_live(&self.session, &self.history).is_err() {
                eprintln!("memory: could not resync the session after a rollback");
            }
            self.stored = self.history.len();
        }
    }

    fn persist(&mut self) {
        if self.stored >= self.history.len() {
            return;
        }
        match self.db.append(&self.session, &self.history[self.stored..], true) {
            Ok(()) => self.stored = self.history.len(),
            Err(e) => eprintln!("memory: could not save the conversation: {e:#}"),
        }
    }

    /// Runs one turn to completion and returns the final assistant text; `attachments`
    /// (images) go after the user's text. History is append-only; on error the whole turn is
    /// rolled back.
    pub async fn run_turn_with(
        &mut self,
        user_text: &str,
        attachments: Vec<Block>,
        ctx: &ToolCtx,
        on_event: &mut (dyn FnMut(Event) + Send),
    ) -> Result<String> {
        self.session_start(ctx).await?;
        self.load_settings().await?;
        self.turn_start = self.history.len();
        self.turn_system = None;
        self.turn_provider = None;
        self.turn_calls.clear();
        let ext = self.tools.extensions().cloned();
        let mut text = user_text.to_string();
        if let Some(ext) = &ext
            && ext.listens("before_turn")
        {
            let data = ext.emit("before_turn", json!({"text": text, "system": self.system}), &ctx.origin).await;
            if let Some(t) = data["text"].as_str() {
                text = t.to_string();
            }
            if let Some(s) = data["system"].as_str().filter(|s| *s != self.system) {
                self.turn_system = Some(s.to_string());
            }
        }
        self.log("user_message", json!({"text": text}), ctx.origin.turn.as_ref());
        // `turn_system` stays: a fork after the turn talks with the prompt it ended on.
        let result = self.run_turn_inner(&text, attachments, ctx, on_event).await;
        match &result {
            Ok(_) => {
                self.persist();
                // A turn cancelled before it ever ran must not cut this one back.
                self.turn_start = self.history.len();
            }
            Err(_) => self.rollback_turn(),
        }
        result
    }

    async fn run_turn_inner(
        &mut self,
        user_text: &str,
        attachments: Vec<Block>,
        ctx: &ToolCtx,
        on_event: &mut (dyn FnMut(Event) + Send),
    ) -> Result<String> {
        let mut user = Message::user_text(user_text);
        user.content.extend(attachments);
        self.history.push(user);
        let specs = self.specs();

        // How many steps a turn may take is the hooks' business (`llm_call` can take the tools
        // away); the loop ends when the model stops calling tools.
        for step in 0.. {
            if step > 0 {
                on_event(Event::Step);
                // Messages the user sent meanwhile join the tool results.
                let news = ctx.inbox.as_ref().map(|i| i.take()).unwrap_or_default();
                if !news.is_empty()
                    && let Some(last) = self.history.last_mut()
                {
                    last.content.push(Block::Text(news.join("\n")));
                }
            }
            let completion = self.call_with_retries(step, &specs, ctx, on_event).await?;
            let reply = completion.message;
            self.history.push(reply.clone());

            match completion.stop_reason {
                StopReason::ToolUse if reply.tool_uses().next().is_some() => {}
                StopReason::Refusal => return Ok("[the model refused to answer]".into()),
                StopReason::MaxTokens => {
                    return Ok(format!("{}\n[reply truncated at max_tokens]", reply.text()));
                }
                _ => return Ok(reply.text()),
            }

            // Run the requested tools concurrently; results go back in call order.
            let calls: Vec<_> = reply.tool_uses().collect();
            for (_, name, input) in &calls {
                on_event(Event::ToolCall { name, input });
            }
            let outputs =
                futures_util::future::join_all(calls.iter().map(|(id, name, input)| self.tools.call(Some(id), name, input, ctx))).await;
            let mut results = Vec::new();
            for ((id, name, input), (output, is_error)) in calls.iter().zip(outputs) {
                self.turn_calls.push(json!({"name": name, "input": input, "output": output, "isError": is_error}));
                results.push(Block::ToolResult {
                    tool_use_id: id.to_string(),
                    content: output,
                    is_error,
                });
            }
            self.history.push(Message {
                role: Role::User,
                content: results,
            });
        }
        unreachable!("the loop returns")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Db;
    use crate::llm::{Completion, ToolSpec};
    use async_trait::async_trait;

    /// Replies "summary" to everything and records nothing else.
    struct Fake;

    #[async_trait]
    impl LlmProvider for Fake {
        fn name(&self) -> &str {
            "fake"
        }
        async fn complete(&self, _: &str, _: &str, _: &[Message], _: &[ToolSpec]) -> Result<Completion> {
            Ok(Completion {
                message: Message { role: Role::Assistant, content: vec![Block::Text("summary".into())] },
                stop_reason: StopReason::EndTurn,
                usage: crate::llm::Usage::default(),
            })
        }
    }

    fn agent(db: Arc<Db>) -> Agent {
        Agent::new(Arc::new(Fake), ToolRegistry::default(), "sys".into(), db, "test").unwrap()
    }

    #[test]
    fn rollback_drops_dangling_tool_calls() {
        let mut a = agent(Db::in_memory());
        a.history.push(Message::user_text("old"));
        a.history.push(Message { role: Role::Assistant, content: vec![Block::Text("ok".into())] });
        a.turn_start = 2;
        a.history.push(Message::user_text("new"));
        a.history.push(Message {
            role: Role::Assistant,
            content: vec![Block::ToolUse { id: "1".into(), name: "bash".into(), input: Value::Null }],
        });
        a.rollback_turn();
        assert_eq!(a.history.len(), 2);
    }
}
