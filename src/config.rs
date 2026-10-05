//! Files under `~/.august` (or `$AUGUST_HOME`): settings and owner-only secrets.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::collections::BTreeMap;
use std::path::PathBuf;

/// Folder the agent's tools operate in (`$AUGUST_WORKSPACE`, default `./workspace`).
pub fn workspace() -> Result<PathBuf> {
    let dir = PathBuf::from(crate::util::env_or("AUGUST_WORKSPACE", "./workspace"));
    std::fs::create_dir_all(&dir)?;
    Ok(dir.canonicalize()?)
}

pub fn home() -> PathBuf {
    std::env::var("AUGUST_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into())).join(".august")
        })
}

/// Reads `~/.august/<name>`; a missing file yields `T::default()`.
pub fn load<T: DeserializeOwned + Default>(name: &str) -> Result<T> {
    let path = home().join(name);
    match std::fs::read_to_string(&path) {
        Ok(s) => serde_json::from_str(&s).with_context(|| format!("parse {}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(T::default()),
        Err(e) => Err(e).with_context(|| format!("read {}", path.display())),
    }
}

/// Atomically writes `~/.august/<name>` with mode 0600.
pub fn save<T: Serialize>(name: &str, value: &T) -> Result<()> {
    let path = home().join(name);
    std::fs::create_dir_all(home())?;
    let tmp = path.with_extension("json.tmp");
    {
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut opts, 0o600);
        let mut f = opts.open(&tmp)?;
        std::io::Write::write_all(&mut f, serde_json::to_string_pretty(value)?.as_bytes())?;
    }
    std::fs::rename(&tmp, &path).with_context(|| format!("write {}", path.display()))
}

pub const CONFIG: &str = "config.json";
pub const CREDENTIALS: &str = "credentials.json";

/// Active provider and model, chosen with `august login` / `august model`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Config {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    /// Backup provider used when the active one keeps failing: `provider:model`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fallback: Option<String>,
}

/// API keys by provider id. ChatGPT OAuth tokens live in `chatgpt-auth.json`.
pub type Credentials = BTreeMap<String, ApiCredential>;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiCredential {
    pub key: String,
    /// OpenAI-compatible endpoints only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
}

/// Per-messenger settings (tokens, allowlists), keyed by channel id.
pub const CHANNELS: &str = "channels.json";
