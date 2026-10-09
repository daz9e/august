//! Turns as one primitive. Every run of the agent is a turn with an id, a mode, the thread
//! it belongs to, the turn that started it (if any) and who did (`source`). `/stop` cancels
//! every turn of a thread, sub-agents included. Extensions start turns of any mode, wait
//! for their outcome, list and cancel them.

use super::Gateway;
use crate::agent::{self, Agent, TurnMode, TurnTag};
use crate::extensions::Origin;
use crate::messengers::Thread;
use crate::tools::ToolCtx;
use anyhow::{Result, bail};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;
use tokio::sync::{Notify, oneshot};

/// How long an outcome nobody collects is kept.
const OUTCOME_TTL: Duration = Duration::from_secs(600);

/// How a turn ended: `status` is `ok`, `error` or `cancelled`.
#[derive(Debug, Clone)]
pub struct Outcome {
    pub status: &'static str,
    pub reply: String,
    pub error: Option<String>,
    /// `{name, input, output, isError}` of each tool call, where known.
    pub tool_calls: Vec<Value>,
}

impl Outcome {
    pub fn of(r: Option<Result<String>>, tool_calls: Vec<Value>) -> Self {
        match r {
            Some(Ok(reply)) => Self { status: "ok", reply, error: None, tool_calls },
            Some(Err(e)) => Self { status: "error", reply: String::new(), error: Some(format!("{e:#}")), tool_calls },
            None => Self { status: "cancelled", reply: String::new(), error: None, tool_calls },
        }
    }

    pub fn json(&self) -> Value {
        // Visible turns only count their calls (as nulls).
        let calls: Vec<&Value> = self.tool_calls.iter().filter(|c| !c.is_null()).collect();
        json!({"status": self.status, "reply": self.reply, "error": self.error, "toolCalls": calls})
    }
}

struct Running {
    thread: Thread,
    tag: TurnTag,
    cancel: Arc<Notify>,
}

#[derive(Default)]
pub struct Turns {
    next: AtomicU64,
    running: StdMutex<HashMap<u64, Running>>,
    /// Outcomes of turns extensions started, until they collect them.
    outcomes: StdMutex<HashMap<u64, oneshot::Receiver<Outcome>>>,
    /// Threads extensions were told are settled, until a turn begins there.
    settled: StdMutex<std::collections::HashSet<Thread>>,
    /// `turn_end` handlers still running, by thread (they may start the next turn).
    ending: StdMutex<HashMap<Thread, usize>>,
}

impl Turns {
    /// Registers a turn; the returned `Notify` fires when it is cancelled.
    pub fn begin(&self, thread: &Thread, mode: TurnMode, source: Option<String>, parent: Option<u64>, meta: Value) -> (TurnTag, Arc<Notify>) {
        let id = self.next.fetch_add(1, Ordering::Relaxed) + 1;
        let tag = TurnTag { id, mode, source, parent, meta };
        let cancel = Arc::new(Notify::new());
        let running = Running { thread: thread.clone(), tag: tag.clone(), cancel: cancel.clone() };
        self.running.lock().unwrap().insert(id, running);
        self.settled.lock().unwrap().remove(thread);
        (tag, cancel)
    }

    pub fn end(&self, id: u64) {
        self.running.lock().unwrap().remove(&id);
    }

    /// A running turn, by id.
    pub fn tag(&self, id: u64) -> Option<TurnTag> {
        self.running.lock().unwrap().get(&id).map(|r| r.tag.clone())
    }

    /// Cancels every running turn of `thread`; returns how many there were.
    pub fn cancel_thread(&self, thread: &Thread) -> usize {
        let running = self.running.lock().unwrap();
        let mine: Vec<&Running> = running.values().filter(|r| &r.thread == thread).collect();
        for r in &mine {
            r.cancel.notify_one();
        }
        mine.len()
    }

    pub fn cancel(&self, id: u64) -> bool {
        self.running.lock().unwrap().get(&id).map(|r| r.cancel.notify_one()).is_some()
    }

    pub fn busy(&self, thread: &Thread) -> bool {
        self.running.lock().unwrap().values().any(|r| &r.thread == thread)
    }

    /// Running turns, of one thread or all: `{id, mode, source, parent, thread}`.
    pub fn list(&self, thread: Option<&Thread>) -> Value {
        let running = self.running.lock().unwrap();
        let mut list: Vec<&Running> = running.values().filter(|r| thread.is_none_or(|t| &r.thread == t)).collect();
        list.sort_by_key(|r| r.tag.id);
        let list: Vec<Value> = list
            .into_iter()
            .map(|r| {
                let mut v = serde_json::to_value(&r.tag).unwrap_or_default();
                v["thread"] = json!({"messenger": r.thread.messenger, "id": r.thread.id});
                v
            })
            .collect();
        Value::Array(list)
    }
}

/// A turn an extension starts.
#[derive(Debug, Default, serde::Deserialize)]
pub struct TurnRequest {
    #[serde(skip)]
    pub thread: Option<Thread>,
    pub text: String,
    /// `visible`, `quiet` (the default), `fork` or `fresh`.
    pub mode: Option<TurnMode>,
    /// Who starts it (shown to hooks as `ctx.turn.source`).
    pub source: Option<String>,
    pub parent: Option<u64>,
    /// `fresh`: instructions added to the base system prompt.
    pub system: Option<String>,
    /// `fresh`: only these tools; `fork`: only these may be called (the rest stay offered,
    /// so the prompt cache holds).
    pub tools: Option<Vec<String>>,
    /// `fresh`: never these tools.
    #[serde(default)]
    pub exclude: Vec<String>,
    /// Whatever the starter wants hooks to know about the turn (`ctx.turn.meta`).
    #[serde(default)]
    pub meta: Value,
}

impl Gateway {
    /// Starts a turn for an extension and returns its id; its outcome waits for `wait_turn`.
    pub(super) fn start_turn(self: &Arc<Self>, req: TurnRequest) -> Result<u64> {
        let thread = req.thread.clone().ok_or_else(|| anyhow::anyhow!("a turn needs a thread"))?;
        let mode = req.mode.unwrap_or(TurnMode::Quiet);
        if thread.is_session() {
            anyhow::ensure!(mode != TurnMode::Visible, "a stored conversation has no chat to show a visible turn in");
            self.db.current_session(&thread.key())?;
        } else if self.channel(&thread.messenger).is_none() {
            bail!("messenger `{}` is not running", thread.messenger);
        }
        // Registered now, so /stop cancels it even while it waits for the thread.
        let (tag, cancel) = self.turns.begin(&thread, mode, req.source.clone(), req.parent, req.meta.clone());
        if mode != TurnMode::Visible {
            self.journal_turn(&thread, &tag, "turn_start", json!({"mode": tag.mode, "parent": tag.parent, "text": req.text}));
        }
        let (tx, rx) = oneshot::channel();
        self.turns.outcomes.lock().unwrap().insert(tag.id, rx);
        let (me, id) = (self.clone(), tag.id);
        tokio::spawn(async move {
            let outcome = if mode == TurnMode::Visible {
                me.visible_turn(thread, tag, cancel, req.text).await
            } else {
                let text = req.text.clone();
                let outcome = me.run_turn(thread.clone(), tag.clone(), cancel, req).await;
                me.turns.end(id);
                me.turn_ended(&thread, &tag, &text, &outcome);
                outcome
            };
            tx.send(outcome).ok();
            // Nobody collected it: drop it after a while.
            tokio::time::sleep(OUTCOME_TTL).await;
            me.turns.outcomes.lock().unwrap().remove(&id);
        });
        Ok(id)
    }

    /// Waits up to `timeout` for a turn's outcome (once per turn).
    pub(super) async fn wait_turn(&self, id: u64, timeout: Duration) -> Result<Value> {
        let rx = self.turns.outcomes.lock().unwrap().remove(&id);
        let rx = rx.ok_or_else(|| anyhow::anyhow!("no outcome of turn #{id} to wait for (unknown, or already collected)"))?;
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(outcome)) => Ok(outcome.json()),
            Ok(Err(_)) => bail!("turn #{id} was dropped"),
            Err(_) => Ok(json!({"status": "running"})),
        }
    }

    /// A visible turn an extension started: like one of the user's, after whatever runs in
    /// the thread now, streamed there; the model sees who it is from.
    async fn visible_turn(self: &Arc<Self>, thread: Thread, tag: TurnTag, cancel: Arc<Notify>, text: String) -> Outcome {
        let text = match tag.source.as_deref() {
            Some(s) if s != "user" => format!("[from {s}] {text}"),
            _ => text,
        };
        let id = tag.id;
        let run = async {
            let channel = self.messenger(&thread)?;
            self.turn(channel, thread.clone(), &thread.id, &text, Vec::new(), Some((tag, cancel))).await
        };
        run.await.unwrap_or_else(|e| {
            self.turns.end(id);
            Outcome::of(Some(Err(e)), Vec::new())
        })
    }

    async fn run_turn(self: &Arc<Self>, thread: Thread, tag: TurnTag, cancel: Arc<Notify>, req: TurnRequest) -> Outcome {
        let origin = Origin { thread: Some(thread.clone()), turn: Some(tag.clone()), ..Default::default() };
        let ctx = ToolCtx {
            workspace: self.workspace.clone(),
            db: self.db.clone(),
            origin,
            inbox: None,
            caller: "model".into(),
        };
        let outcome = match tag.mode {
            TurnMode::Quiet => {
                let state = match self.chat(&thread).await {
                    Ok(s) => s,
                    Err(e) => return Outcome::of(Some(Err(e)), Vec::new()),
                };
                // After whatever runs in the thread now; the user's next message waits for it.
                let mut agent = state.agent.lock().await;
                agent.set_provider(self.provider.read().unwrap().clone());
                let shown_nowhere: &mut (dyn FnMut(crate::agent::Event) + Send) = &mut |_| {};
                let r = tokio::select! {
                    r = agent.run_turn(&req.text, &ctx, shown_nowhere) => Some(r),
                    _ = cancel.notified() => None,
                };
                if r.is_none() {
                    agent.rollback_turn();
                }
                Outcome::of(r, Vec::new())
            }
            TurnMode::Fork => {
                let fork = match self.chat(&thread).await {
                    Ok(state) => state.agent.lock().await.fork(),
                    Err(e) => return Outcome::of(Some(Err(e)), Vec::new()),
                };
                let r = tokio::select! {
                    r = fork.run(&req.text, req.tools.as_deref(), &ctx) => Some(r),
                    _ = cancel.notified() => None,
                };
                match r {
                    Some(Ok((reply, calls))) => Outcome::of(Some(Ok(reply)), calls),
                    Some(Err(e)) => Outcome::of(Some(Err(e)), Vec::new()),
                    None => Outcome::of(None, Vec::new()),
                }
            }
            TurnMode::Fresh => {
                let r = tokio::select! {
                    r = self.fresh(&thread, &tag, &req, &ctx) => Some(r),
                    _ = cancel.notified() => None,
                };
                Outcome::of(r, Vec::new())
            }
            TurnMode::Visible => unreachable!("visible turns run in visible_turn"),
        };
        outcome
    }

    /// A conversation of its own (a sub-agent) with the thread's hooks; returns the reply.
    async fn fresh(&self, thread: &Thread, tag: &TurnTag, req: &TurnRequest, ctx: &ToolCtx) -> Result<String> {
        // A key of its own, so it starts fresh instead of resuming an older one's session.
        let key = format!("{}#agent-{}", thread.key(), &crate::util::new_uuid()[..8]);
        let mut tools = self.tools().without(&req.exclude);
        if let Some(only) = &req.tools {
            tools = tools.only(only);
        }
        let provider = self.provider.read().unwrap().clone();
        let system = agent::system_prompt(&self.workspace, req.system.as_deref().unwrap_or(""));
        let mut agent = Agent::new(provider, tools, system, self.db.clone(), &key)?;
        eprintln!("fresh turn #{} starts: {}", tag.id, req.text.chars().take(80).collect::<String>());
        agent.run_turn(&req.text, ctx, &mut |_| {}).await
    }

    /// Tells extensions (`turn_settled`, once) when nothing runs in `thread` any more and
    /// nothing is about to.
    pub(super) async fn settle(&self, thread: &Thread) {
        let chat = self.chats.lock().await.get(thread).cloned();
        let ending = self.turns.ending.lock().unwrap().get(thread).is_some_and(|n| *n > 0);
        if ending || self.turns.busy(thread) || chat.is_some_and(|c| c.inbox.busy()) || !self.ext.listens("turn_settled") {
            return;
        }
        if self.turns.settled.lock().unwrap().insert(thread.clone()) {
            self.ext.emit("turn_settled", json!({}), &Origin::thread(thread.clone())).await;
        }
    }

    /// Records a turn's start or end in the journal of the thread's session.
    pub(super) fn journal_turn(&self, thread: &Thread, tag: &TurnTag, kind: &str, data: Value) {
        let mut e = crate::db::Entry::new(kind, data);
        e.session = self.db.current_session(&thread.key()).ok().flatten();
        e.turn = Some(tag.id);
        e.source = Some(tag.source.clone().unwrap_or_else(|| "user".into()));
        crate::agent::SessionStore::journal(&*self.db, &e);
    }

    /// Tells extensions a turn ended (in the background), then, once their `turn_end`
    /// handlers are done (they may start the next turn), whether the thread settled.
    pub(super) fn turn_ended(self: &Arc<Self>, thread: &Thread, tag: &TurnTag, text: &str, outcome: &Outcome) {
        self.journal_turn(thread, tag, "turn_end", json!({"status": outcome.status, "error": outcome.error}));
        let data = json!({
            "text": text,
            "reply": outcome.reply,
            "status": outcome.status,
            "error": outcome.error,
            "toolCalls": outcome.tool_calls.len(),
            "unattended": tag.mode != TurnMode::Visible,
        });
        let origin = Origin { thread: Some(thread.clone()), turn: Some(tag.clone()), ..Default::default() };
        let (me, thread) = (self.clone(), thread.clone());
        *self.turns.ending.lock().unwrap().entry(thread.clone()).or_default() += 1;
        tokio::spawn(async move {
            me.ext.emit("turn_end", data, &origin).await;
            if let Some(n) = me.turns.ending.lock().unwrap().get_mut(&thread) {
                *n -= 1;
            }
            me.settle(&thread).await;
        });
    }
}
