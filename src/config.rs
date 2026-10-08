//! Files under `~/.august` (or `$AUGUST_HOME`). Settings live in one file per unit, owned by
//! the user (mode 0600, secrets included):
//!
//! ```text
//! config/august.json               provider, model, effort, fallback, home thread
//! config/providers/<id>.json       key, base_url; a provider of the user's own adds format, ...
//! config/messengers/<id>.json      e.g. telegram: token, allowed
//! config/extensions/<name>.json    enabled, origin, settings
//! ```
//!
//! Older homes (`config.json`, `credentials.json`, `providers.json`, `channels.json`,
//! `disabled` markers) are moved into this layout on first use; the old files are kept in
//! `config/.migrated/`.

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


/// August's own settings (`config/august.json`): the active provider and model, chosen
/// with `august login` / `august model`, and the home thread.
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
    /// The thread extensions reach as `"home"` (`messenger:id`); set with `/home`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub home: Option<String>,
}

/// API keys by provider id (from `config/providers/<id>.json`). ChatGPT OAuth tokens live
/// in `chatgpt-auth.json`.
pub type Credentials = BTreeMap<String, ApiCredential>;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiCredential {
    pub key: String,
    /// OpenAI-compatible endpoints only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
}

// ---- units -------------------------------------------------------------------

/// Kinds of units with a file each under `config/<kind>/`.
pub const KINDS: [&str; 3] = ["providers", "messengers", "extensions"];

/// Fields whose values are secrets wherever they appear: shown as `••••`.
const SECRET_FIELDS: [&str; 5] = ["key", "token", "api_key", "password", "secret"];
pub const MASK: &str = "••••";

fn config_dir() -> PathBuf {
    static MIGRATED: std::sync::Once = std::sync::Once::new();
    MIGRATED.call_once(|| {
        if let Err(e) = migrate() {
            eprintln!("could not move old settings into {}: {e:#}", home().join("config").display());
        }
    });
    home().join("config")
}

/// `config/august.json`, or `config/<kind>/<id>.json`.
fn unit_file(kind: &str, id: &str) -> Result<PathBuf> {
    if kind == "august" {
        return Ok(config_dir().join("august.json"));
    }
    anyhow::ensure!(KINDS.contains(&kind), "unknown kind `{kind}` (august | {})", KINDS.join(" | "));
    anyhow::ensure!(valid_name(id), "bad unit name `{id}`");
    Ok(config_dir().join(kind).join(format!("{id}.json")))
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
    std::fs::create_dir_all(path.parent().unwrap_or(&home()))?;
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

/// A unit's settings (`{}` when it has no file). `kind` is `august` (id ignored) or one of `KINDS`.
pub fn unit(kind: &str, id: &str) -> Result<serde_json::Value> {
    read_json(&unit_file(kind, id)?)
}

pub fn save_unit(kind: &str, id: &str, value: &serde_json::Value) -> Result<()> {
    write_json(&unit_file(kind, id)?, value)
}

/// Every unit of `kind` with a file, by id.
pub fn units(kind: &str) -> Result<BTreeMap<String, serde_json::Value>> {
    let mut out = BTreeMap::new();
    let dir = config_dir().join(kind);
    for e in std::fs::read_dir(&dir).into_iter().flatten().filter_map(|e| e.ok()) {
        let name = e.file_name().to_string_lossy().to_string();
        if let Some(id) = name.strip_suffix(".json") {
            out.insert(id.to_string(), read_json(&e.path())?);
        }
    }
    Ok(out)
}

pub fn app() -> Result<Config> {
    Ok(serde_json::from_value(unit("august", "")?)?)
}

pub fn save_app(cfg: &Config) -> Result<()> {
    // Fields this version doesn't know stay.
    let mut v = unit("august", "")?;
    let o = v.as_object_mut().context("august.json must be an object")?;
    for k in ["provider", "model", "effort", "fallback", "home"] {
        o.remove(k);
    }
    if let serde_json::Value::Object(known) = serde_json::to_value(cfg)? {
        o.extend(known);
    }
    save_unit("august", "", &v)
}

/// Stored API keys of every provider.
pub fn credentials() -> Result<Credentials> {
    Ok(units("providers")?
        .into_iter()
        .filter_map(|(id, v)| {
            let key = v["key"].as_str()?.to_string();
            Some((id, ApiCredential { key, base_url: v["base_url"].as_str().map(String::from) }))
        })
        .collect())
}

/// Stores (or with `None` removes) a provider's key; its other settings stay.
pub fn save_credential(id: &str, cred: Option<&ApiCredential>) -> Result<()> {
    let mut v = unit("providers", id)?;
    let o = v.as_object_mut().context("provider settings must be an object")?;
    o.remove("key");
    o.remove("base_url");
    if let Some(c) = cred {
        o.insert("key".into(), c.key.clone().into());
        if let Some(url) = &c.base_url {
            o.insert("base_url".into(), url.clone().into());
        }
    }
    save_unit("providers", id, &v)
}

/// A dotted path into the settings: `august.model`, `providers.openai.base_url`,
/// `messengers.telegram.allowed`, `extensions.browser.settings.headless`.
/// Returns `(kind, id, path inside the unit's file)`.
pub fn split_path(path: &str) -> Result<(String, String, Vec<String>)> {
    let mut parts = path.split('.').filter(|p| !p.is_empty()).map(String::from);
    let kind = parts.next().context("empty path")?;
    let id = if kind == "august" { String::new() } else { parts.next().with_context(|| format!("`{kind}` needs a name: {kind}.<name>"))? };
    unit_file(&kind, &id)?;
    Ok((kind, id, parts.collect()))
}

/// The value at `path` (null if unset).
pub fn get(path: &str) -> Result<serde_json::Value> {
    let (kind, id, inner) = split_path(path)?;
    let mut v = unit(&kind, &id)?;
    for p in &inner {
        v = v.get(p).cloned().unwrap_or(serde_json::Value::Null);
    }
    Ok(v)
}

/// Sets the value at `path`; null deletes it.
pub fn set(path: &str, value: serde_json::Value) -> Result<()> {
    let (kind, id, inner) = split_path(path)?;
    let mut root = unit(&kind, &id)?;
    let Some((last, parents)) = inner.split_last() else {
        anyhow::ensure!(value.is_object() || value.is_null(), "a unit's settings are an object");
        let value = if value.is_null() { serde_json::json!({}) } else { value };
        return save_unit(&kind, &id, &value);
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
    save_unit(&kind, &id, &root)
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

/// Moves settings of an older home into `config/`: `config.json` → `august.json`,
/// `credentials.json` and `providers.json` → `providers/`, `channels.json` → `messengers/`,
/// `disabled` markers → `extensions/<name>.json`. Runs once, when `config/` doesn't exist.
fn migrate() -> Result<()> {
    let (home, dir) = (home(), home().join("config"));
    if dir.exists() {
        return Ok(());
    }
    let old = |name: &str| home.join(name);
    let found = ["config.json", "credentials.json", "providers.json", "channels.json"].iter().any(|n| old(n).exists());
    let markers: Vec<(String, PathBuf)> = [home.join("extensions"), home.join("extensions/.runtime/defaults")]
        .iter()
        .flat_map(|d| std::fs::read_dir(d).into_iter().flatten().filter_map(|e| e.ok()))
        .map(|e| (e.file_name().to_string_lossy().to_string(), e.path().join("disabled")))
        .filter(|(_, m)| m.exists())
        .collect();
    if !found && markers.is_empty() {
        return Ok(());
    }
    let write = |kind: &str, id: &str, v: serde_json::Value| -> Result<()> {
        let path = if kind == "august" { dir.join("august.json") } else { dir.join(kind).join(format!("{id}.json")) };
        let mut merged = read_json(&path)?;
        for (k, val) in v.as_object().into_iter().flatten() {
            merged[k] = val.clone();
        }
        write_json(&path, &merged)
    };
    let objects = |name: &str| -> Result<BTreeMap<String, serde_json::Value>> {
        match read_json(&old(name))? {
            serde_json::Value::Object(o) => Ok(o.into_iter().collect()),
            _ => Ok(BTreeMap::new()),
        }
    };
    if old("config.json").exists() {
        write("august", "", read_json(&old("config.json"))?)?;
    }
    let mut providers = objects("providers.json")?;
    for (id, v) in objects("credentials.json")? {
        let entry = providers.entry(id).or_insert_with(|| serde_json::json!({}));
        for (k, val) in v.as_object().into_iter().flatten() {
            entry[k] = val.clone();
        }
    }
    // OpenAI-compatible providers are settings of the `openai` extension.
    let mut openai = serde_json::json!({});
    for (id, v) in providers {
        if id == "openai" {
            openai["key"] = v["key"].clone();
            openai["base_url"] = v["base_url"].clone();
        } else if v["format"] == "openai" {
            openai["endpoints"][&id] = v;
        } else {
            write("providers", &id, v)?;
        }
    }
    if openai.as_object().is_some_and(|o| o.values().any(|v| !v.is_null())) {
        write("extensions", "openai", serde_json::json!({"settings": openai}))?;
    }
    for (id, v) in objects("channels.json")? {
        write("messengers", &id, v)?;
    }
    for (name, marker) in &markers {
        write("extensions", name, serde_json::json!({"enabled": false}))?;
        std::fs::remove_file(marker).ok();
    }
    let keep = dir.join(".migrated");
    std::fs::create_dir_all(&keep)?;
    for name in ["config.json", "credentials.json", "providers.json", "channels.json"] {
        if old(name).exists() {
            std::fs::rename(old(name), keep.join(name))?;
        }
    }
    Ok(())
}
