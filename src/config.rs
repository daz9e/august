//! `~/.august` (or `$AUGUST_HOME`): config, memory files, skills, sessions, cron jobs.
//!
//! Settings live in `config.json`, in sections, described by `config.schema.json` (editors use
//! it for completion and checks). Secrets live in `.env`.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Config {
    pub model: Model,
    pub tools: Tools,
    pub session: Session,
    pub memory: Memory,
    pub telegram: Telegram,
    pub voice: Voice,
    #[serde(skip)]
    pub secrets: Secrets,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Model {
    /// "anthropic" or "chatgpt"; empty until `august login`.
    pub provider: String,
    pub name: String,
    /// low | medium | high | xhigh | max
    pub effort: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Tools {
    /// The provider's built-in web search tool.
    pub web_search: bool,
    /// Ask before running dangerous shell commands.
    pub approvals: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Session {
    /// Compact the session once a request's context exceeds this many tokens.
    pub compact_at: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Memory {
    /// Background memory/skills review every N user messages (0 = off).
    pub review_every: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Telegram {
    pub allowed: Vec<i64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Voice {
    /// OpenAI model that transcribes voice messages (needs OPENAI_API_KEY).
    pub model: String,
}

/// What `.env` holds. Names there are the environment-style ones in `SECRETS`.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Secrets {
    pub anthropic_key: Option<String>,
    /// Used only to transcribe voice messages.
    pub openai_key: Option<String>,
    pub telegram_token: Option<String>,
}

pub const SECRETS: [&str; 3] = ["ANTHROPIC_API_KEY", "OPENAI_API_KEY", "TELEGRAM_BOT_TOKEN"];

impl Secrets {
    fn slots(&mut self) -> [(&'static str, &mut Option<String>); 3] {
        [(SECRETS[0], &mut self.anthropic_key), (SECRETS[1], &mut self.openai_key), (SECRETS[2], &mut self.telegram_token)]
    }

    fn parse(text: &str) -> Self {
        let mut s = Self::default();
        for line in text.lines() {
            let Some((k, v)) = line.trim().split_once('=') else { continue };
            let v = v.trim().trim_matches(|c| c == '"' || c == '\'');
            if let Some((_, slot)) = s.slots().into_iter().find(|(name, _)| *name == k.trim()) {
                *slot = (!v.is_empty()).then(|| v.to_string());
            }
        }
        s
    }

    fn render(&mut self) -> String {
        self.slots().into_iter().filter_map(|(k, v)| v.as_ref().map(|v| format!("{k}={v}\n"))).collect()
    }
}

impl Default for Model {
    fn default() -> Self {
        Self { provider: String::new(), name: String::new(), effort: "medium".into() }
    }
}

impl Default for Tools {
    fn default() -> Self {
        Self { web_search: true, approvals: true }
    }
}

impl Default for Session {
    fn default() -> Self {
        Self { compact_at: 150_000 }
    }
}

impl Default for Memory {
    fn default() -> Self {
        Self { review_every: 10 }
    }
}

impl Default for Voice {
    fn default() -> Self {
        Self { model: "gpt-4o-mini-transcribe".into() }
    }
}

/// Written into config.json so editors find the schema.
const SCHEMA_URL: &str = "https://raw.githubusercontent.com/daz9e/august/master/config.schema.json";

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

pub fn path() -> PathBuf {
    home().join("config.json")
}

pub fn env_path() -> PathBuf {
    home().join(".env")
}

impl Config {
    pub fn load() -> Result<Self> {
        let text = match std::fs::read_to_string(path()) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => "{}".into(),
            Err(e) => return Err(e.into()),
        };
        let json: Value = serde_json::from_str(&text).context("config.json is not valid JSON")?;
        let mut cfg = if json["provider"].is_string() { migrate_flat(&json)? } else { serde_json::from_value(json).context("config.json has a wrong value")? };
        if let Some(env) = std::fs::read_to_string(env_path()).ok() {
            cfg.secrets = Secrets::parse(&env);
        }
        Ok(cfg)
    }

    /// Writes the settings into config.json and the secrets into .env.
    pub fn save(&self) -> Result<()> {
        #[derive(Serialize)]
        struct File<'a> {
            #[serde(rename = "$schema")]
            schema: &'a str,
            #[serde(flatten)]
            cfg: &'a Config,
        }
        let json = serde_json::to_string_pretty(&File { schema: SCHEMA_URL, cfg: self })?;
        write_private(&path(), &(json + "\n"))?;
        write_private(&env_path(), &self.secrets.clone().render())
    }

    /// `config set`: a `section.key` setting, or one of `SECRETS`. Strings need no quotes.
    pub fn set(key: &str, raw: &str) -> Result<()> {
        let mut cfg = Self::load()?;
        let secret = cfg.secrets.slots().into_iter().find(|(name, _)| name.eq_ignore_ascii_case(key)).map(|(_, slot)| *slot = (!raw.is_empty()).then(|| raw.to_string()));
        if secret.is_none() {
            let mut json = serde_json::to_value(&cfg)?;
            let slot = key.split_once('.').and_then(|(section, field)| json.get_mut(section)?.get_mut(field));
            let Some(slot) = slot else { bail!("unknown setting `{key}`; settings are section.key, see `august config`") };
            *slot = match slot {
                Value::String(_) => Value::from(raw),
                _ => serde_json::from_str(raw).unwrap_or_else(|_| Value::from(raw)),
            };
            let secrets = cfg.secrets;
            cfg = serde_json::from_value(json).with_context(|| format!("`{raw}` is not a valid value for {key}"))?;
            cfg.secrets = secrets;
        }
        cfg.save()
    }

    /// The settings with secrets summarised, for `august config` and /config.
    pub fn show() -> Result<String> {
        let mut cfg = Self::load()?;
        let mut out = serde_json::to_string_pretty(&cfg)? + "\n";
        out += &format!("\n{}:\n", env_path().display());
        for (name, v) in cfg.secrets.slots() {
            out += &format!("  {name}: {}\n", if v.is_some() { "set" } else { "not set" });
        }
        Ok(out)
    }
}

/// One-time move from the flat config.json of earlier versions; the old file is kept as config.json.old.
fn migrate_flat(j: &Value) -> Result<Config> {
    let s = |k: &str| j[k].as_str().map(str::to_string);
    let d = Config::default();
    let cfg = Config {
        model: Model { provider: s("provider").unwrap_or_default(), name: s("model").unwrap_or_default(), effort: s("effort").unwrap_or(d.model.effort) },
        tools: Tools { web_search: j["web_search"].as_bool().unwrap_or(true), approvals: j["approvals"].as_bool().unwrap_or(true) },
        session: Session { compact_at: j["context_tokens"].as_u64().unwrap_or(d.session.compact_at) },
        memory: Memory { review_every: j["review_every"].as_u64().map(|n| n as usize).unwrap_or(d.memory.review_every) },
        telegram: Telegram { allowed: serde_json::from_value(j["telegram_allowed"].clone()).unwrap_or_default() },
        voice: Voice { model: s("transcribe_model").unwrap_or(d.voice.model) },
        secrets: Secrets { anthropic_key: s("anthropic_key"), openai_key: s("openai_key"), telegram_token: s("telegram_token") },
    };
    std::fs::copy(path(), home().join("config.json.old"))?;
    cfg.save()?;
    Ok(cfg)
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Every setting is described in the schema, and the schema has nothing else.
    #[test]
    fn schema_matches_config() {
        let schema: Value = serde_json::from_str(include_str!("../config.schema.json")).unwrap();
        let cfg = serde_json::to_value(Config::default()).unwrap();
        let keys = |v: &Value| v.as_object().unwrap().keys().cloned().collect::<Vec<_>>();
        let mut sections = keys(&schema["properties"]);
        sections.retain(|k| k != "$schema");
        assert_eq!(sections, keys(&cfg));
        for section in sections {
            let props = &schema["properties"][&section]["properties"];
            assert_eq!(keys(props), keys(&cfg[&section]), "{section}");
            for (key, default) in cfg[&section].as_object().unwrap() {
                assert_eq!(&props[key]["default"], default, "{section}.{key}");
                assert!(props[key]["description"].is_string(), "{section}.{key}");
            }
        }
    }

    #[test]
    fn parses_env() {
        let mut s = Secrets::parse("# keys\nANTHROPIC_API_KEY=\"sk-a\"\nOTHER=x\nTELEGRAM_BOT_TOKEN=\n");
        assert_eq!(s.anthropic_key.as_deref(), Some("sk-a"));
        assert_eq!(s.telegram_token, None);
        assert_eq!(s.render(), "ANTHROPIC_API_KEY=sk-a\n");
    }
}
