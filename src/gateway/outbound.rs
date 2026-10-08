//! Every message August sends or edits passes the `message_out` hook, whoever sends it:
//! replies, command answers, approvals, extensions' `send`.

use crate::extensions::{Extensions, Origin};
use crate::messengers::bus::Bus;
use crate::messengers::{Attachment, CommandSpec, Description, Inbound, Messenger, OutMessage, Thread};
use anyhow::Result;
use async_trait::async_trait;
use serde_json::{Value, json};
use std::sync::Arc;

pub struct Hooked {
    pub inner: Arc<dyn Messenger>,
    pub ext: Arc<Extensions>,
}

impl Hooked {
    /// The message after `message_out`; `None` when a hook dropped it (`block`).
    async fn out(&self, kind: &str, thread: &str, id: Option<&str>, m: &OutMessage) -> Option<OutMessage> {
        if !self.ext.listens("message_out") {
            return Some(m.clone());
        }
        let buttons: Vec<Vec<Value>> = m.buttons.iter().map(|row| row.iter().map(|b| json!({"id": b.id, "label": b.label})).collect()).collect();
        let data = json!({"kind": kind, "id": id, "text": m.text, "buttons": buttons, "files": m.files, "reply_to": m.reply_to});
        let data = self.ext.emit("message_out", data, &Origin::thread(Thread::new(self.inner.id(), thread))).await;
        if data["block"] == true || data["block"].as_str().is_some_and(|s| !s.is_empty()) {
            return None;
        }
        match super::ops::message(&json!({"message": data})) {
            Ok(m) => Some(m),
            Err(e) => {
                eprintln!("message_out hook returned a bad message ({e}); sending the original");
                Some(m.clone())
            }
        }
    }
}

#[async_trait]
impl Messenger for Hooked {
    fn id(&self) -> &str {
        self.inner.id()
    }
    fn describe(&self) -> Description {
        self.inner.describe()
    }
    async fn threads(&self) -> Vec<String> {
        self.inner.threads().await
    }
    async fn run(&self, bus: Bus<Inbound>) -> Result<()> {
        self.inner.run(bus).await
    }
    async fn send(&self, thread: &str, message: &OutMessage) -> Result<String> {
        match self.out("send", thread, None, message).await {
            Some(m) => self.inner.send(thread, &m).await,
            None => Ok(String::new()),
        }
    }
    async fn edit(&self, thread: &str, id: &str, message: &OutMessage) -> Result<()> {
        match self.out("edit", thread, Some(id), message).await {
            Some(m) if !id.is_empty() => self.inner.edit(thread, id, &m).await,
            _ => Ok(()),
        }
    }
    async fn presence(&self, thread: &str, busy: bool) {
        self.inner.presence(thread, busy).await
    }
    async fn delete(&self, thread: &str, id: &str) -> Result<()> {
        self.inner.delete(thread, id).await
    }
    async fn react(&self, thread: &str, id: &str, emoji: &str) -> Result<()> {
        self.inner.react(thread, id, emoji).await
    }
    async fn set_commands(&self, commands: &[CommandSpec]) -> Result<()> {
        self.inner.set_commands(commands).await
    }
    async fn download(&self, file: &Attachment) -> Result<Vec<u8>> {
        self.inner.download(file).await
    }
}
