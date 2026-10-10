//! One user turn: runs the agent, streams its events to whoever draws it, handles `/stop`.

use super::Gateway;
use super::turns::Outcome;
use crate::agent::{Event, TurnTag};
use crate::extensions::{Extensions, Origin};
use crate::messengers::{Messenger, OutMessage, Thread};
use crate::llm::Block;
use crate::tools::ToolCtx;
use anyhow::Result;
use serde_json::{Value, json};
use std::sync::Arc;
use tokio::sync::{Notify, mpsc, oneshot};

/// What goes to the extension drawing a turn, in order.
pub(super) enum Live {
    Event(Value),
    /// A message goes out in the reply's place: the renderer gets a `break`, `drawn` fires
    /// once it handled everything before, and the stream holds until `resume` drops.
    Barrier { drawn: oneshot::Sender<()>, resume: oneshot::Receiver<()> },
}

impl Gateway {
    /// Sends `message` from `from` (an extension, or `core`) to `thread`; while a reply is
    /// drawn there, in its place in the stream.
    pub(super) async fn send_in_order(&self, thread: &Thread, message: OutMessage, from: &str) -> Result<String> {
        let messenger = self.messenger(thread)?;
        let live = self.live.lock().unwrap().get(thread).cloned();
        let Some((renderer, live)) = live.filter(|(renderer, _)| renderer != from) else {
            return messenger.send(&thread.id, &message).await;
        };
        let (drawn, drawn_rx) = oneshot::channel();
        let (_resume, resume) = oneshot::channel::<()>();
        if live.send(Live::Barrier { drawn, resume }).is_ok() && drawn_rx.await.is_err() {
            eprintln!("renderer {renderer} went away; sending without waiting");
        }
        messenger.send(&thread.id, &message).await
    }

    /// Runs a turn once the chat is free, then whatever the user sent meanwhile that the
    /// turn didn't pick up; returns how the first one ended. `begun`: the turn was already
    /// registered (an extension started it).
    pub(super) async fn turn(
        self: &Arc<Self>,
        channel: Arc<dyn Messenger>,
        id: Thread,
        chat: &str,
        text: &str,
        images: Vec<Block>,
        begun: Option<(TurnTag, Arc<Notify>)>,
    ) -> Result<Outcome> {
        let state = self.chat(&id).await?;
        let mut agent = state.agent.lock().await; // turns in one chat run in order
        state.inbox.start();
        channel.presence(chat, true).await;
        let stashed = state.inbox.unstash();
        let text = stashed.into_iter().chain([text.to_string()]).collect::<Vec<_>>().join("\n");
        let (mut images, mut text, mut begun) = (images, text, begun);
        let mut first = None;
        loop {
            let ended = self.turn_once(&state, &mut agent, channel.clone(), &id, chat, &text, images, begun.take()).await;
            first.get_or_insert(ended);
            let left = state.inbox.finish();
            if !left.is_empty() {
                (text, images) = (left.join("\n"), Vec::new());
                continue;
            }
            channel.presence(chat, false).await;
            self.settle(&id).await;
            return Ok(first.expect("ran at least once"));
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn turn_once(
        self: &Arc<Self>,
        state: &super::Chat,
        agent: &mut crate::agent::Agent,
        channel: Arc<dyn Messenger>,
        id: &Thread,
        chat: &str,
        text: &str,
        images: Vec<Block>,
        begun: Option<(TurnTag, Arc<Notify>)>,
    ) -> Outcome {
        let (tag, cancel) = begun.unwrap_or_else(|| self.turns.begin(id, crate::agent::TurnMode::Visible, None, None, Value::Null));
        self.journal_turn(id, &tag, "turn_start", json!({"mode": tag.mode, "parent": tag.parent, "text": text}));
        agent.set_provider(self.provider.read().unwrap().clone());

        let renderer = self.ext.taker("render");
        let drawing = renderer.as_ref().map(|name| {
            let (tx, task) = draw(self.ext.clone(), name.clone(), Origin { thread: Some(id.clone()), turn: Some(tag.clone()), ..Default::default() });
            self.live.lock().unwrap().insert(id.clone(), (name.clone(), tx.clone()));
            tx.send(Live::Event(json!({"kind": "start", "capabilities": channel.describe().capabilities}))).ok();
            (tx, task)
        });

        let origin = crate::extensions::Origin { thread: Some(id.clone()), turn: Some(tag.clone()), ..Default::default() };
        let ctx = ToolCtx {
            db: self.db.clone(),
            origin,
            inbox: Some(state.inbox.clone()),
            caller: "model".into(),
        };
        let hooked = self.ext.listens("turn_event").then(|| turn_events(self.ext.clone(), ctx.origin.clone()));
        let live = drawing.as_ref().map(|(tx, _)| tx.clone());
        let mut on_event = move |e: Event| {
            let e = match &e {
                Event::Text(t) => json!({"kind": "text", "text": t}),
                Event::Step => json!({"kind": "step"}),
                Event::ToolCall { name, input } => json!({"kind": "tool", "tool": name, "input": input}),
                Event::Note(text) => json!({"kind": "note", "text": text}),
            };
            if let Some(live) = &live {
                live.send(Live::Event(e.clone())).ok();
            }
            if let Some(hooked) = &hooked {
                hooked.send(e).ok();
            }
        };

        state.inbox.steering(true);
        let outcome = tokio::select! {
            r = agent.run_turn_with(text, images, &ctx, &mut on_event) => Some(r),
            _ = cancel.notified() => None,
        };
        state.inbox.steering(false);
        let ended = Outcome::of(
            outcome.as_ref().map(|r| r.as_ref().map(String::clone).map_err(|e| anyhow::anyhow!("{e:#}"))),
            vec![serde_json::Value::Null; agent.tool_calls()],
        );
        if outcome.is_none() {
            agent.rollback_turn();
        }
        match &drawing {
            Some((tx, _)) => {
                tx.send(Live::Event(json!({"kind": "end", "status": ended.status, "reply": ended.reply, "error": ended.error}))).ok();
            }
            // Nobody draws turns: just the outcome, once.
            None => {
                let text = match (ended.status, &ended.error) {
                    ("ok", _) if ended.reply.is_empty() => "(empty reply)".to_string(),
                    ("ok", _) => ended.reply.clone(),
                    ("cancelled", _) => "⏹ Stopped.".into(),
                    (_, e) => format!("⚠️ Error: {}", e.as_deref().unwrap_or_default()),
                };
                if let Err(e) = channel.send(chat, &OutMessage::text(text)).await {
                    eprintln!("{}: send failed: {e:#}", channel.id());
                }
            }
        }
        self.turns.end(tag.id);
        self.turn_ended(id, &tag, text, &ended);
        self.live.lock().unwrap().remove(id);
        drop(ctx);
        drop(on_event);
        if let Some((tx, task)) = drawing {
            drop(tx);
            task.await.ok();
        }
        ended
    }
}

/// Hands a turn's events to the extension `name` that draws it, one at a time and in order;
/// text that piles up while it draws goes out as one.
fn draw(ext: Arc<Extensions>, name: String, origin: Origin) -> (mpsc::UnboundedSender<Live>, tokio::task::JoinHandle<()>) {
    let (tx, mut rx) = mpsc::unbounded_channel::<Live>();
    let task = tokio::spawn(async move {
        while let Some(first) = rx.recv().await {
            let mut batch = vec![first];
            while let Ok(next) = rx.try_recv() {
                if let (Some(Live::Event(last)), Live::Event(e)) = (batch.last_mut(), &next)
                    && merge_text(last, e)
                {
                    continue;
                }
                batch.push(next);
            }
            for item in batch {
                match item {
                    Live::Event(e) => {
                        ext.run_job(&name, "render", &e, &origin).await;
                    }
                    Live::Barrier { drawn, resume } => {
                        ext.run_job(&name, "render", &json!({"kind": "break"}), &origin).await;
                        drawn.send(()).ok();
                        resume.await.ok();
                    }
                }
            }
        }
    });
    (tx, task)
}

/// Appends text event `e` to `last` when both are text.
fn merge_text(last: &mut Value, e: &Value) -> bool {
    let (Some(a), Some(b)) = (last["text"].as_str(), e["text"].as_str()) else { return false };
    if last["kind"] != "text" || e["kind"] != "text" {
        return false;
    }
    last["text"] = format!("{a}{b}").into();
    true
}

/// Feeds a visible turn's events to the `turn_event` hook in order; text fragments that pile
/// up while a handler runs go out as one.
fn turn_events(ext: Arc<Extensions>, origin: Origin) -> mpsc::UnboundedSender<Value> {
    let (tx, mut rx) = mpsc::unbounded_channel::<Value>();
    tokio::spawn(async move {
        while let Some(first) = rx.recv().await {
            let mut batch: Vec<Value> = vec![first];
            while let Ok(e) = rx.try_recv() {
                if !batch.last_mut().is_some_and(|last| merge_text(last, &e)) {
                    batch.push(e);
                }
            }
            for e in batch {
                ext.emit("turn_event", e, &origin).await;
            }
        }
    });
    tx
}
