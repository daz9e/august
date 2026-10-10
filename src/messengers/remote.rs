//! A messenger that lives in an extension: August calls it (`messenger_send`, ...) to reach
//! its threads, and it hands in what comes from them with the `inbound` operation.

use super::{Attachment, CommandSpec, Description, Inbound, Messenger, OutMessage, bus::Bus};
use crate::extensions::Extensions;
use anyhow::Result;
use async_trait::async_trait;
use base64::Engine;
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;

const CALL_TIMEOUT: Duration = Duration::from_secs(60);
/// Uploads and downloads may be large.
const FILE_TIMEOUT: Duration = Duration::from_secs(300);

pub struct Remote {
    description: Description,
    ext: Arc<Extensions>,
}

impl Remote {
    pub fn new(description: Description, ext: Arc<Extensions>) -> Self {
        Self { description, ext }
    }

    async fn call(&self, method: &str, params: Value, timeout: Duration) -> Result<Value> {
        self.ext.call_messenger(&self.description.id, method, params, timeout).await
    }
}

#[async_trait]
impl Messenger for Remote {
    fn id(&self) -> &str {
        &self.description.id
    }

    fn describe(&self) -> Description {
        self.description.clone()
    }

    async fn threads(&self) -> Vec<String> {
        let v = self.call("messenger_threads", json!({}), CALL_TIMEOUT).await;
        v.ok().and_then(|v| serde_json::from_value(v).ok()).unwrap_or_default()
    }

    /// What comes in arrives through the `inbound` operation instead.
    async fn run(&self, _bus: Bus<Inbound>) -> Result<()> {
        Ok(())
    }

    async fn send(&self, thread: &str, message: &OutMessage) -> Result<String> {
        let timeout = if message.files.is_empty() { CALL_TIMEOUT } else { FILE_TIMEOUT };
        let v = self.call("messenger_send", json!({"thread": thread, "message": message}), timeout).await?;
        Ok(v.as_str().unwrap_or_default().to_string())
    }

    async fn edit(&self, thread: &str, id: &str, message: &OutMessage) -> Result<()> {
        self.call("messenger_edit", json!({"thread": thread, "id": id, "message": message}), CALL_TIMEOUT).await.map(drop)
    }

    async fn presence(&self, thread: &str, busy: bool) {
        if let Err(e) = self.call("messenger_presence", json!({"thread": thread, "busy": busy}), CALL_TIMEOUT).await {
            eprintln!("{e:#}");
        }
    }

    async fn delete(&self, thread: &str, id: &str) -> Result<()> {
        self.call("messenger_delete", json!({"thread": thread, "id": id}), CALL_TIMEOUT).await.map(drop)
    }

    async fn react(&self, thread: &str, id: &str, emoji: &str) -> Result<()> {
        self.call("messenger_react", json!({"thread": thread, "id": id, "emoji": emoji}), CALL_TIMEOUT).await.map(drop)
    }

    async fn set_commands(&self, commands: &[CommandSpec]) -> Result<()> {
        self.call("messenger_commands", json!({"commands": commands}), CALL_TIMEOUT).await.map(drop)
    }

    async fn open_thread(&self, parent: &str, title: &str) -> Result<String> {
        let v = self.call("messenger_open_thread", json!({"parent": parent, "title": title}), CALL_TIMEOUT).await?;
        Ok(v.as_str().unwrap_or_default().to_string())
    }

    async fn action(&self, thread: &str, name: &str, args: Value) -> Result<Value> {
        self.call("messenger_action", json!({"thread": thread, "action": name, "args": args}), CALL_TIMEOUT).await
    }

    async fn download(&self, file: &Attachment) -> Result<Vec<u8>> {
        let v = self.call("messenger_download", json!({"file": file}), FILE_TIMEOUT).await?;
        Ok(base64::engine::general_purpose::STANDARD.decode(v.as_str().unwrap_or_default())?)
    }
}
