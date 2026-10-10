//! Turns as one primitive. Every run of the agent is a turn with an id, the thread it
//! belongs to, the conversation it runs in (the thread's, a copy, a new one), whether it is
//! shown, the turn that started it (if any) and who did (`source`). `/stop` cancels every
//! turn of a thread, sub-agents included. Extensions start turns, wait for their outcome,
//! list and cancel them.

use super::Gateway;
use crate::agent::{Agent, Conversation, TurnTag};
use crate::extensions::Origin;
use crate::messengers::Thread;
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
    /// Registers a turn and gives it its id; the returned `Notify` fires when it is cancelled.
    pub fn begin(&self, thread: &Thread, mut tag: TurnTag) -> (TurnTag, Arc<Notify>) {
        let id = self.next.fetch_add(1, Ordering::Relaxed) + 1;
        tag.id = id;
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

    /// Whether `id` is a turn shown in `thread`'s own conversation (the user's).
    pub fn is_reply_of(&self, id: u64, thread: &Thread) -> bool {
        let running = self.running.lock().unwrap();
        running.get(&id).is_some_and(|r| &r.thread == thread && r.tag.show && r.tag.conversation == Conversation::Thread)
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

    /// Running turns, of one thread or all: `{id, conversation, show, source, parent, thread}`.
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
    /// `thread` (the default): the thread's conversation; `copy`: a copy of it, dropped
    /// afterwards; `new`: a conversation of its own.
    #[serde(default)]
    pub conversation: Conversation,
    /// Shown in the thread as it runs, like the user's turns (default: not).
    #[serde(default)]
    pub show: bool,
    /// Who starts it (shown to hooks as `ctx.turn.source`).
    pub source: Option<String>,
    pub parent: Option<u64>,
    /// `new`: instructions added to the base system prompt.
    pub system: Option<String>,
    /// Only these tools may be called. A `new` conversation is offered only these; the
    /// others keep offering what they did, so the prompt cache holds.
    pub tools: Option<Vec<String>>,
    /// Never these tools (like `tools`).
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
        if thread.is_session() {
            anyhow::ensure!(!req.show, "a stored conversation has no chat to show a turn in");
            self.db.current_session(&thread.key())?;
        } else if self.channel(&thread.messenger).is_none() {
            bail!("messenger `{}` is not running", thread.messenger);
        }
        let callable = (req.tools.is_some() || !req.exclude.is_empty()).then(|| {
            let names = self.tools().specs().into_iter().map(|s| s.name);
            let allowed = names.filter(|n| req.tools.as_ref().is_none_or(|t| t.contains(n)) && !req.exclude.contains(n));
            Arc::new(allowed.collect())
        });
        let tag = TurnTag {
            id: 0,
            conversation: req.conversation,
            show: req.show,
            source: req.source.clone(),
            parent: req.parent,
            meta: req.meta.clone(),
            callable,
        };
        // Registered now, so /stop cancels it even while it waits for the thread.
        let (tag, cancel) = self.turns.begin(&thread, tag);
        let (tx, rx) = oneshot::channel();
        self.turns.outcomes.lock().unwrap().insert(tag.id, rx);
        let (me, id) = (self.clone(), tag.id);
        tokio::spawn(async move {
            let outcome = me.run_turn(thread, tag, cancel, req).await;
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

    /// Runs a turn an extension started, in the conversation it asked for.
    async fn run_turn(self: &Arc<Self>, thread: Thread, tag: TurnTag, cancel: Arc<Notify>, req: TurnRequest) -> Outcome {
        let channel = if tag.show {
            match self.messenger(&thread) {
                Ok(c) => Some(c),
                Err(e) => return self.not_started(&thread, &tag, &req.text, e),
            }
        } else {
            None
        };
        // The thread's conversation and a shown turn: like one of the user's, after whatever
        // runs in the thread now, with the messages sent meanwhile.
        if tag.show && tag.conversation == Conversation::Thread {
            let (id, text) = (tag.clone(), req.text.clone());
            return match self.turn(channel.expect("shown"), thread.clone(), &thread.id, &req.text, Vec::new(), Some((tag, cancel))).await {
                Ok(outcome) => outcome,
                Err(e) => self.not_started(&thread, &id, &text, e),
            };
        }
        let state = match tag.conversation {
            Conversation::New => None,
            _ => match self.chat(&thread).await {
                Ok(s) => Some(s),
                Err(e) => return self.not_started(&thread, &tag, &req.text, e),
            },
        };
        match tag.conversation {
            // After whatever runs in the thread now; the user's next message waits for it.
            Conversation::Thread => {
                let state = state.expect("a chat");
                let mut agent = state.agent.lock().await;
                self.turn_once(&mut agent, channel, &thread, &req.text, Vec::new(), (tag, cancel), None).await
            }
            Conversation::Copy => {
                let mut fork = state.expect("a chat").agent.lock().await.fork();
                self.turn_once(&mut fork, channel, &thread, &req.text, Vec::new(), (tag, cancel), None).await
            }
            Conversation::New => {
                // A key of its own, so it starts fresh instead of resuming an older one's session.
                let key = format!("{}#agent-{}", thread.key(), &crate::util::new_uuid()[..8]);
                let mut tools = self.tools().without(&req.exclude);
                if let Some(only) = &req.tools {
                    tools = tools.only(only);
                }
                let provider = self.provider.read().unwrap().clone();
                let system = req.system.clone().unwrap_or_default();
                let mut agent = match Agent::new(provider, tools, system, self.db.clone(), &key) {
                    Ok(a) => a,
                    Err(e) => return self.not_started(&thread, &tag, &req.text, e),
                };
                self.turn_once(&mut agent, channel, &thread, &req.text, Vec::new(), (tag, cancel), None).await
            }
        }
    }

    /// A turn that failed before it ran: it ends like any other.
    fn not_started(self: &Arc<Self>, thread: &Thread, tag: &TurnTag, text: &str, e: anyhow::Error) -> Outcome {
        let outcome = Outcome::of(Some(Err(e)), Vec::new());
        self.turns.end(tag.id);
        self.turn_ended(thread, tag, text, &outcome);
        outcome
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
            "unattended": !tag.show,
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
