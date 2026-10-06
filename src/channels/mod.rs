//! Messenger abstraction. A `Channel` is a live connection to one messenger (it
//! publishes `Inbound` events on the bus and can send/edit messages); a
//! `ChannelDef` is the vendor: how to configure it and how to build the channel.
//! Message text everywhere is plain Markdown; each vendor converts it to its own
//! dialect.

pub mod bus;
pub mod chunk;
pub mod terminal;
pub mod telegram;

pub use chunk::split_markdown;

use bus::Bus;
use anyhow::Result;
use async_trait::async_trait;
use std::sync::Arc;

/// A conversation on a particular channel.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ChatId {
    pub channel: String,
    pub chat: String,
}

#[derive(Debug, Clone)]
pub struct User {
    pub id: String,
    pub name: String,
}

/// A file that came with a message; the channel downloads it on request.
#[derive(Debug, Clone)]
pub struct Attachment {
    /// Vendor handle for `Channel::download`.
    pub id: String,
    /// Original file name, if the messenger has one (photos don't).
    pub name: Option<String>,
    pub mime: Option<String>,
    pub size: Option<u64>,
}

#[derive(Debug, Clone)]
pub enum InboundKind {
    /// A message: text (or a caption) and any attached files.
    Message { text: String, files: Vec<Attachment> },
    /// `/name args` (without the slash).
    Command { name: String, args: String },
    /// A button press; `data` is what the button was created with.
    Action { id: String, data: String },
}

#[derive(Debug, Clone)]
pub struct Inbound {
    pub chat: ChatId,
    pub user: User,
    pub kind: InboundKind,
}

#[derive(Debug, Clone)]
pub struct Button {
    pub label: String,
    pub data: String,
}

#[derive(Clone)]
pub struct CommandSpec {
    pub name: std::borrow::Cow<'static, str>,
    pub description: std::borrow::Cow<'static, str>,
}

impl CommandSpec {
    pub const fn new(name: &'static str, description: &'static str) -> Self {
        Self { name: std::borrow::Cow::Borrowed(name), description: std::borrow::Cow::Borrowed(description) }
    }
}

pub struct Limits {
    /// Max Markdown characters per message the gateway should send.
    pub max_len: usize,
    /// Minimum gap between edits of one message while streaming.
    pub edit_interval: std::time::Duration,
}

#[async_trait]
pub trait Channel: Send + Sync {
    fn id(&self) -> &str;
    fn limits(&self) -> Limits;

    /// Receives messages until the connection is lost for good, publishing them on `bus`.
    async fn run(&self, bus: Bus<Inbound>) -> Result<()>;

    /// Sends a new message and returns its id.
    async fn send(&self, chat: &str, markdown: &str, buttons: &[Button]) -> Result<String>;
    async fn edit(&self, chat: &str, message: &str, markdown: &str, buttons: &[Button])
    -> Result<()>;
    /// "typing…" indicator; best effort.
    async fn typing(&self, chat: &str) -> Result<()>;
    async fn set_commands(&self, commands: &[CommandSpec]) -> Result<()>;
    /// Confirms a button press to the messenger (stops the button spinner).
    async fn ack_action(&self, action_id: &str) -> Result<()>;
    /// Fetches the contents of an inbound attachment.
    async fn download(&self, file: &Attachment) -> Result<Vec<u8>>;
    /// Sends a local file; images are shown inline where the messenger can.
    async fn send_file(&self, chat: &str, path: &std::path::Path, caption: &str) -> Result<()>;
}

#[async_trait]
pub trait ChannelDef: Send + Sync {
    fn id(&self) -> &'static str;
    fn label(&self) -> &'static str;
    fn is_configured(&self) -> Result<bool>;
    /// Interactive setup (token, owner pairing, ...); saves into `channels.json`.
    async fn setup(&self) -> Result<()>;
    /// `None` when not configured.
    fn build(&self) -> Result<Option<Arc<dyn Channel>>>;
}

pub fn registry() -> Vec<Box<dyn ChannelDef>> {
    vec![Box::new(telegram::TelegramDef)]
}

pub fn def(id: &str) -> Option<Box<dyn ChannelDef>> {
    registry().into_iter().find(|d| d.id() == id)
}

/// Splits `/cmd@bot args` into `(cmd, args)`; `None` if `text` is not a command.
pub fn parse_command(text: &str, bot_name: Option<&str>) -> Option<(String, String)> {
    let rest = text.trim().strip_prefix('/')?;
    let (head, args) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
    let (name, target) = head.split_once('@').unwrap_or((head, ""));
    if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return None;
    }
    if !target.is_empty() && !bot_name.is_some_and(|b| b.eq_ignore_ascii_case(target)) {
        return None; // addressed to another bot
    }
    Some((name.to_ascii_lowercase(), args.trim().to_string()))
}

/// Builds every configured messenger; errors if none is set up.
pub fn build_configured() -> Result<Vec<Arc<dyn Channel>>> {
    let mut chans = Vec::new();
    for def in registry() {
        if let Some(ch) = def.build()? {
            chans.push(ch);
        }
    }
    if chans.is_empty() {
        anyhow::bail!("no messenger configured: run `august connect`");
    }
    Ok(chans)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands() {
        assert_eq!(parse_command("/new", None), Some(("new".into(), "".into())));
        assert_eq!(
            parse_command("/Model@MyBot gpt 5", Some("mybot")),
            Some(("model".into(), "gpt 5".into()))
        );
        assert_eq!(parse_command("/model@other", Some("mybot")), None);
        assert_eq!(parse_command("hello /x", None), None);
        assert_eq!(parse_command("/ path", None), None);
    }
}
