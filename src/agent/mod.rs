//! The agent loop: model -> tools -> model ... until the model stops calling tools.

mod compaction;
mod inbox;
mod prompt;
mod review;
mod store;

pub use inbox::Inbox;
pub use prompt::system_prompt;
pub use store::SessionStore;
use compaction::DEFAULT_CONTEXT_TOKENS;

use crate::extensions::Origin;
use crate::llm::{Block, Completion, LlmProvider, Message, Role, StopReason, ToolSpec, Usage};
use crate::tools::{ToolCtx, ToolRegistry};
use anyhow::Result;
use serde_json::{Value, json};
use std::sync::Arc;

/// Model calls one turn may make (override: `AUGUST_MAX_STEPS`).
const MAX_STEPS: usize = 150;

fn max_steps() -> usize {
    std::env::var("AUGUST_MAX_STEPS").ok().and_then(|v| v.parse().ok()).filter(|n| *n > 0).unwrap_or(MAX_STEPS)
}
pub enum Event<'a> {
    /// A fragment of the model's reply, as it streams in.
    Text(&'a str),
    /// A new model call starts after tool results (text from before it is complete).
    Step,
    ToolCall { name: &'a str, input: &'a Value },
    /// Older history was summarised to free up context.
    Compacted,
}

pub struct Agent {
    provider: Arc<dyn LlmProvider>,
    tools: ToolRegistry,
    system: String,
    /// The system prompt a `before_turn` hook set for the running turn.
    turn_system: Option<String>,
    /// Facts and skills as shown in the system prompt, fixed for the session.
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
    context_limit: usize,
    /// Input tokens the provider reported for the latest call.
    last_input_tokens: usize,
    /// After a failed summary, don't retry until the history has grown to this length.
    compact_retry_at: usize,
    /// Tool calls the running turn made.
    turn_tool_calls: usize,
    review_counters: review::Counters,
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
            context_limit: std::env::var("AUGUST_CONTEXT_TOKENS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(DEFAULT_CONTEXT_TOKENS),
            last_input_tokens: 0,
            compact_retry_at: 0,
            turn_tool_calls: 0,
            review_counters: review::Counters::default(),
        })
    }

    /// Stores the tokens of a model call; accounting never fails a turn.
    fn record_usage(&self, usage: &Usage) {
        if let Err(e) = self.db.record_usage(&self.session, usage) {
            eprintln!("memory: could not record token usage: {e:#}");
        }
    }

    pub fn set_provider(&mut self, provider: Arc<dyn LlmProvider>) {
        self.provider = provider;
    }

    /// Starts a new conversation; the old one stays searchable.
    pub fn reset(&mut self) -> Result<()> {
        let previous = std::mem::replace(&mut self.session, self.db.new_session(&self.chat_key)?);
        self.notify_ext("session_start", json!({"previous": previous, "session": self.session}), self.chat_ref());
        self.history.clear();
        self.snapshot = None;
        self.stored = 0;
        self.turn_start = 0;
        self.last_input_tokens = 0;
        self.compact_retry_at = 0;
        Ok(())
    }

    /// The thread for hooks outside a turn: `telegram:5#task1` -> `telegram:5`.
    fn chat_ref(&self) -> Origin {
        let (messenger, id) = self.chat_key.split_once(':')?;
        Some(crate::messengers::Thread::new(messenger, id.split('#').next().unwrap_or(id)))
    }

    /// Fires an observe-only extension event in the background.
    fn notify_ext(&self, event: &'static str, data: Value, chat: Origin) {
        if let Some(ext) = self.tools.extensions().filter(|e| e.listens(event)).cloned() {
            tokio::spawn(async move { ext.emit(event, data, &chat).await });
        }
    }

    /// One model call with the `llm_call` / `llm_result` hooks; `step` counts from 0.
    async fn call_model(
        &mut self,
        step: usize,
        specs: &[ToolSpec],
        ctx: &ToolCtx,
        on_event: &mut (dyn FnMut(Event) + Send),
    ) -> Result<Completion> {
        let mut system = self.system_now();
        if let Some(ext) = self.tools.extensions().filter(|e| e.listens("llm_call")) {
            let data = ext.emit("llm_call", json!({"step": step, "system": system}), &ctx.origin).await;
            if let Some(s) = data["system"].as_str() {
                system = s.to_string();
            }
        }
        // A `context` hook may change what the model sees for this one call (not the history).
        let mut rewritten = None;
        if let Some(ext) = self.tools.extensions().filter(|e| e.listens("context")) {
            let messages: Vec<Value> = self.history.iter().map(Message::to_json).collect();
            let data = ext.emit("context", json!({"step": step, "messages": messages}), &ctx.origin).await;
            if let Some(list) = data["messages"].as_array() {
                let parsed: Option<Vec<Message>> = list.iter().map(Message::from_json).collect();
                match parsed {
                    Some(m) if m.len() != self.history.len() || list != &messages => rewritten = Some(m),
                    Some(_) => {}
                    None => eprintln!("context hook returned malformed messages; ignored"),
                }
            }
        }
        let messages = rewritten.as_deref().unwrap_or(&self.history);
        let completion = {
            let mut on_text = |t: &str| on_event(Event::Text(t));
            self.provider.complete_stream(&self.session, &system, messages, specs, &mut on_text).await?
        };
        self.last_input_tokens = completion.usage.context_tokens() as usize;
        self.record_usage(&completion.usage);
        let u = &completion.usage;
        let calls: Vec<Value> =
            completion.message.tool_uses().map(|(_, name, input)| json!({"name": name, "input": input})).collect();
        let data = json!({
            "step": step,
            "text": completion.message.text(),
            "toolCalls": calls,
            "usage": {"inputTokens": u.input_tokens, "outputTokens": u.output_tokens,
                      "cacheReadTokens": u.cache_read_tokens, "cacheWriteTokens": u.cache_write_tokens},
        });
        self.notify_ext("llm_result", data, ctx.origin.clone());
        Ok(completion)
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

    /// Runs one user turn to completion and returns the final assistant text.
    /// History is append-only; on error the whole turn is rolled back.
    pub async fn run_turn(
        &mut self,
        user_text: &str,
        ctx: &ToolCtx,
        on_event: &mut (dyn FnMut(Event) + Send),
    ) -> Result<String> {
        self.run_turn_with(user_text, Vec::new(), ctx, on_event).await
    }

    /// Like `run_turn`, with extra blocks (images) after the user's text.
    pub async fn run_turn_with(
        &mut self,
        user_text: &str,
        attachments: Vec<Block>,
        ctx: &ToolCtx,
        on_event: &mut (dyn FnMut(Event) + Send),
    ) -> Result<String> {
        self.turn_start = self.history.len();
        self.turn_system = None;
        self.turn_tool_calls = 0;
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
        let result = self.run_turn_inner(&text, attachments, ctx, on_event).await;
        self.turn_system = None;
        match &result {
            Ok(reply) => {
                self.persist();
                if !ctx.unattended {
                    if let Some(review) = self.review_due(ctx, self.turn_tool_calls) {
                        let notify = ctx.notify.clone();
                        tokio::spawn(async move {
                            match review.run().await {
                                Ok(changes) if changes.is_empty() => eprintln!("review: nothing to save"),
                                Ok(changes) => {
                                    eprintln!("review: {}", changes.join("; "));
                                    if let (Some(n), Some(text)) = (notify, review::notice(&changes)) {
                                        n.notify(&text).await;
                                    }
                                }
                                Err(e) => eprintln!("review failed: {e:#}"),
                            }
                        });
                    }
                }
                let data = json!({"text": text, "reply": reply, "unattended": ctx.unattended});
                self.notify_ext("turn_end", data, ctx.origin.clone());
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
        let stamp = chrono::Local::now().format("%a %Y-%m-%d %H:%M");
        let mut user = Message::user_text(format!("[{stamp}] {user_text}"));
        user.content.extend(attachments);
        self.history.push(user);
        let specs = self.tools.specs();

        let limit = max_steps();
        for step in 0..limit {
            if step > 0 {
                on_event(Event::Step);
                // Messages the user sent meanwhile join the tool results.
                let news = ctx.inbox.as_ref().map(|i| i.take()).unwrap_or_default();
                if !news.is_empty() {
                    let stamp = chrono::Local::now().format("%a %Y-%m-%d %H:%M");
                    let text = format!("[{stamp}] [The user sent this while you were working]\n{}", news.join("\n"));
                    if let Some(last) = self.history.last_mut() {
                        last.content.push(Block::Text(text));
                    }
                }
            }
            match self.compact(false).await {
                Ok(Some(_)) => on_event(Event::Compacted),
                Ok(None) => {}
                Err(e) => eprintln!("context compaction failed: {e:#}"),
            }
            let completion = self.call_model(step, &specs, ctx, on_event).await?;
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
            self.turn_tool_calls += calls.len();
            for (_, name, input) in &calls {
                on_event(Event::ToolCall { name, input });
            }
            let outputs =
                futures_util::future::join_all(calls.iter().map(|(_, name, input)| self.tools.call(name, input, ctx))).await;
            let mut results = Vec::new();
            for ((id, _, _), (output, is_error)) in calls.iter().zip(outputs) {
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
        // Out of steps: one last call without tools for a report instead of a dead end.
        on_event(Event::Step);
        let note = format!(
            "[Step limit reached: you used all {limit} steps of this turn. Don't call tools. Tell the \
             user briefly what you did, what is left, and how to continue.]"
        );
        if let Some(last) = self.history.last_mut() {
            last.content.push(Block::Text(note));
        }
        let completion = self.call_model(limit, &[], ctx, on_event).await?;
        self.history.push(completion.message.clone());
        Ok(completion.message.text())
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
                usage: Usage::default(),
            })
        }
    }

    fn agent(db: Arc<Db>) -> Agent {
        Agent::new(Arc::new(Fake), ToolRegistry::with_defaults(), "sys".into(), db, "test").unwrap()
    }

    fn big_history(a: &mut Agent) {
        for i in 0..10 {
            a.history.push(Message::user_text(format!("question {i} {}", "x".repeat(2_000))));
            a.history.push(Message { role: Role::Assistant, content: vec![Block::Text(format!("answer {i}"))] });
        }
    }

    #[tokio::test]
    async fn compaction_keeps_a_valid_tail_and_persists() {
        let db = Db::in_memory();
        let mut a = agent(db.clone());
        a.context_limit = 1_000;
        big_history(&mut a);
        let before = a.history.len();
        let (b, after) = a.compact(false).await.unwrap().expect("compacted");
        assert!(after < b);
        assert!(a.history.len() < before);
        assert_eq!(a.history[0].role, Role::User);
        assert!(a.history[0].text().contains("[Summary of the earlier conversation]"));
        // roles still alternate
        assert!(a.history.windows(2).all(|w| w[0].role != w[1].role));
        // the stored live history matches memory, so a restart resumes it
        let resumed = agent(db);
        assert_eq!(resumed.history.len(), a.history.len());
    }

    #[tokio::test]
    async fn small_histories_are_left_alone() {
        let mut a = agent(Db::in_memory());
        a.history.push(Message::user_text("hi"));
        assert!(a.compact(false).await.unwrap().is_none());
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
            content: vec![Block::ToolUse { id: "1".into(), name: "shell".into(), input: Value::Null }],
        });
        a.rollback_turn();
        assert_eq!(a.history.len(), 2);
    }

    #[test]
    fn reset_starts_a_fresh_session() {
        let db = Db::in_memory();
        let mut a = agent(db.clone());
        a.history.push(Message::user_text("hi"));
        a.persist();
        a.reset().unwrap();
        assert_eq!(agent(db).history.len(), 0);
    }
}
