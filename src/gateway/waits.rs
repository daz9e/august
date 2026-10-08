//! Waiting for what a thread sends next: a press of one of some buttons, or a text. A
//! wait takes the event before the gateway would treat it as a new message or command.

use crate::messengers::{Inbound, InboundKind, Thread};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::sync::oneshot;

/// What a wait takes.
#[derive(Debug, Clone, Default)]
pub struct Accept {
    /// Presses of these buttons (by id).
    pub buttons: Vec<String>,
    /// Any text message without files.
    pub text: bool,
    /// The text is a secret (a key): its message is deleted from the chat once taken.
    pub secret: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Reply {
    Press(String),
    Text(String),
    /// The thread got `/stop` or `/new` (the reason) before anything came.
    Cancelled(&'static str),
}

struct Wait {
    id: u64,
    thread: Thread,
    accept: Accept,
    tx: oneshot::Sender<Reply>,
}

#[derive(Default)]
pub struct Waits {
    list: Mutex<Vec<Wait>>,
    next: AtomicU64,
}

impl Waits {
    /// Starts waiting in `thread`; the receiver gets the first event `accept` takes, or
    /// closes when the wait is removed or cancelled.
    pub fn add(&self, thread: Thread, accept: Accept) -> (u64, oneshot::Receiver<Reply>) {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.list.lock().unwrap().push(Wait { id, thread, accept, tx });
        (id, rx)
    }

    pub fn remove(&self, id: u64) {
        self.list.lock().unwrap().retain(|w| w.id != id);
    }

    /// Hands `ev` to the oldest wait in its thread that takes it; `Some(secret)` if one did.
    pub fn offer(&self, ev: &Inbound) -> Option<bool> {
        let reply = |w: &Wait| match &ev.kind {
            InboundKind::Press { button, .. } if w.accept.buttons.contains(button) => Some(Reply::Press(button.clone())),
            InboundKind::Message { text, files, .. } if w.accept.text && files.is_empty() && !text.trim().is_empty() => {
                Some(Reply::Text(text.trim().to_string()))
            }
            _ => None,
        };
        let mut list = self.list.lock().unwrap();
        let (i, r) = list.iter().enumerate().filter(|(_, w)| w.thread == ev.thread).find_map(|(i, w)| Some((i, reply(w)?)))?;
        let secret = matches!(r, Reply::Text(_)) && list[i].accept.secret;
        list.remove(i).tx.send(r).is_ok().then_some(secret)
    }

    /// Ends every wait in `thread` (on /stop or /new), telling them `why`.
    pub fn cancel(&self, thread: &Thread, why: &'static str) {
        let mut list = self.list.lock().unwrap();
        for w in std::mem::take(&mut *list) {
            if &w.thread == thread {
                w.tx.send(Reply::Cancelled(why)).ok();
            } else {
                list.push(w);
            }
        }
    }
}
