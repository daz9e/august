//! Turns as one primitive. Every run of the agent is a turn with an id, a mode, the thread
//! it belongs to, the turn that started it (if any) and who did (`source`). `/stop` cancels
//! every turn of a thread, sub-agents included. Extensions start quiet, fork and fresh
//! turns, wait for their outcome, list and cancel them.

use super::Gateway;
use super::turn::ThreadFiles;
use crate::agent::{TurnMode, TurnTag};
use crate::extensions::{AgentOpts, Origin};
use crate::messengers::Thread;
use crate::tools::{Approver, ToolCtx};
use anyhow::{Result, bail};
use async_trait::async_trait;
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
        json!({"status": self.status, "reply": self.reply, "error": self.error, "toolCalls": self.tool_calls})
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
}

impl Turns {
    /// Registers a turn; the returned `Notify` fires when it is cancelled.
    pub fn begin(&self, thread: &Thread, mode: TurnMode, source: Option<String>, parent: Option<u64>) -> (TurnTag, Arc<Notify>) {
        let id = self.next.fetch_add(1, Ordering::Relaxed) + 1;
        let tag = TurnTag { id, mode, source, parent };
        let cancel = Arc::new(Notify::new());
        let running = Running { thread: thread.clone(), tag: tag.clone(), cancel: cancel.clone() };
        self.running.lock().unwrap().insert(id, running);
        (tag, cancel)
    }

    pub fn end(&self, id: u64) {
        self.running.lock().unwrap().remove(&id);
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
    /// `quiet`, `fork` or `fresh`.
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
    /// `fork`: run approvals without asking (nobody may be there to ask).
    #[serde(default)]
    pub approve_all: bool,
}

/// Lets everything through (a fork that may write in the background).
struct AllowAll;

#[async_trait]
impl Approver for AllowAll {
    async fn approve(&self, _action: &str) -> bool {
        true
    }
}

impl Gateway {
    /// Starts a turn for an extension and returns its id; its outcome waits for `wait_turn`.
    pub(super) fn start_turn(self: &Arc<Self>, req: TurnRequest) -> Result<u64> {
        let thread = req.thread.clone().ok_or_else(|| anyhow::anyhow!("a turn needs a thread"))?;
        let Some(messenger) = self.channels.get(&thread.messenger).cloned() else {
            bail!("messenger `{}` is not running", thread.messenger);
        };
        let mode = req.mode.unwrap_or(TurnMode::Quiet);
        if mode == TurnMode::Visible {
            bail!("an extension starts quiet, fork or fresh turns; to hand the thread a message, use prompt");
        }
        let (tag, cancel) = self.turns.begin(&thread, mode, req.source.clone(), req.parent);
        let (tx, rx) = oneshot::channel();
        self.turns.outcomes.lock().unwrap().insert(tag.id, rx);
        let (me, id) = (self.clone(), tag.id);
        tokio::spawn(async move {
            let outcome = me.run_turn(messenger, thread, tag, cancel, req).await;
            me.turns.end(id);
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

    async fn run_turn(self: &Arc<Self>, messenger: Arc<dyn crate::messengers::Messenger>, thread: Thread, tag: TurnTag, cancel: Arc<Notify>, req: TurnRequest) -> Outcome {
        let origin = Origin { thread: Some(thread.clone()), turn: Some(tag.clone()) };
        let approver: Arc<dyn Approver> = match req.approve_all {
            true => Arc::new(AllowAll),
            false => Arc::new(self.approver(messenger.clone(), thread.clone(), None)),
        };
        let ctx = ToolCtx {
            workspace: self.workspace.clone(),
            approver,
            db: self.db.clone(),
            origin,
            files: Some(Arc::new(ThreadFiles { messenger: messenger.clone(), thread: thread.id.clone() })),
            extensions: Some(self.ext.clone()),
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
                let opts = AgentOpts { system: req.system, tools: req.tools, exclude: req.exclude };
                let r = tokio::select! {
                    r = self.subagent(thread.clone(), &req.text, opts, ctx) => Some(r),
                    _ = cancel.notified() => None,
                };
                Outcome::of(r, Vec::new())
            }
            TurnMode::Visible => Outcome::of(Some(Err(anyhow::anyhow!("not here"))), Vec::new()),
        };
        self.turn_ended(&thread, &tag, &req.text, &outcome);
        outcome
    }

    /// Tells extensions a turn ended (in the background).
    pub(super) fn turn_ended(&self, thread: &Thread, tag: &TurnTag, text: &str, outcome: &Outcome) {
        if !self.ext.listens("turn_end") {
            return;
        }
        let data = json!({
            "text": text,
            "reply": outcome.reply,
            "status": outcome.status,
            "error": outcome.error,
            "toolCalls": outcome.tool_calls.len(),
            "unattended": tag.mode != TurnMode::Visible,
        });
        let origin = Origin { thread: Some(thread.clone()), turn: Some(tag.clone()) };
        let ext = self.ext.clone();
        tokio::spawn(async move { ext.emit("turn_end", data, &origin).await });
    }
}
