//! `terminal`: August in a terminal window. Two halves in one binary:
//! - as an extension (no arguments) it is the messenger `cli`: it listens on a local socket
//!   (`AUGUST_HOME/august.sock`), every window that connects is a thread `cli:<n>`, numbered
//!   from 1 by the lowest free number, so the first window gets its conversation back after a
//!   restart; and it adds `august` alone (and `login`, `model`, ...) to the `august` program;
//! - as `attach <socket> [-- line...]` it is that window (`client.rs`), which `august` runs.
//!
//! Anything that speaks the socket's protocol can be a window. One JSON object per line:
//! - window → August: `{"type":"hello"}` first, then `{"type":"text","text":...}` (a
//!   message or `/command`; an optional `"reply_to":{"id":...,"text":...}` quotes one of
//!   August's messages) and `{"type":"press","button":...}`.
//! - August → window: `{"type":"hello","thread":...}`, `{"type":"send","id":...,"text":...,
//!   "buttons":[[{"id":...,"label":...}]],"files":[path...]}` (Markdown; button rows; files
//!   with the text as caption), `{"type":"edit","id":...,"text":...,"buttons":[[...]]}`,
//!   `{"type":"busy"}` (August is working) and `{"type":"idle"}` (nothing more for now).

mod client;
#[cfg(test)]
mod tests;

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use august_ext::messenger::{Button, Capabilities, Description, InboundKind, Messenger, OutMessage, Place, Quote, User, parse_command};
use august_ext::{August, Thread};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::mpsc;

pub const ID: &str = "cli";

#[derive(Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum ToAugust {
    Hello,
    Text {
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reply_to: Option<Quote>,
    },
    Press { button: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum ToWindow {
    Hello { thread: String },
    /// `buttons` in rows; `files` are local paths (the text is their caption).
    Send { id: String, text: String, buttons: Vec<Vec<Button>>, files: Vec<String> },
    Edit { id: String, text: String, buttons: Vec<Vec<Button>> },
    Busy,
    Idle,
}

/// The windows connected now.
#[derive(Default)]
struct Windows {
    /// By thread id.
    open: Mutex<HashMap<String, mpsc::UnboundedSender<ToWindow>>>,
    next_message: AtomicU64,
}

impl Windows {
    fn to(&self, thread: &str, msg: ToWindow) -> Result<()> {
        let open = self.open.lock().unwrap();
        let tx = open.get(thread).with_context(|| format!("terminal {thread} is closed"))?;
        tx.send(msg).map_err(|_| anyhow::anyhow!("terminal {thread} is closed"))
    }

    /// Registers a new window under the lowest free number.
    fn join(&self, tx: mpsc::UnboundedSender<ToWindow>) -> String {
        let mut open = self.open.lock().unwrap();
        let id = (1..).map(|n: u32| n.to_string()).find(|n| !open.contains_key(n)).unwrap();
        open.insert(id.clone(), tx);
        id
    }

    /// Serves one window until it closes, handing August what it says.
    async fn serve(&self, stream: UnixStream, august: August) -> Result<()> {
        let (read, mut write) = stream.into_split();
        let mut lines = BufReader::new(read).lines();
        match lines.next_line().await?.map(|l| serde_json::from_str::<ToAugust>(&l)) {
            Some(Ok(ToAugust::Hello)) => {}
            _ => bail!("a terminal must start with hello"),
        }
        let (tx, mut rx) = mpsc::unbounded_channel();
        let id = self.join(tx.clone());
        tx.send(ToWindow::Hello { thread: id.clone() }).ok();
        let writer = tokio::spawn(async move {
            while let Some(msg) = rx.recv().await {
                let line = serde_json::to_string(&msg).unwrap_or_default() + "\n";
                if write.write_all(line.as_bytes()).await.is_err() {
                    break;
                }
            }
        });
        let thread = Thread { messenger: ID.into(), id: id.clone() };
        let user = User { id: format!("terminal-{id}"), name: std::env::var("USER").unwrap_or_else(|_| "you".into()) };
        while let Ok(Some(line)) = lines.next_line().await {
            let kind = match serde_json::from_str::<ToAugust>(&line) {
                Ok(ToAugust::Text { text, reply_to }) => match parse_command(&text, None) {
                    Some((name, args)) => InboundKind::Command { name, args },
                    None => {
                        let id = format!("in-{}", self.next_message.fetch_add(1, Ordering::Relaxed));
                        // Everything a terminal shows from the other side is August's.
                        let reply_to = reply_to.map(|q| Quote { mine: true, ..q });
                        InboundKind::Message { id, text: text.trim().to_string(), files: Vec::new(), reply_to, addressed: true }
                    }
                },
                Ok(ToAugust::Press { button }) => InboundKind::Press { button },
                Ok(ToAugust::Hello) | Err(_) => continue,
            };
            if let Err(e) = august.inbound(&thread, &Place::default(), &user, &kind).await {
                eprintln!("terminal {id}: {e:#}");
            }
        }
        self.open.lock().unwrap().remove(&id);
        writer.abort();
        Ok(())
    }
}

struct Terminal(Arc<Windows>);

#[async_trait]
impl Messenger for Terminal {
    async fn threads(&self) -> Vec<String> {
        let mut open: Vec<String> = self.0.open.lock().unwrap().keys().cloned().collect();
        open.sort();
        open
    }

    async fn send(&self, thread: &str, message: &OutMessage) -> Result<String> {
        let id = self.0.next_message.fetch_add(1, Ordering::Relaxed).to_string();
        let files = message.files.iter().map(|p| p.display().to_string()).collect();
        self.0.to(thread, ToWindow::Send { id: id.clone(), text: message.text.clone(), buttons: message.buttons.clone(), files })?;
        Ok(id)
    }

    async fn edit(&self, thread: &str, id: &str, message: &OutMessage) -> Result<()> {
        self.0.to(thread, ToWindow::Edit { id: id.into(), text: message.text.clone(), buttons: message.buttons.clone() })
    }

    async fn presence(&self, thread: &str, busy: bool) {
        self.0.to(thread, if busy { ToWindow::Busy } else { ToWindow::Idle }).ok();
    }
}

fn description() -> Description {
    Description {
        id: ID.into(),
        name: "a terminal".into(),
        capabilities: Capabilities {
            markdown: true,
            max_len: 1_000_000,
            buttons: 9,
            edit: true,
            edit_interval_ms: 30,
            files_in: false,
            files_out: true,
            images: false,
            audio_in: false,
            commands: false,
            presence: true,
            delete: false,
            reactions: false,
            reply: false,
            threads: true,
            open_thread: false,
        },
        notes: "Buttons are shown numbered; the user answers with the number. One thread per open terminal window.".into(),
        actions: Vec::new(),
    }
}

/// `august <cmd>` that open a window starting with a slash command: `(cmd, its command)`.
const SHORTCUTS: &[(&str, &str, &str)] = &[
    ("login", "login", "Sign in to a model provider or a service: august login [account]"),
    ("connect", "login", "Connect a messenger: august connect telegram"),
    ("logout", "logout", "Sign out: august logout <account>"),
    ("model", "model", "Show or switch the model: august model [provider:model]"),
    ("models", "models", "List the models of the provider"),
    ("status", "status", "What August runs on now"),
];

async fn serve(august: August) {
    august.describe("The terminal: `august` opens a window that is a thread of messenger `cli`.", "");
    let home = august.env("AUGUST_HOME").map(PathBuf::from).unwrap_or_else(|| august.dir().clone());
    let socket = home.join("august.sock");
    let exe = std::env::current_exe().map(|p| p.display().to_string()).unwrap_or_else(|_| "august-ext-terminal".into());
    let attach = vec![exe, "attach".into(), socket.display().to_string()];
    august.register_cli("", "Open a terminal window", &attach);
    for (name, command, about) in SHORTCUTS {
        let exec: Vec<String> = attach.iter().cloned().chain(["--".into(), format!("/{command}")]).collect();
        august.register_cli(name, about, &exec);
    }
    let windows: Arc<Windows> = Arc::default();
    august.register_messenger(description(), Arc::new(Terminal(windows.clone())));
    let listener = match listen(&socket) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("terminal: {e:#}");
            return august.run().await;
        }
    };
    let me = august.clone();
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let (windows, august) = (windows.clone(), me.clone());
            tokio::spawn(async move {
                if let Err(e) = windows.serve(stream, august).await {
                    eprintln!("terminal: {e:#}");
                }
            });
        }
    });
    august.run().await;
}

fn listen(path: &std::path::Path) -> Result<UnixListener> {
    std::fs::remove_file(path).ok();
    std::fs::create_dir_all(path.parent().unwrap_or(path))?;
    let listener = UnixListener::bind(path).with_context(|| format!("listen on {}", path.display()))?;
    // Only this user may talk to the agent.
    std::fs::set_permissions(path, std::os::unix::fs::PermissionsExt::from_mode(0o600))?;
    Ok(listener)
}

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("attach") {
        let socket = PathBuf::from(args.get(1).context("usage: attach <socket> [-- /command args...]")?);
        // `-- /login telegram`: the first line, as if typed.
        let first = args.iter().position(|a| a == "--").map(|i| args[i + 1..].join(" "));
        return client::run(&socket, first.as_deref()).await;
    }
    serve(August::new()).await;
    Ok(())
}
