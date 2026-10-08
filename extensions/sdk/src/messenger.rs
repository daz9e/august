//! Messengers: what connects August to a person (Telegram, a terminal window, ...). The
//! types are shared by the core and the extensions that offer messengers, as the message
//! format both speak. An extension offers one with `August::register_messenger`, sends what
//! comes in with `August::inbound`, and August calls it to send, edit, ... (`Messenger`).
//! Message text everywhere is plain Markdown; each messenger converts it to its own dialect.

use anyhow::{Result, bail};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// A button under a message. `id` is opaque to the messenger: a press comes back with it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Button {
    pub id: String,
    pub label: String,
}

/// A message to send, in the one format every messenger takes. A messenger renders all of
/// it its own way, degrading what it can't show (e.g. buttons as a numbered list, files as
/// their paths) rather than failing.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct OutMessage {
    /// Markdown.
    pub text: String,
    /// Rows of buttons.
    pub buttons: Vec<Vec<Button>>,
    /// Local files to send with it (images shown inline where the messenger can); the text
    /// is their caption.
    pub files: Vec<PathBuf>,
    /// The id of a message this one answers.
    pub reply_to: Option<String>,
}

impl OutMessage {
    pub fn text(text: impl Into<String>) -> Self {
        Self { text: text.into(), ..Default::default() }
    }

    /// Every button, row after row.
    pub fn all_buttons(&self) -> impl Iterator<Item = &Button> {
        self.buttons.iter().flatten()
    }
}

/// What kind of file came in.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FileKind {
    /// A voice note recorded in the messenger.
    Voice,
    Audio,
    Image,
    Video,
    Document,
}

/// A file that came with a message; the messenger downloads it on request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Attachment {
    /// The messenger's handle for `Messenger::download`.
    pub id: String,
    pub kind: FileKind,
    /// Original file name, if the messenger has one (photos don't).
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub mime: Option<String>,
    #[serde(default)]
    pub size: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct User {
    pub id: String,
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum InboundKind {
    /// A message: its id in the messenger, text (or a caption) and any attached files.
    Message {
        id: String,
        text: String,
        #[serde(default)]
        files: Vec<Attachment>,
    },
    /// The user reacted to message `message` with `emoji` (an empty one: took it back).
    Reaction { message: String, emoji: String },
    /// `/name args` (without the slash).
    Command { name: String, args: String },
    /// A press of the button with this `id` (given when the message was sent). The
    /// messenger confirms the press itself.
    Press { button: String },
}

/// What a messenger tells about itself. The core (and extensions) decide by it instead of
/// assuming what a messenger can do.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Description {
    pub id: String,
    /// Human name, e.g. "Telegram".
    pub name: String,
    pub capabilities: Capabilities,
    /// Anything else the messenger offers, free form.
    #[serde(default)]
    pub extra: serde_json::Value,
}

/// What a messenger can do; what it leaves out it can't.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Capabilities {
    /// Markdown is rendered (else shown as is).
    pub markdown: bool,
    /// Longest message, in Markdown characters.
    pub max_len: usize,
    /// Buttons under one message, at most (0: none).
    pub buttons: usize,
    /// Sent messages can be edited, so replies stream in place.
    pub edit: bool,
    /// Shortest gap between two edits of one message, in milliseconds.
    pub edit_interval_ms: u64,
    /// Messages from the user can carry files.
    pub files_in: bool,
    /// Files can be sent to the user.
    pub files_out: bool,
    /// Images are shown inline.
    pub images: bool,
    /// Voice notes and audio files can come in.
    pub audio_in: bool,
    /// A menu of `/commands`.
    pub commands: bool,
    /// Shows that August is busy (e.g. "typing…").
    pub presence: bool,
    /// Sent messages can be deleted.
    pub delete: bool,
    /// Messages can carry and receive emoji reactions.
    pub reactions: bool,
    /// A message can answer another one (`reply_to`).
    pub reply: bool,
    /// More than one thread (several chats or windows).
    pub threads: bool,
}

/// An entry of the command menu.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandSpec {
    pub name: String,
    pub description: String,
}

/// A messenger an extension offers: what August asks of it. `thread` is the thread's id
/// within this messenger.
#[async_trait]
pub trait Messenger: Send + Sync {
    /// Sends a new message to `thread` and returns its id.
    async fn send(&self, thread: &str, message: &OutMessage) -> Result<String>;
    /// Replaces a sent message (where `capabilities.edit`).
    async fn edit(&self, thread: &str, id: &str, message: &OutMessage) -> Result<()>;
    /// August starts (`busy`) or stops working in `thread`; shown however the messenger
    /// shows it ("typing…", a prompt), and kept up by the messenger until it changes.
    async fn presence(&self, _thread: &str, _busy: bool) {}
    /// Deletes a sent message (where `capabilities.delete`).
    async fn delete(&self, _thread: &str, _id: &str) -> Result<()> {
        bail!("this messenger can't delete messages")
    }
    /// Sets August's reaction on a message (where `capabilities.reactions`); empty removes it.
    async fn react(&self, _thread: &str, _id: &str, _emoji: &str) -> Result<()> {
        bail!("this messenger has no reactions")
    }
    /// The command menu (where `capabilities.commands`).
    async fn set_commands(&self, _commands: &[CommandSpec]) -> Result<()> {
        Ok(())
    }
    /// Threads this messenger knows of (August adds the ones it has seen).
    async fn threads(&self) -> Vec<String> {
        Vec::new()
    }
    /// Fetches the contents of an inbound attachment.
    async fn download(&self, _file: &Attachment) -> Result<Vec<u8>> {
        bail!("this messenger has no attachments")
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands() {
        assert_eq!(parse_command("/new", None), Some(("new".into(), "".into())));
        assert_eq!(parse_command("/Model@MyBot gpt 5", Some("mybot")), Some(("model".into(), "gpt 5".into())));
        assert_eq!(parse_command("/model@other", Some("mybot")), None);
        assert_eq!(parse_command("hello /x", None), None);
        assert_eq!(parse_command("/ path", None), None);
    }
}
