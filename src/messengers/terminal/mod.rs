//! The terminal messenger. August listens on a local socket (`AUGUST_HOME/august.sock`);
//! every terminal that connects (`august` without arguments, see `client.rs`) is a thread
//! `cli:<n>`, numbered from 1 by the lowest free number, so the first window gets its
//! conversation back after a restart. Anything that speaks the protocol below can be a
//! terminal: a line editor, a full-screen UI, a test.
//!
//! Protocol, one JSON object per line:
//! - client → August: `{"type":"hello"}` first, then `{"type":"text","text":...}` (a
//!   message or `/command`) and `{"type":"press","button":...}`.
//! - August → client: `{"type":"hello","thread":...}`, `{"type":"send","id":...,"text":...,
//!   "buttons":[[{"id":...,"label":...}]],"files":[path...]}` (button rows; files with the
//!   text as caption), `{"type":"edit","id":...,"text":...,"buttons":[[...]]}` and
//!   `{"type":"idle"}` (nothing more to say for now).

pub mod client;

use super::{Attachment, Capabilities, CommandSpec, Description, Inbound, InboundKind, Messenger, OutMessage, Thread, User, bus::Bus};
use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::mpsc;

pub const ID: &str = "cli";

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum ToAugust {
    Hello,
    Text { text: String },
    Press { button: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WireButton {
    pub id: String,
    pub label: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum ToClient {
    Hello { thread: String },
    /// `buttons` in rows; `files` are local paths (the text is their caption).
    Send { id: String, text: String, buttons: Vec<Vec<WireButton>>, files: Vec<String> },
    Edit { id: String, text: String, buttons: Vec<Vec<WireButton>> },
    Idle,
}

fn wire(rows: &[Vec<super::Button>]) -> Vec<Vec<WireButton>> {
    rows.iter().map(|r| r.iter().map(|b| WireButton { id: b.id.clone(), label: b.label.clone() }).collect()).collect()
}

/// Where terminals connect.
pub fn socket_path() -> PathBuf {
    crate::config::home().join("august.sock")
}

#[derive(Default)]
pub struct Terminal(Arc<Clients>);

#[derive(Default)]
struct Clients {
    /// Connected terminals by thread id.
    clients: Mutex<HashMap<String, mpsc::UnboundedSender<ToClient>>>,
    next_message: AtomicU64,
}

impl Clients {
    fn to(&self, thread: &str, msg: ToClient) -> Result<()> {
        let clients = self.clients.lock().unwrap();
        let tx = clients.get(thread).with_context(|| format!("terminal {thread} is closed"))?;
        tx.send(msg).map_err(|_| anyhow::anyhow!("terminal {thread} is closed"))
    }

    /// Registers a new terminal under the lowest free number.
    fn join(&self, tx: mpsc::UnboundedSender<ToClient>) -> String {
        let mut clients = self.clients.lock().unwrap();
        let id = (1..).map(|n: u32| n.to_string()).find(|n| !clients.contains_key(n)).unwrap();
        clients.insert(id.clone(), tx);
        id
    }

    async fn serve(&self, stream: UnixStream, bus: Bus<Inbound>) -> Result<()> {
        let (read, mut write) = stream.into_split();
        let mut lines = BufReader::new(read).lines();
        match lines.next_line().await?.map(|l| serde_json::from_str::<ToAugust>(&l)) {
            Some(Ok(ToAugust::Hello)) => {}
            _ => bail!("a terminal must start with hello"),
        }
        let (tx, mut rx) = mpsc::unbounded_channel();
        let id = self.join(tx.clone());
        tx.send(ToClient::Hello { thread: id.clone() }).ok();
        let writer = tokio::spawn(async move {
            while let Some(msg) = rx.recv().await {
                let line = serde_json::to_string(&msg).unwrap_or_default() + "\n";
                if write.write_all(line.as_bytes()).await.is_err() {
                    break;
                }
            }
        });
        let thread = Thread::new(ID, &id);
        let user = User { id: format!("terminal-{id}"), name: std::env::var("USER").unwrap_or_else(|_| "you".into()) };
        while let Ok(Some(line)) = lines.next_line().await {
            let kind = match serde_json::from_str::<ToAugust>(&line) {
                Ok(ToAugust::Text { text }) => match super::parse_command(&text, None) {
                    Some((name, args)) => InboundKind::Command { name, args },
                    None => InboundKind::Message { text: text.trim().to_string(), files: Vec::new() },
                },
                Ok(ToAugust::Press { button }) => InboundKind::Press { button },
                Ok(ToAugust::Hello) | Err(_) => continue,
            };
            bus.publish(Inbound { thread: thread.clone(), user: user.clone(), kind });
        }
        self.clients.lock().unwrap().remove(&id);
        writer.abort();
        Ok(())
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
                threads: true,
            },
            extra: serde_json::json!({
                "buttons": "shown numbered; the user answers with the number",
                "threads": "one per open terminal window",
            }),
        }
    }

    async fn threads(&self) -> Vec<String> {
        let mut open: Vec<String> = self.0.clients.lock().unwrap().keys().cloned().collect();
        open.sort();
        open
    }

    async fn run(&self, bus: Bus<Inbound>) -> Result<()> {
        let path = socket_path();
        if UnixStream::connect(&path).await.is_ok() {
            bail!("another August is already listening on {}", path.display());
        }
        std::fs::remove_file(&path).ok();
        std::fs::create_dir_all(path.parent().unwrap_or(&path))?;
        let listener = UnixListener::bind(&path).with_context(|| format!("listen on {}", path.display()))?;
        // Only this user may talk to the agent.
        std::fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o600))?;
        loop {
            let (stream, _) = listener.accept().await?;
            let (clients, bus) = (self.0.clone(), bus.clone());
            tokio::spawn(async move {
                if let Err(e) = clients.serve(stream, bus).await {
                    eprintln!("terminal: {e:#}");
                }
            });
        }
    }

    async fn send(&self, thread: &str, message: &OutMessage) -> Result<String> {
        let id = self.0.next_message.fetch_add(1, Ordering::Relaxed).to_string();
        let files = message.files.iter().map(|p| p.display().to_string()).collect();
        self.0.to(thread, ToClient::Send { id: id.clone(), text: message.text.clone(), buttons: wire(&message.buttons), files })?;
        Ok(id)
    }

    async fn edit(&self, thread: &str, id: &str, message: &OutMessage) -> Result<()> {
        self.0.to(thread, ToClient::Edit { id: id.into(), text: message.text.clone(), buttons: wire(&message.buttons) })
    }

    async fn typing(&self, _thread: &str) -> Result<()> {
        Ok(())
    }

    async fn set_commands(&self, _commands: &[CommandSpec]) -> Result<()> {
        Ok(())
    }

    async fn download(&self, _file: &Attachment) -> Result<Vec<u8>> {
        bail!("the terminal has no attachments")
    }

    async fn idle(&self, thread: &str) {
        self.0.to(thread, ToClient::Idle).ok();
    }
}
