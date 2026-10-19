//! How a turn talks to whoever is on the other side (terminal, Telegram, a cron job).

use anyhow::{Result, bail};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;

pub type Fut<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// What the user typed, plus attached images.
#[derive(Debug, Clone, Default)]
pub struct Input {
    pub text: String,
    pub images: Vec<PathBuf>,
}

impl Input {
    pub fn text(text: impl Into<String>) -> Self {
        Self { text: text.into(), images: Vec::new() }
    }

    /// Several messages sent while the agent was busy, as one.
    pub fn merge(inputs: Vec<Input>) -> Input {
        let mut out = Input::default();
        for i in inputs {
            if !i.text.is_empty() {
                if !out.text.is_empty() {
                    out.text.push_str("\n\n");
                }
                out.text.push_str(&i.text);
            }
            out.images.extend(i.images);
        }
        out
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Yes,
    /// Yes, and don't ask again for this kind of command in this session.
    Always,
    No,
}

pub trait Channel: Send + Sync {
    /// Streamed reply text.
    fn text(&self, _delta: &str) {}
    /// A tool call is starting (also marks the end of the current text segment).
    fn tool(&self, _summary: &str) {}
    /// The last tool call finished.
    fn tool_done(&self, _output: &str, _is_error: bool) {}
    /// Messages the user sent while the turn was running, taken out of the queue.
    fn steer(&self) -> Vec<Input> {
        Vec::new()
    }
    fn approve<'a>(&'a self, _command: &'a str, _reason: &'a str) -> Fut<'a, Decision> {
        Box::pin(async { Decision::No })
    }
    fn send_file<'a>(&'a self, _path: &'a Path, _caption: &'a str) -> Fut<'a, Result<String>> {
        Box::pin(async { bail!("there is no chat to send files to here") })
    }
}

/// No one is watching (background review).
pub struct Silent;
impl Channel for Silent {}
