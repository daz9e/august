//! Messengers. A `Messenger` is a live connection to one messenger (it publishes
//! `Inbound` events on the bus and can send/edit messages): the terminal is built in, any
//! other one lives in an extension (`remote.rs`). The message format is the SDK's
//! (`august_ext::messenger`). Message text everywhere is plain Markdown; each messenger
//! converts it to its own dialect.

pub mod bus;
pub use august_ext::chunk;
pub mod remote;
pub mod terminal;

use bus::Bus;
use anyhow::Result;
use async_trait::async_trait;
use std::sync::Arc;

/// A conversation in a messenger: a Telegram chat, a terminal window, ...
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Thread {
    pub messenger: String,
    pub id: String,
}

impl Thread {
    pub fn new(messenger: impl Into<String>, id: impl Into<String>) -> Self {
        Self { messenger: messenger.into(), id: id.into() }
    }

    /// `messenger:id`, the key sessions are stored under.
    pub fn key(&self) -> String {
        format!("{}:{}", self.messenger, self.id)
    }
    /// A stored conversation addressed directly (`session:<id>`), not a chat in a messenger.
    pub fn is_session(&self) -> bool {
        self.messenger == "session"
    }
}

pub use august_ext::messenger::{
    Attachment, Button, Capabilities, CommandSpec, Description, FileKind, InboundKind, OutMessage, User, parse_command,
};

#[derive(Debug, Clone)]
pub struct Inbound {
    pub thread: Thread,
    pub user: User,
    pub kind: InboundKind,
}

/// The system prompt's line on how replies are shown, from the messenger's description.
pub fn surface(d: &Description) -> String {
    if d.capabilities.markdown {
        format!(
            "The user reads your replies in {}, which renders Markdown (bold, italic, `code`, \
             fenced code blocks, lists, links). Avoid tables and headings unless they really help.",
            d.name
        )
    } else {
        format!("The user reads your replies in {}: plain text, Markdown is shown as is.", d.name)
    }
}

#[async_trait]
pub trait Messenger: Send + Sync {
    fn id(&self) -> &str;
    fn describe(&self) -> Description;

    /// Threads this messenger knows of (the core adds the ones it has seen).
    async fn threads(&self) -> Vec<String> {
        Vec::new()
    }

    /// Receives messages until the connection is lost for good, publishing them on `bus`.
    async fn run(&self, bus: Bus<Inbound>) -> Result<()>;

    /// Sends a new message to `thread` and returns its id.
    async fn send(&self, thread: &str, message: &OutMessage) -> Result<String>;
    /// Replaces a sent message (where `capabilities.edit`).
    async fn edit(&self, thread: &str, id: &str, message: &OutMessage) -> Result<()>;
    /// August starts (`busy`) or stops working in `thread`; shown however the messenger
    /// shows it ("typing…", a prompt), and kept up by the messenger until it changes.
    async fn presence(&self, _thread: &str, _busy: bool) {}
    /// Deletes a sent message (where `capabilities.delete`).
    async fn delete(&self, _thread: &str, _id: &str) -> Result<()> {
        anyhow::bail!("this messenger can't delete messages")
    }
    /// Sets August's reaction on a message (where `capabilities.reactions`); empty removes it.
    async fn react(&self, _thread: &str, _id: &str, _emoji: &str) -> Result<()> {
        anyhow::bail!("this messenger has no reactions")
    }
    async fn set_commands(&self, commands: &[CommandSpec]) -> Result<()>;
    /// Fetches the contents of an inbound attachment.
    async fn download(&self, file: &Attachment) -> Result<Vec<u8>>;
}

/// The messengers built into the core: the terminal. The rest come from extensions.
pub fn builtin() -> Vec<Arc<dyn Messenger>> {
    vec![Arc::new(terminal::Terminal::default())]
}
