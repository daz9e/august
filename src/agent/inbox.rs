//! Messages that arrive while a turn runs. Instead of waiting for the turn to end, they
//! are handed to the agent before its next model call; whatever is left when the turn
//! ends becomes the next turn.

use std::sync::Mutex;

#[derive(Default)]
pub struct Inbox(Mutex<State>);

#[derive(Default)]
struct State {
    busy: bool,
    pending: Vec<String>,
    /// Kept for the next turn, without starting one.
    stashed: Vec<String>,
}

impl Inbox {
    /// Marks a turn as running.
    pub fn start(&self) {
        self.0.lock().unwrap().busy = true;
    }

    /// Takes `text` if a turn is running; `false` means the caller should start one.
    pub fn offer(&self, text: &str) -> bool {
        let mut s = self.0.lock().unwrap();
        if s.busy {
            s.pending.push(text.to_string());
        }
        s.busy
    }

    /// Keeps `text` for the next turn, whenever it comes.
    pub fn stash(&self, text: &str) {
        self.0.lock().unwrap().stashed.push(text.to_string());
    }

    /// What was stashed for this turn.
    pub fn unstash(&self) -> Vec<String> {
        std::mem::take(&mut self.0.lock().unwrap().stashed)
    }

    /// A turn of the chat runs, or is about to run what was sent meanwhile.
    pub fn busy(&self) -> bool {
        self.0.lock().unwrap().busy
    }

    /// Messages that arrived since the last call.
    pub fn take(&self) -> Vec<String> {
        std::mem::take(&mut self.0.lock().unwrap().pending)
    }

    /// Called when a turn ends: returns messages still waiting (the turn stays busy and
    /// should run them next), or marks the chat idle when there are none.
    pub fn finish(&self) -> Vec<String> {
        let mut s = self.0.lock().unwrap();
        if s.pending.is_empty() {
            s.busy = false;
        }
        std::mem::take(&mut s.pending)
    }
}
