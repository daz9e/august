//! `~/.august` (or `$AUGUST_HOME`): config, memory files, skills, sessions, cron jobs.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// "anthropic" or "chatgpt".
    pub provider: String,
    pub model: String,
    /// low | medium | high | xhigh | max
    pub effort: String,
    pub anthropic_key: Option<String>,
    /// OpenAI API key, used only to transcribe voice messages.
    pub openai_key: Option<String>,
    pub transcribe_model: String,
    pub telegram_token: Option<String>,
    pub telegram_allowed: Vec<i64>,
    /// Compact the session once a request's context exceeds this.
    pub context_tokens: u64,
    /// Background memory/skills review every N user messages (0 = off).
    pub review_every: usize,
    /// Ask before running dangerous shell commands.
    pub approvals: bool,
    /// The provider's built-in web search tool.
    pub web_search: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            provider: String::new(),
            model: String::new(),
            effort: "medium".into(),
            anthropic_key: None,
            openai_key: None,
            transcribe_model: "gpt-4o-mini-transcribe".into(),
            telegram_token: None,
            telegram_allowed: Vec::new(),
            context_tokens: 150_000,
            review_every: 10,
            approvals: true,
            web_search: true,
        }
    }
}

pub fn home() -> PathBuf {
    std::env::var_os("AUGUST_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| user_home().join(".august"))
}

pub fn user_home() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| "/".into())
}

pub fn workspace() -> PathBuf {
    home().join("workspace")
}

impl Config {
    fn path() -> PathBuf {
        home().join("config.json")
    }

    pub fn load() -> Result<Self> {
        match std::fs::read_to_string(Self::path()) {
            Ok(s) => serde_json::from_str(&s).context("config.json is not valid"),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e.into()),
        }
    }

    pub fn save(&self) -> Result<()> {
        write_private(&Self::path(), &serde_json::to_string_pretty(self)?)
    }
}

/// Writes a file only the owner can read (it holds secrets), atomically.
pub fn write_private(path: &Path, content: &str) -> Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("tmp");
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)?;
    std::io::Write::write_all(&mut f, content.as_bytes())?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}
