//! A program `august <command>` runs (`August::register_cli`) talking to the core: it calls
//! the core's operations as the extension that registered the command, with its permissions,
//! over the core's control socket. One JSON object per line each way:
//! `{"token", "call": op, "params"}` → `{"result"}` or `{"error"}`.

use anyhow::{Context, Result, anyhow};
use serde_json::{Value, json};
use std::path::Path;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::net::UnixStream;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};

pub struct Client {
    token: String,
    read: Lines<BufReader<OwnedReadHalf>>,
    write: OwnedWriteHalf,
}

impl Client {
    /// The client `august` handed this program (`AUGUST_SOCKET`, `AUGUST_TOKEN`).
    pub async fn from_env() -> Result<Client> {
        let socket = std::env::var("AUGUST_SOCKET").context("AUGUST_SOCKET is not set: run this through `august`")?;
        Client::connect(Path::new(&socket), &std::env::var("AUGUST_TOKEN").unwrap_or_default()).await
    }

    pub async fn connect(socket: &Path, token: &str) -> Result<Client> {
        let stream = UnixStream::connect(socket).await.with_context(|| format!("August is not running ({})", socket.display()))?;
        let (read, write) = stream.into_split();
        Ok(Client { token: token.into(), read: BufReader::new(read).lines(), write })
    }

    /// Runs operation `op` of the core's table (`ops` lists them).
    pub async fn call(&mut self, op: &str, params: Value) -> Result<Value> {
        let line = json!({"token": self.token, "call": op, "params": params}).to_string() + "\n";
        self.write.write_all(line.as_bytes()).await?;
        let reply = self.read.next_line().await?.ok_or_else(|| anyhow!("August went away"))?;
        let reply: Value = serde_json::from_str(&reply)?;
        match reply["error"].as_str() {
            Some(e) => Err(anyhow!("{e}")),
            None => Ok(reply["result"].clone()),
        }
    }
}
