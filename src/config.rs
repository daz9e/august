//! Files under `~/.august` (or `$AUGUST_HOME`). Settings live in one file per unit, owned by
//! the user (mode 0600, secrets included):
//!
//! ```text
//! config/august.json               provider, model, effort, home thread
//! config/extensions/<name>.json    enabled, origin, settings
//! secrets/<name>.json              an extension's secrets: API keys of accounts, tokens
//! ```

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Where one running August lives: its home (files under `~/.august`), the workspace its
/// tools operate in, and the environment it takes overrides from (`AUGUST_PROVIDER`, ...).
#[derive(Clone)]
pub struct Root(Arc<RootInner>);

struct RootInner {
    home: PathBuf,
    workspace: PathBuf,
    env: HashMap<String, String>,
}

impl Root {
    /// `home` and `workspace` (an absolute path) as given, reading `env`.
    pub fn new(home: PathBuf, workspace: PathBuf, env: HashMap<String, String>) -> Self {
        Self(Arc::new(RootInner { home, workspace, env }))
    }

    /// From the process: `home()`, `$AUGUST_WORKSPACE` (default `./workspace`, created) and
    /// its environment.
    pub fn from_env() -> Result<Self> {
        let workspace = PathBuf::from(crate::util::env_or("AUGUST_WORKSPACE", "./workspace"));
        std::fs::create_dir_all(&workspace)?;
        Ok(Self::new(home(), workspace.canonicalize()?, std::env::vars().collect()))
    }

    pub fn home(&self) -> &Path {
        &self.0.home
    }

    pub fn workspace(&self) -> &Path {
        &self.0.workspace
    }

    /// An environment variable, unless unset or empty.
    pub fn env(&self, key: &str) -> Option<String> {
        self.0.env.get(key).filter(|v| !v.is_empty()).cloned()
    }

    /// Whether `key` is set at all (even empty).
    pub fn env_set(&self, key: &str) -> bool {
        self.0.env.contains_key(key)
    }
}

/// A unit's, extension's or skill's name: lowercase letters, digits, `-` and `_` (max 64).
pub fn valid_name(name: &str) -> bool {
    !name.is_empty() && name.len() <= 64 && name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
}

pub fn home() -> PathBuf {
    std::env::var("AUGUST_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into())).join(".august")
        })
}

/// August's own settings (`config/august.json`): the active provider and model, chosen
/// with `/login` / `/model` (or `august login` / `august model`), and the home thread.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Config {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    /// The thread extensions reach as `"home"` (`messenger:id`); set with `/home`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub home: Option<String>,
}

/// JSON Schema of `august.json`, in the shape extensions describe their settings with.
pub fn app_schema() -> serde_json::Value {
    serde_json::json!({"type": "object", "properties": {
        "provider": {"type": "string", "description": "The active model provider (an account's provider id); chosen with /login"},
        "model": {"type": "string", "description": "The active model of that provider; /models lists them"},
        "effort": {"type": "string", "description": "Reasoning effort for models that have it (low, medium, high)"},
        "home": {"type": "string", "description": "The thread extensions report to (messenger:id); set with /home"},
        "hooks": {"type": "object", "description": "Hook chains: `order` lists extensions whose handlers run first, in that order (the rest after, by name)"},
    }})
}

// ---- units -------------------------------------------------------------------

/// Kinds of units with a file each under `config/<kind>/`.
pub const KINDS: [&str; 1] = ["extensions"];

/// Fields whose values are secrets wherever they appear: shown as `••••`.
const SECRET_FIELDS: [&str; 5] = ["key", "token", "api_key", "password", "secret"];
pub const MASK: &str = "••••";

/// `config/<kind>/<id>.json` relative to the config folder; `august.json` for `august`.
fn unit_rel(kind: &str, id: &str) -> Result<PathBuf> {
    if kind == "august" {
        return Ok(PathBuf::from("august.json"));
    }
    anyhow::ensure!(KINDS.contains(&kind), "unknown kind `{kind}` (august | {})", KINDS.join(" | "));
    anyhow::ensure!(valid_name(id), "bad unit name `{id}`");
    Ok(PathBuf::from(kind).join(format!("{id}.json")))
}

fn read_json(path: &std::path::Path) -> Result<serde_json::Value> {
    match std::fs::read_to_string(path) {
        Ok(s) => serde_json::from_str(&s).with_context(|| format!("parse {}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(serde_json::json!({})),
        Err(e) => Err(e).with_context(|| format!("read {}", path.display())),
    }
}

/// Atomically writes `path` with mode 0600.
fn write_json(path: &std::path::Path, value: &serde_json::Value) -> Result<()> {
    std::fs::create_dir_all(path.parent().context("no parent folder")?)?;
    let tmp = path.with_extension("json.tmp");
    {
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut opts, 0o600);
        let mut f = opts.open(&tmp)?;
        std::io::Write::write_all(&mut f, serde_json::to_string_pretty(value)?.as_bytes())?;
    }
    std::fs::rename(&tmp, path).with_context(|| format!("write {}", path.display()))
}

/// A dotted path into the settings: `august.model`, `extensions.browser.settings.headless`.
/// Returns `(kind, id, path inside the unit's file)`.
pub fn split_path(path: &str) -> Result<(String, String, Vec<String>)> {
    let mut parts = path.split('.').filter(|p| !p.is_empty()).map(String::from);
    let kind = parts.next().context("empty path")?;
    let id = if kind == "august" { String::new() } else { parts.next().with_context(|| format!("`{kind}` needs a name: {kind}.<name>"))? };
    unit_rel(&kind, &id)?;
    Ok((kind, id, parts.collect()))
}

impl Root {
    /// `config/august.json`, or `config/<kind>/<id>.json`.
    fn unit_file(&self, kind: &str, id: &str) -> Result<PathBuf> {
        Ok(self.home().join("config").join(unit_rel(kind, id)?))
    }

    /// A unit's settings (`{}` when it has no file). `kind` is `august` (id ignored) or one of `KINDS`.
    pub fn unit(&self, kind: &str, id: &str) -> Result<serde_json::Value> {
        read_json(&self.unit_file(kind, id)?)
    }

    pub fn save_unit(&self, kind: &str, id: &str, value: &serde_json::Value) -> Result<()> {
        write_json(&self.unit_file(kind, id)?, value)
    }

    pub fn app(&self) -> Result<Config> {
        Ok(serde_json::from_value(self.unit("august", "")?)?)
    }

    pub fn save_app(&self, cfg: &Config) -> Result<()> {
        // Fields this version doesn't know stay.
        let mut v = self.unit("august", "")?;
        let o = v.as_object_mut().context("august.json must be an object")?;
        for k in ["provider", "model", "effort", "home"] {
            o.remove(k);
        }
        if let serde_json::Value::Object(known) = serde_json::to_value(cfg)? {
            o.extend(known);
        }
        self.save_unit("august", "", &v)
    }

    /// `secrets/<ext>.json` (mode 0600): an extension's secrets by key (an account's API key
    /// under the account's id, tokens, ...). Only August reads them, for that extension.
    fn secrets_file(&self, ext: &str) -> Result<PathBuf> {
        anyhow::ensure!(valid_name(ext), "bad extension name `{ext}`");
        Ok(self.home().join("secrets").join(format!("{ext}.json")))
    }

    pub fn secret(&self, ext: &str, key: &str) -> Result<Option<String>> {
        Ok(read_json(&self.secrets_file(ext)?)?[key].as_str().map(String::from))
    }

    /// Keeps (or with `None` deletes) secret `key` of `ext`.
    pub fn set_secret(&self, ext: &str, key: &str, value: Option<&str>) -> Result<()> {
        let path = self.secrets_file(ext)?;
        let mut all = read_json(&path)?;
        let o = all.as_object_mut().context("secrets must be an object")?;
        match value {
            Some(v) => o.insert(key.into(), v.into()),
            None => o.remove(key),
        };
        write_json(&path, &all)
    }

    /// The value at `path` (null if unset).
    pub fn get(&self, path: &str) -> Result<serde_json::Value> {
        let (kind, id, inner) = split_path(path)?;
        let mut v = self.unit(&kind, &id)?;
        for p in &inner {
            v = v.get(p).cloned().unwrap_or(serde_json::Value::Null);
        }
        Ok(v)
    }

    /// Sets the value at `path`; null deletes it.
    pub fn set(&self, path: &str, value: serde_json::Value) -> Result<()> {
        let (kind, id, inner) = split_path(path)?;
        let mut root = self.unit(&kind, &id)?;
        let Some((last, parents)) = inner.split_last() else {
            anyhow::ensure!(value.is_object() || value.is_null(), "a unit's settings are an object");
            let value = if value.is_null() { serde_json::json!({}) } else { value };
            return self.save_unit(&kind, &id, &value);
        };
        let mut node = &mut root;
        for p in parents {
            if !node[p].is_object() {
                node[p] = serde_json::json!({});
            }
            node = &mut node[p];
        }
        let obj = node.as_object_mut().context("not an object")?;
        if value.is_null() {
            obj.remove(last);
        } else {
            obj.insert(last.clone(), value);
        }
        self.save_unit(&kind, &id, &root)
    }
}

/// `value` with secrets shown as `••••`: the usual secret field names, and `extra` ones
/// (an extension's schema marks its own).
pub fn masked(value: &serde_json::Value, extra: &[String]) -> serde_json::Value {
    match value {
        serde_json::Value::Object(o) => o
            .iter()
            .map(|(k, v)| {
                let secret = SECRET_FIELDS.contains(&k.as_str()) || extra.contains(k);
                let shown = if secret && !v.is_null() && v != "" { MASK.into() } else { masked(v, extra) };
                (k.clone(), shown)
            })
            .collect(),
        serde_json::Value::Array(a) => a.iter().map(|v| masked(v, extra)).collect(),
        other => other.clone(),
    }
}

/// Whether the last segment of `path` names a secret.
pub fn is_secret_path(path: &str, extra: &[String]) -> bool {
    path.rsplit('.').next().is_some_and(|k| SECRET_FIELDS.contains(&k) || extra.iter().any(|e| e == k))
}
