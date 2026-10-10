//! Messengers. A `Messenger` is a live connection to one messenger (it publishes
//! `Inbound` events on the bus and can send/edit messages); each lives in an extension
//! (`remote.rs`), the terminal too. The message format is the SDK's
//! (`august_ext::messenger`). Message text everywhere is plain Markdown; each messenger
//! converts it to its own dialect.

pub mod bus;
pub use august_ext::chunk;
pub mod remote;

use bus::Bus;
use anyhow::Result;
use async_trait::async_trait;

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
    Action, Attachment, Button, Capabilities, CommandSpec, Description, FileKind, InboundKind, OutMessage, Place, PlaceKind, Quote, User,
    parse_command,
};

#[derive(Debug, Clone)]
pub struct Inbound {
    pub thread: Thread,
    pub place: Place,
    pub user: User,
    pub kind: InboundKind,
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
    /// Opens a thread titled `title` inside `parent` (where `capabilities.open_thread`).
    async fn open_thread(&self, _parent: &str, _title: &str) -> Result<String> {
        anyhow::bail!("this messenger can't open threads")
    }
    /// Runs one of `describe().actions`; the arguments are checked by then.
    async fn action(&self, _thread: &str, name: &str, _args: serde_json::Value) -> Result<serde_json::Value> {
        anyhow::bail!("this messenger has no action `{name}`")
    }
}
