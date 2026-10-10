//! The control socket (`AUGUST_HOME/control.sock`, this user only): how the `august` program
//! reaches the running core. One JSON object per line each way, `{"result"}` or `{"error"}`
//! for each request:
//! - `{"op": "cli"}`: the commands extensions add to `august` (`[{name, description, owner}]`);
//! - `{"op": "cli_open", "name"}`: what to run for one (`{exec, owner, token}`); the token
//!   lets that program call the core as the extension that owns the command;
//! - `{"op": "shutdown", "reason"}`: stop the extensions (`shutdown {reason}`) and August;
//! - `{"token", "call": op, "params"}`: run operation `op` as the token's extension.

use super::Gateway;
use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};

/// Where the core at `home` listens.
pub fn socket_in(home: &Path) -> PathBuf {
    home.join("control.sock")
}

/// Starts listening, unless another August already does.
pub(super) async fn bind(home: &Path) -> Result<UnixListener> {
    let path = socket_in(home);
    if UnixStream::connect(&path).await.is_ok() {
        bail!("another August is already running ({})", path.display());
    }
    std::fs::remove_file(&path).ok();
    std::fs::create_dir_all(home)?;
    let listener = UnixListener::bind(&path).with_context(|| format!("listen on {}", path.display()))?;
    // Only this user may control the agent.
    std::fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o600))?;
    Ok(listener)
}

impl Gateway {
    pub(super) async fn serve_control(self: Arc<Self>, listener: UnixListener) {
        while let Ok((stream, _)) = listener.accept().await {
            let me = self.clone();
            tokio::spawn(async move { me.control_client(stream).await });
        }
    }

    async fn control_client(self: Arc<Self>, stream: UnixStream) {
        let (read, mut write) = stream.into_split();
        let mut lines = BufReader::new(read).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let reply = match serde_json::from_str::<Value>(&line) {
                Ok(req) => self.control(&req).await,
                Err(e) => Err(anyhow!("bad request: {e}")),
            };
            let reply = match reply {
                Ok(result) => json!({"result": result}),
                Err(e) => json!({"error": format!("{e:#}")}),
            };
            if write.write_all(format!("{reply}\n").as_bytes()).await.is_err() {
                return;
            }
        }
    }

    async fn control(self: &Arc<Self>, req: &Value) -> Result<Value> {
        if let Some(op) = req["call"].as_str() {
            let token = req["token"].as_str().unwrap_or_default();
            let ext = self.tokens.lock().unwrap().get(token).cloned().ok_or_else(|| anyhow!("unknown token"))?;
            let params = if req["params"].is_object() { req["params"].clone() } else { json!({}) };
            self.ext.allows(&ext, op, &params)?;
            return self.op(&ext, op, &params).await;
        }
        match req["op"].as_str().unwrap_or_default() {
            "cli" => Ok(Value::Array(
                self.ext.cli().into_iter().map(|(owner, c)| json!({"name": c.name, "description": c.description, "owner": owner})).collect(),
            )),
            "cli_open" => {
                let name = req["name"].as_str().unwrap_or_default();
                let (owner, c) = self.ext.cli().into_iter().find(|(_, c)| c.name == name).ok_or_else(|| anyhow!("no extension offers `{name}`"))?;
                // ponytail: tokens live until August stops; expire them if programs should lose access sooner.
                let token = crate::util::new_uuid();
                self.tokens.lock().unwrap().insert(token.clone(), owner.clone());
                Ok(json!({"exec": c.exec, "owner": owner, "token": token}))
            }
            "shutdown" => {
                self.shutdown(req["reason"].as_str().unwrap_or("stop")).await;
                Ok(Value::Null)
            }
            other => bail!("unknown request `{other}`"),
        }
    }
}
