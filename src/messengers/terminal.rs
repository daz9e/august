//! The terminal as a channel: one local chat (`cli:local`) run by the same gateway as the
//! messengers, so commands, turns, approvals, extensions and sub-agents all work the same.
//! Lines from stdin become messages and commands; replies are printed as they stream (an
//! edit prints only what it adds). Buttons are numbered: a number answers the oldest open
//! message with buttons by pressing that button; any other line is a message.

use super::{Attachment, Button, Capabilities, CommandSpec, Description, Inbound, InboundKind, Messenger, OutMessage, Thread, User, bus::Bus};
use anyhow::{Result, bail};
use async_trait::async_trait;
use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{IsTerminal, Write};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::io::{AsyncBufReadExt, BufReader};

pub const ID: &str = "cli";
const CHAT: &str = "local";

#[derive(Default)]
struct Screen {
    /// What each printed message shows so far.
    shown: HashMap<String, String>,
    /// Messages that asked a question (an edit of one shows only its verdict).
    asked: HashSet<String>,
    /// Questions waiting for an answer, oldest first: (message id, buttons).
    open: VecDeque<(String, Vec<Button>)>,
    /// A prompt (`> `, or the options of an open question) is the last thing on screen.
    at_prompt: bool,
    /// The cursor is not at the start of a line.
    mid_line: bool,
}

impl Screen {
    /// What to type to answer the oldest open question.
    fn question_prompt(&self) -> Option<String> {
        let (_, buttons) = self.open.front()?;
        let options: Vec<String> = buttons.iter().enumerate().map(|(i, b)| format!("{}. {}", i + 1, b.label)).collect();
        Some(format!("   {} → ", options.join("   ")))
    }
}

#[derive(Default)]
pub struct Terminal {
    screen: Mutex<Screen>,
    next_id: AtomicU64,
}

impl Terminal {
    /// Prints `text` (`fresh`: on a new line) above the prompt. While a question is open,
    /// its options stay the last line.
    fn print(&self, text: &str, fresh: bool) {
        let mut s = self.screen.lock().unwrap();
        let mut out = std::io::stdout().lock();
        if s.at_prompt {
            // ponytail: also wipes what the user was typing from view (not from the input); a line editor would keep it.
            write!(out, "{}", if out.is_terminal() { "\r\x1b[2K" } else { "\n" }).ok();
            s.at_prompt = false;
            s.mid_line = false;
        }
        if fresh && s.mid_line {
            writeln!(out).ok();
        }
        write!(out, "{text}").ok();
        if !text.is_empty() {
            s.mid_line = !text.ends_with('\n');
        }
        if let Some(prompt) = s.question_prompt() {
            write!(out, "{}{prompt}", if s.mid_line { "\n" } else { "" }).ok();
            s.at_prompt = true;
            s.mid_line = false;
        }
        out.flush().ok();
    }

    /// Shows `> ` once nothing else is going on.
    fn prompt(&self) {
        let mut s = self.screen.lock().unwrap();
        if s.at_prompt {
            return;
        }
        let mut out = std::io::stdout().lock();
        write!(out, "{}> ", if s.mid_line { "\n\n" } else { "\n" }).ok();
        out.flush().ok();
        s.at_prompt = true;
        s.mid_line = false;
    }

    /// The button a number picks on the oldest open message with buttons.
    fn pick(&self, line: &str) -> Option<String> {
        let s = self.screen.lock().unwrap();
        let (_, buttons) = s.open.front()?;
        let i = line.parse::<usize>().ok()?.checked_sub(1)?;
        buttons.get(i).map(|b| b.id.clone())
    }
}

#[async_trait]
impl Messenger for Terminal {
    fn id(&self) -> &str {
        ID
    }

    fn describe(&self) -> Description {
        Description {
            id: ID.into(),
            name: "a terminal".into(),
            capabilities: Capabilities {
                markdown: false,
                max_len: 1_000_000,
                buttons: 9,
                edit: true,
                edit_interval_ms: 30,
                files_in: false,
                files_out: true,
                images: false,
                audio_in: false,
                commands: false,
                typing: false,
                threads: false,
            },
            extra: serde_json::json!({"buttons": "shown numbered; the user answers with the number"}),
        }
    }

    async fn run(&self, bus: Bus<Inbound>) -> Result<()> {
        let chat = Thread { messenger: ID.into(), id: CHAT.into() };
        let user = User { id: "local".into(), name: std::env::var("USER").unwrap_or_else(|_| "you".into()) };
        println!("Write a message; /help lists commands, /exit quits.");
        self.prompt();
        let mut lines = BufReader::new(tokio::io::stdin()).lines();
        while let Some(line) = lines.next_line().await? {
            let line = line.trim().to_string();
            {
                let mut s = self.screen.lock().unwrap();
                s.at_prompt = false;
                s.mid_line = false;
            }
            let kind = match self.pick(&line) {
                Some(button) => {
                    self.screen.lock().unwrap().open.pop_front();
                    InboundKind::Press { button, ack: String::new() }
                }
                None if line.is_empty() => {
                    self.prompt();
                    continue;
                }
                None if line == "/exit" || line == "/quit" => return Ok(()),
                None => match super::parse_command(&line, None) {
                    Some((name, args)) => InboundKind::Command { name, args },
                    None => InboundKind::Message { text: line, files: Vec::new() },
                },
            };
            bus.publish(Inbound { thread: chat.clone(), user: user.clone(), kind });
        }
        Ok(())
    }

    async fn send(&self, _thread: &str, message: &OutMessage) -> Result<String> {
        let (markdown, buttons) = (message.text.as_str(), &message.buttons);
        let id = self.next_id.fetch_add(1, Ordering::Relaxed).to_string();
        {
            let mut s = self.screen.lock().unwrap();
            s.shown.insert(id.clone(), markdown.to_string());
            if !buttons.is_empty() {
                s.asked.insert(id.clone());
                s.open.push_back((id.clone(), buttons.to_vec()));
            }
        }
        self.print(markdown, true);
        Ok(id)
    }

    async fn edit(&self, _thread: &str, message: &str, new: &OutMessage) -> Result<()> {
        let markdown = new.text.as_str();
        let (old, asked) = {
            let mut s = self.screen.lock().unwrap();
            s.open.retain(|(id, _)| id != message); // an edit settles a question (answered or timed out)
            (s.shown.insert(message.into(), markdown.into()).unwrap_or_default(), s.asked.contains(message))
        };
        match markdown.strip_prefix(old.as_str()) {
            _ if asked => self.print(markdown.lines().next().unwrap_or_default(), true),
            Some(added) => self.print(added, false),
            None => self.print(markdown, true),
        }
        Ok(())
    }

    async fn typing(&self, _chat: &str) -> Result<()> {
        Ok(())
    }

    async fn set_commands(&self, _commands: &[CommandSpec]) -> Result<()> {
        Ok(())
    }

    async fn ack(&self, _press: &str) -> Result<()> {
        Ok(())
    }

    async fn download(&self, _file: &Attachment) -> Result<Vec<u8>> {
        bail!("the terminal has no attachments")
    }

    async fn send_file(&self, _chat: &str, path: &std::path::Path, caption: &str) -> Result<()> {
        let caption = if caption.is_empty() { String::new() } else { format!(" — {caption}") };
        self.print(&format!("📎 {}{caption}", path.display()), true);
        Ok(())
    }

    async fn idle(&self, _chat: &str) {
        self.prompt();
    }
}

/// `august` without arguments: the gateway with the terminal as its only channel.
pub async fn run() -> Result<()> {
    crate::gateway::start(vec![std::sync::Arc::new(Terminal::default())]).await
}
