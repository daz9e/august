//! Extensions add tools, slash commands and hooks. User extensions are TypeScript files in
//! `~/.august/extensions/<name>/index.ts`, run by bun through `host.ts`. Default extensions
//! (the repo's `extensions/<name>/main.rs`) are Rust binaries `august-ext-<name>` next to
//! `august`, speaking the same protocol; a user extension of the same name replaces one.
//! Each extension is its own process (see `host.rs`), so a broken or hanging one can't take
//! the gateway down.

mod host;
mod logs;

pub use host::{AccountInfo, ProviderInfo, RpcError};

use crate::config::Root;
use crate::llm::ToolSpec;
use crate::messengers::Thread;
use anyhow::Result;
use async_trait::async_trait;
use host::{Exit, Host};
use logs::Log;
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock, Weak};
use std::time::Duration;

const HOST_TS: &str = include_str!("host.ts");
const TYPES: &str = include_str!("august.d.ts");
const GUIDE: &str = include_str!("guide.md");

const EVENT_TIMEOUT: Duration = Duration::from_secs(10);
/// `message_in` may do real work on attachments (e.g. transcribe a voice note).
const MESSAGE_TIMEOUT: Duration = Duration::from_secs(120);
/// Long enough for a command to do real work (`/compact` summarising a big conversation).
const COMMAND_TIMEOUT: Duration = Duration::from_secs(600);
/// Observe-only hooks run in the background, so they may take long.
const OBSERVER_TIMEOUT: Duration = Duration::from_secs(3600);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);
const TOOL_TIMEOUT: Duration = Duration::from_secs(600);
/// Events whose handlers only observe: what they return is ignored.
const OBSERVERS: &[&str] = &["turn_end", "turn_settled", "turn_event", "llm_result", "session_changed", "reaction", "message_edited", "extension_state", "extension_output", "config_changed", "stop"];
/// How deep events extensions emit may nest (a handler emitting another, ...).
const MAX_DEPTH: u32 = 8;
const ENTRIES: [&str; 3] = ["index.ts", "index.js", "index.mjs"];
/// A folder with this file is an extension in any language: `{command: [argv], env}`.
const MANIFEST: &str = "extension.json";

/// What extensions can ask of August: the operations of the core's table.
#[async_trait]
pub trait Core: Send + Sync {
    /// Runs operation `op` for extension `ext`, whose permission was already checked.
    async fn call(&self, ext: &str, op: &str, params: &Value) -> Result<Value>;
    /// What extensions offer changed (one was loaded, or registered something at runtime).
    fn changed(&self);
}

/// Where a hook, tool or command runs: its thread, and the turn when inside one.
#[derive(Debug, Clone, Default)]
pub struct Origin {
    pub thread: Option<Thread>,
    pub turn: Option<crate::agent::TurnTag>,
    /// How many extension events this call is nested in.
    pub depth: u32,
}

impl Origin {
    pub fn thread(thread: Thread) -> Self {
        Self { thread: Some(thread), ..Default::default() }
    }
}

#[derive(Clone)]
enum State {
    /// Being launched: its `extension_launch` hooks, the process, its `extension_ready` hooks.
    Starting,
    Running(Arc<Host>),
    Failed(String),
    Disabled,
}

/// Both ends of a link to an extension: what it writes, and where August writes to it.
pub type Io = (Box<dyn tokio::io::AsyncRead + Send + Unpin>, Box<dyn tokio::io::AsyncWrite + Send + Unpin>);
/// Starts an extension August reaches over a link of the embedder's (e.g. one running in the
/// same process) and returns that link; called at each start and restart.
pub type Link = Arc<dyn Fn() -> Io + Send + Sync>;

/// How an extension is started.
#[derive(Clone)]
enum Launch {
    /// A TypeScript or JavaScript entry file, run by bun through `host.ts`.
    Script(PathBuf),
    /// A folder with `extension.json`: its `command`, in any language.
    Command(PathBuf),
    /// A default extension's binary; `dir` is its folder (state files).
    Binary { exe: PathBuf, dir: PathBuf },
    /// Reached over a link the embedder makes (`Extensions::link`).
    Linked(Link),
}

impl Launch {
    fn dir(&self) -> PathBuf {
        match self {
            Launch::Script(entry) => entry.parent().unwrap_or(Path::new(".")).to_path_buf(),
            Launch::Command(dir) | Launch::Binary { dir, .. } => dir.clone(),
            Launch::Linked(_) => PathBuf::new(),
        }
    }

    /// What tells whether it changed since it last ran: its folder, or a default's binary.
    fn source(&self) -> PathBuf {
        match self {
            Launch::Binary { exe, .. } => exe.clone(),
            other => other.dir(),
        }
    }
}

/// The process to start: its argv, extra environment and folder.
struct Spec {
    command: Vec<String>,
    env: serde_json::Map<String, Value>,
    dir: PathBuf,
}

impl Spec {
    fn json(&self) -> (Value, Value) {
        (json!(self.command), Value::Object(self.env.clone()))
    }

    /// Takes `command` and `env` as `extension_launch` hooks left them.
    fn update(&mut self, data: &Value) -> Result<(), String> {
        let command: Option<Vec<String>> = data["command"].as_array().and_then(|a| a.iter().map(|x| x.as_str().map(String::from)).collect());
        self.command = command.filter(|c| !c.is_empty()).ok_or("`command` must be a non-empty list of strings")?;
        let env = data["env"].as_object().cloned().unwrap_or_default();
        if env.values().any(|v| !v.is_string()) {
            return Err("`env` values must be strings".into());
        }
        self.env = env;
        Ok(())
    }

    fn command(&self) -> tokio::process::Command {
        // A relative program (`./weather`) is the extension's own, in its folder.
        let program = Path::new(&self.command[0]);
        let program = if program.components().count() > 1 && program.is_relative() { self.dir.join(program) } else { program.to_path_buf() };
        let mut c = tokio::process::Command::new(program);
        c.args(&self.command[1..]).current_dir(&self.dir);
        for (k, v) in &self.env {
            c.env(k, v.as_str().unwrap_or_default());
        }
        c
    }
}

struct Slot {
    name: String,
    launch: Launch,
    state: State,
    /// Identifies the process; exits of replaced processes are ignored.
    generation: u64,
    /// Restarts since it last ran stably.
    restarts: u32,
    /// Why it is in its state: `start`, `install`, `exit`, `hang`, ...
    reason: String,
    /// Nobody will restart it (until /reload, a save or an enable).
    gave_up: bool,
    /// What its health check last said when not ok: `(status, detail)`.
    health: Option<(String, String)>,
}

impl Slot {
    fn new(name: String, launch: Launch, state: State, reason: &str) -> Self {
        Slot { name, launch, state, generation: 0, restarts: 0, reason: reason.into(), gave_up: false, health: None }
    }
}

/// How the core watches an extension: `august.supervise` in `august.json`, overridden per
/// extension by `supervise` in its own settings file.
struct Supervise {
    ping_interval: Duration,
    ping_timeout: Duration,
    ping_misses: u32,
    max_restarts: u32,
    stable_after: Duration,
}

fn supervise(root: &Root, name: &str) -> Supervise {
    let global = root.get("august.supervise").unwrap_or_default();
    let own = root.unit("extensions", name).map(|v| v["supervise"].clone()).unwrap_or_default();
    let num = |k: &str, default: u64| own[k].as_u64().or(global[k].as_u64()).unwrap_or(default);
    Supervise {
        ping_interval: Duration::from_millis(num("ping_interval_ms", 30_000).max(100)),
        ping_timeout: Duration::from_millis(num("ping_timeout_ms", 10_000).max(100)),
        ping_misses: num("ping_misses", 2).max(1) as u32,
        max_restarts: num("max_restarts", 5) as u32,
        stable_after: Duration::from_millis(num("stable_after_ms", 600_000)),
    }
}

/// `(size, modified)` of every file under `src` (or of `src` itself), by relative path.
// ponytail: metadata, not contents; skips hidden folders and the usual build and dependency
// caches (`target`, `node_modules`, `__pycache__`), so a setup's outputs don't count as changes.
fn fingerprint(src: &Path) -> std::collections::BTreeMap<String, (u64, u128)> {
    fn walk(root: &Path, dir: &Path, out: &mut std::collections::BTreeMap<String, (u64, u128)>) {
        for e in std::fs::read_dir(dir).into_iter().flatten().filter_map(|e| e.ok()) {
            let (path, name) = (e.path(), e.file_name().to_string_lossy().to_string());
            let Ok(meta) = e.metadata() else { continue };
            if meta.is_dir() {
                if !name.starts_with('.') && !["target", "node_modules", "__pycache__"].contains(&name.as_str()) {
                    walk(root, &path, out);
                }
            } else if let Ok(rel) = path.strip_prefix(root) {
                out.insert(rel.to_string_lossy().into(), stamp(&meta));
            }
        }
    }
    fn stamp(meta: &std::fs::Metadata) -> (u64, u128) {
        let modified = meta.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map_or(0, |d| d.as_nanos());
        (meta.len(), modified)
    }
    let mut out = std::collections::BTreeMap::new();
    match std::fs::metadata(src) {
        Ok(m) if m.is_dir() => walk(src, src, &mut out),
        Ok(m) => {
            out.insert(src.file_name().unwrap_or_default().to_string_lossy().into(), stamp(&m));
        }
        Err(_) => {}
    }
    out
}

/// Events only extensions with `admin` may hook: their handlers can keep any extension from
/// running, or read what it writes.
const GUARDED: &[&str] = &["extension_launch", "extension_ready", "extension_exit", "extension_output"];

pub struct Extensions {
    root: Root,
    dir: PathBuf,
    /// Where the default extensions' binaries are (`august-ext-<name>`); none: no defaults.
    defaults: Option<PathBuf>,
    core: RwLock<Option<Arc<dyn Core>>>,
    /// Extensions reached over links of the embedder's, by name.
    linked: RwLock<Vec<(String, Link)>>,
    slots: RwLock<Vec<Slot>>,
    generation: AtomicU64,
    me: Weak<Extensions>,
}

/// The extension-writing guide with the API types (op `guide`), for extensions in `dir`.
pub fn guide(dir: &Path) -> String {
    format!("Extensions live in {}.\n\n{GUIDE}\n```ts\n{TYPES}```\n", dir.display())
}

pub fn valid_name(name: &str) -> bool {
    crate::config::valid_name(name)
}

/// `$AUGUST_BUN`, or `bun` on PATH (the gateway's PATH includes the login shell's).
fn find_bun(root: &Root) -> Option<PathBuf> {
    if let Some(p) = root.env("AUGUST_BUN") {
        return Some(PathBuf::from(p));
    }
    let path = root.env("PATH").unwrap_or_default();
    std::env::split_paths(&path).map(|d| d.join("bun")).find(|p| p.is_file())
}

fn entry_of(folder: &Path) -> Option<Launch> {
    if folder.join(MANIFEST).is_file() {
        return Some(Launch::Command(folder.to_path_buf()));
    }
    ENTRIES.iter().map(|e| folder.join(e)).find(|p| p.is_file()).map(Launch::Script)
}

/// Names of the properties a settings schema marks `"secret": true`.
pub fn secret_fields(schema: &Value) -> Vec<String> {
    schema["properties"].as_object().into_iter().flatten().filter(|(_, p)| p["secret"] == true).map(|(k, _)| k.clone()).collect()
}

/// `settings` with the schema's defaults filled in where unset.
pub fn with_defaults(schema: &Value, settings: &Value) -> Value {
    let mut out = if settings.is_object() { settings.clone() } else { json!({}) };
    for (k, p) in schema["properties"].as_object().into_iter().flatten() {
        if out.get(k).is_none_or(Value::is_null) && let Some(d) = p.get("default") {
            out[k] = d.clone();
        }
    }
    out
}

/// An extension starts unless the user turned it off (`enabled: false` in its settings).
fn enabled(root: &Root, name: &str) -> bool {
    root.unit("extensions", name).map_or(true, |v| v["enabled"] != false)
}

/// The folder of the running binary, where the extensions that ship with August are.
pub fn bin_dir() -> PathBuf {
    let exe = std::env::current_exe().unwrap_or_default();
    exe.parent().unwrap_or(Path::new(".")).to_path_buf()
}

/// `august-ext-<name>` in `bin`.
fn default_binary(bin: &Path, name: &str) -> PathBuf {
    bin.join(format!("august-ext-{name}"))
}

/// The extensions that ship with August: binaries `august-ext-<name>` in `bin`.
fn shipped(bin: Option<&Path>) -> Vec<String> {
    let Some(bin) = bin else { return Vec::new() };
    let names = std::fs::read_dir(bin).into_iter().flatten().filter_map(|e| e.ok());
    let mut names: Vec<String> = names
        .filter_map(|e| e.file_name().to_str()?.strip_prefix("august-ext-").map(String::from))
        .filter(|n| valid_name(n) && default_binary(bin, n).is_file())
        .collect();
    names.sort();
    names
}

/// Defaults, extensions linked in and those marked `early` start before the rest.
fn early(root: &Root, name: &str, launch: &Launch) -> bool {
    matches!(launch, Launch::Binary { .. } | Launch::Linked(_)) || root.unit("extensions", name).is_ok_and(|v| v["early"] == true)
}

/// Why a hook chain stopped something.
fn block_text(data: &Value) -> String {
    data["block"].as_str().filter(|s| !s.is_empty()).unwrap_or("by a hook").to_string()
}

/// What an extension registered, as hooks (`extension_ready`) see it.
fn manifest_json(m: &host::Manifest) -> Value {
    json!({
        "summary": m.summary,
        "details": m.details,
        "tools": m.tools.iter().map(|t| json!({"name": t.name, "description": t.description, "parameters": t.input_schema})).collect::<Vec<_>>(),
        "commands": m.commands.iter().map(|(n, d)| json!({"name": n, "description": d})).collect::<Vec<_>>(),
        "hooks": m.events,
        "needs": m.needs,
        "settings": m.settings,
        "emits": m.emits.iter().map(|d| json!({"name": d.name, "description": d.description, "observe": d.observe})).collect::<Vec<_>>(),
        "replaces": m.replaces,
        "takes": m.takes,
        "accounts": m.accounts.iter().map(|a| &a.id).collect::<Vec<_>>(),
        "providers": m.providers.iter().map(|p| &p.id).collect::<Vec<_>>(),
        "messengers": m.messengers.iter().map(|d| &d.id).collect::<Vec<_>>(),
    })
}

/// `(memory KB, CPU %)` of every process group, summed over its processes.
fn usage() -> HashMap<i32, (u64, f64)> {
    let out = std::process::Command::new("ps").args(["-axo", "pgid=,rss=,%cpu="]).output();
    let mut all: HashMap<i32, (u64, f64)> = HashMap::new();
    for line in out.map(|o| String::from_utf8_lossy(&o.stdout).into_owned()).unwrap_or_default().lines() {
        let f: Vec<&str> = line.split_whitespace().collect();
        if let [pgid, rss, cpu] = f[..] {
            let e = all.entry(pgid.parse().unwrap_or(0)).or_default();
            e.0 += rss.parse::<u64>().unwrap_or(0);
            e.1 += cpu.parse::<f64>().unwrap_or(0.0);
        }
    }
    all
}

fn stopped(data: &Value) -> bool {
    let block = &data["block"];
    block == true || block.as_str().is_some_and(|s| !s.is_empty()) || data["handled"] == true
}

impl Extensions {
    /// Extensions of `root` (in `home/extensions`), with the defaults in `defaults`.
    pub fn new(root: Root, defaults: Option<PathBuf>) -> Arc<Self> {
        Arc::new_cyclic(|me| Self {
            dir: root.home().join("extensions"),
            root,
            defaults,
            linked: RwLock::default(),
            core: RwLock::new(None),
            slots: RwLock::new(Vec::new()),
            generation: AtomicU64::new(0),
            me: me.clone(),
        })
    }

    /// Adds extension `name`, reached over the links `link` makes; it starts with the others.
    /// One in the extensions folder of the same name wins.
    pub fn link(&self, name: &str, link: Link) {
        let mut linked = self.linked.write().unwrap();
        linked.retain(|(n, _)| n != name);
        linked.push((name.into(), link));
    }

    pub fn set_core(&self, core: Arc<dyn Core>) {
        *self.core.write().unwrap() = Some(core);
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// The default extensions' folders.
    fn defaults_dir(&self) -> PathBuf {
        self.dir.join(".runtime/defaults")
    }

    /// Every extension, sorted by name: the user's ones, then the defaults they don't replace.
    fn discover(&self) -> Vec<(String, Launch)> {
        let mut found: Vec<(String, Launch)> = Vec::new();
        for e in std::fs::read_dir(&self.dir).into_iter().flatten().filter_map(|e| e.ok()) {
            let name = e.file_name().to_string_lossy().to_string();
            if valid_name(&name) && let Some(launch) = entry_of(&e.path()) {
                found.push((name, launch));
            }
        }
        for (name, link) in self.linked.read().unwrap().iter() {
            if !found.iter().any(|(n, _)| n == name) {
                found.push((name.clone(), Launch::Linked(link.clone())));
            }
        }
        for name in shipped(self.defaults.as_deref()) {
            if !found.iter().any(|(n, _)| *n == name) {
                let launch = Launch::Binary { exe: default_binary(self.defaults.as_deref().unwrap_or(Path::new(".")), &name), dir: self.defaults_dir().join(&name) };
                found.push((name, launch));
            }
        }
        found.sort_by(|a, b| a.0.cmp(&b.0));
        found
    }

    /// Folders of the defaults (dropping the TypeScript copies earlier versions wrote there).
    fn prepare_defaults(&self) -> std::io::Result<()> {
        for name in shipped(self.defaults.as_deref()) {
            let folder = self.defaults_dir().join(name);
            std::fs::create_dir_all(&folder)?;
            std::fs::remove_file(folder.join("index.ts")).ok();
        }
        Ok(())
    }

    /// Writes `host.ts` next to the extensions and returns `(bun, host.ts)`.
    fn runtime(&self) -> Result<(PathBuf, PathBuf), String> {
        let bun = find_bun(&self.root).ok_or("bun is not installed (https://bun.sh); set AUGUST_BUN to its path")?;
        let rt = self.dir.join(".runtime");
        let host = rt.join("host.ts");
        let write = || -> std::io::Result<()> {
            std::fs::create_dir_all(&rt)?;
            if std::fs::read_to_string(&host).ok().as_deref() != Some(HOST_TS) {
                std::fs::write(&host, HOST_TS)?;
            }
            if std::fs::read_to_string(rt.join("august.d.ts")).ok().as_deref() != Some(TYPES) {
                std::fs::write(rt.join("august.d.ts"), TYPES)?;
            }
            Ok(())
        };
        write().map_err(|e| format!("could not write {}: {e}", host.display()))?;
        Ok((bun, host))
    }

    /// The process `launch` stands for, before hooks change it.
    fn spec(&self, name: &str, launch: &Launch) -> Result<Spec, String> {
        let text = |p: &Path| p.display().to_string();
        match launch {
            Launch::Script(entry) => {
                let (bun, host) = self.runtime()?;
                Ok(Spec { command: vec![text(&bun), "run".into(), text(&host), text(entry), name.into()], env: Default::default(), dir: launch.dir() })
            }
            Launch::Command(dir) => {
                let file = dir.join(MANIFEST);
                let read = std::fs::read_to_string(&file).map_err(|e| format!("could not read {}: {e}", file.display()))?;
                let v: Value = serde_json::from_str(&read).map_err(|e| format!("{MANIFEST}: {e}"))?;
                let mut spec = Spec { command: Vec::new(), env: Default::default(), dir: dir.clone() };
                spec.update(&v).map_err(|e| format!("{MANIFEST}: {e}"))?;
                Ok(spec)
            }
            Launch::Linked(_) => Ok(Spec { command: Vec::new(), env: Default::default(), dir: launch.dir() }),
            Launch::Binary { exe, dir } => {
                if !exe.is_file() {
                    return Err(format!("{} is missing; build it with `cargo build`", exe.display()));
                }
                let mut env = serde_json::Map::new();
                env.insert("AUGUST_EXTENSION_DIR".into(), json!(text(dir)));
                env.insert("AUGUST_EXTENSIONS".into(), json!(text(&self.dir)));
                Ok(Spec { command: vec![text(exe)], env, dir: dir.clone() })
            }
        }
    }

    /// Where the core remembers what each extension looked like when it last started.
    fn launched_file(&self) -> PathBuf {
        self.dir.join(".runtime/launched.json")
    }

    fn launched(&self) -> Value {
        std::fs::read_to_string(self.launched_file()).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_else(|| json!({}))
    }

    fn remember(&self, name: &str, print: &std::collections::BTreeMap<String, (u64, u128)>) {
        let mut all = self.launched();
        all[name] = json!(print.iter().map(|(k, (len, at))| (k.clone(), json!([len, at.to_string()]))).collect::<serde_json::Map<_, _>>());
        std::fs::create_dir_all(self.dir.join(".runtime")).ok();
        std::fs::write(self.launched_file(), all.to_string()).ok();
    }

    /// Launches `name`: its `extension_launch` hooks, the process, its `extension_ready`
    /// hooks. A first launch is `install`, one with changed files `update`; else `reason`.
    async fn launch_one(&self, name: &str, launch: &Launch, reason: &str) -> (u64, State, String) {
        let generation = self.generation.fetch_add(1, Ordering::Relaxed) + 1;
        let log = Arc::new(Log::new(self.root.home(), name));
        let print = fingerprint(&launch.source());
        let before = self.launched()[name].as_object().cloned();
        let (reason, changed): (&str, Vec<String>) = match &before {
            None => ("install", print.keys().cloned().collect()),
            Some(b) => {
                let was = |k: &str| b.get(k).map(|v| (v[0].as_u64().unwrap_or(0), v[1].as_str().unwrap_or("").to_string()));
                let mut changed: Vec<String> = print.iter().filter(|(k, (len, at))| was(k) != Some((*len, at.to_string()))).map(|(k, _)| k.clone()).collect();
                changed.extend(b.keys().filter(|k| !print.contains_key(*k)).cloned());
                (if changed.is_empty() { reason } else { "update" }, changed)
            }
        };
        let failed = |e: String| {
            log.note(&format!("failed to start: {e}"));
            (generation, State::Failed(e), reason.to_string())
        };
        log.note(&format!("launching ({reason})"));
        self.tell(json!({"name": name, "state": "starting", "reason": reason, "error": null, "restarts": null, "final": false}));

        let mut spec = match self.spec(name, launch) {
            Ok(s) => s,
            Err(e) => return failed(e),
        };
        let (command, env) = spec.json();
        let data = json!({"name": name, "dir": spec.dir, "command": command, "env": env, "reason": reason, "changed": changed});
        let data = self.emit("extension_launch", data, &Origin::default()).await;
        if stopped(&data) {
            return failed(format!("launch blocked: {}", block_text(&data)));
        }
        let linked = match launch {
            Launch::Linked(link) => Some(link.clone()),
            _ => None,
        };
        if linked.is_none() {
            if let Err(e) = spec.update(&data) {
                return failed(format!("an extension_launch hook left {e}"));
            }
            log.note(&format!("command: {}", spec.command.join(" ")));
        }

        let core = self.core.read().unwrap().clone();
        let (me, n) = (self.me.clone(), name.to_string());
        let on_exit = Box::new(move |exit: Exit| {
            if let Some(me) = me.upgrade() {
                me.crashed(&n, generation, exit);
            }
        });
        let me = self.me.clone();
        let n = name.to_string();
        let on_line = Arc::new(move |line: String| {
            if let Some(me) = me.upgrade().filter(|me| me.listens("extension_output")) {
                me.emit_later("extension_output", json!({"name": n, "line": line}));
            }
        });
        let started = match linked {
            Some(link) => Host::link(link(), name, core, log.clone(), on_exit).await,
            None => {
                let mut command = spec.command();
                command.env("AUGUST_WORKSPACE", self.root.workspace()).env("AUGUST_HOME", self.root.home());
                Host::start(command, name, core, log.clone(), on_line, on_exit).await
            }
        };
        let host = match started {
            Ok(h) => Arc::new(h),
            Err(e) => return failed(e),
        };

        // Pending: it runs, but gets nothing until its `extension_ready` hooks let it in.
        // ponytail: calls it makes during its own setup already run with what it declared.
        let manifest = manifest_json(&host.manifest());
        let data = json!({"name": name, "reason": reason, "changed": changed, "manifest": manifest});
        let data = self.emit("extension_ready", data, &Origin::default()).await;
        if stopped(&data) {
            let why = format!("blocked: {}", block_text(&data));
            host.stop("blocked", &why);
            log.note(&why);
            return (generation, State::Failed(why), reason.to_string());
        }
        if let Some(needs) = data["needs"].as_array() {
            host.limit_needs(needs.iter().filter_map(|n| n.as_str().map(String::from)).collect());
        }
        self.remember(name, &print);
        self.watch(name.to_string(), generation, Arc::downgrade(&host));
        (generation, State::Running(host), reason.to_string())
    }

    /// Puts what a launch gave into `name`'s slot and says so.
    fn put(&self, name: &str, launch: &Launch, (generation, state, reason): (u64, State, String), restarts: u32) {
        {
            let mut slots = self.slots.write().unwrap();
            let slot = Slot { generation, state, restarts, ..Slot::new(name.into(), launch.clone(), State::Disabled, &reason) };
            match slots.iter_mut().find(|s| s.name == name) {
                Some(s) => *s = slot,
                None => {
                    slots.push(slot);
                    slots.sort_by(|a, b| a.name.cmp(&b.name));
                }
            }
        }
        self.announce(name);
    }

    /// The process of `name` ended: the `extension_exit` hooks and the restart policy decide
    /// whether and when it starts again (backoff 1, 2, 4, … s up to 60 s; `max_restarts` in
    /// a row, then it stays down until `/reload`).
    fn crashed(&self, name: &str, generation: u64, exit: Exit) {
        if exit.reason == "blocked" {
            return; // its `extension_ready` hooks refused it; nothing to restart
        }
        let policy = supervise(&self.root, name);
        let (restarts, uptime) = {
            let mut slots = self.slots.write().unwrap();
            let Some(slot) = slots.iter_mut().find(|s| s.name == name && s.generation == generation) else {
                return; // replaced by a reload
            };
            let uptime = match &slot.state {
                State::Running(h) => h.started.elapsed(),
                _ => Duration::ZERO,
            };
            if uptime >= policy.stable_after {
                slot.restarts = 0;
            }
            let what = match exit.reason.as_str() {
                "exit" => "crashed",
                "hang" => "hung",
                "unhealthy" => "unhealthy",
                _ => "failed to start",
            };
            eprintln!("extension {name}: {what}");
            slot.state = State::Failed(format!("{what}: {}", exit.error));
            slot.reason = exit.reason.clone();
            slot.health = None;
            (slot.restarts, uptime)
        };
        let (me, name) = (self.me.clone(), name.to_string());
        tokio::spawn(async move {
            let Some(me) = me.upgrade() else { return };
            let data = json!({"name": name, "reason": exit.reason, "code": exit.code, "error": exit.error, "restarts": restarts, "uptime_ms": uptime.as_millis() as u64});
            let data = me.emit("extension_exit", data, &Origin::default()).await;
            let give_up = data["restart"] == false || restarts >= policy.max_restarts;
            {
                let mut slots = me.slots.write().unwrap();
                if let Some(slot) = slots.iter_mut().find(|s| s.name == name && s.generation == generation) {
                    slot.gave_up = give_up;
                }
            }
            me.announce(&name);
            if give_up {
                eprintln!("extension {name}: not restarting until /reload");
                return;
            }
            let backoff = Duration::from_secs(1 << restarts.min(6)).min(Duration::from_secs(60));
            tokio::time::sleep(data["delay_ms"].as_u64().map(Duration::from_millis).unwrap_or(backoff)).await;
            let Some(launch) = me.slot_launch(&name, generation) else { return };
            let launched = me.launch_one(&name, &launch, "restart").await;
            let start_error = match &launched.1 {
                State::Failed(e) => Some(e.clone()),
                _ => None,
            };
            let new_generation = launched.0;
            if me.slot_launch(&name, generation).is_none() {
                return; // reloaded meanwhile
            }
            me.put(&name, &launch, launched, restarts + 1);
            eprintln!("extension {name}: restarted");
            if let Some(error) = start_error {
                me.crashed(&name, new_generation, Exit { reason: "start".into(), error, code: None });
            }
        });
    }

    fn slot_launch(&self, name: &str, generation: u64) -> Option<Launch> {
        let slots = self.slots.read().unwrap();
        slots.iter().find(|s| s.name == name && s.generation == generation).map(|s| s.launch.clone())
    }

    fn is_current(&self, name: &str, generation: u64) -> bool {
        self.slot_launch(name, generation).is_some()
    }

    /// Asks a running extension how it is doing every `ping_interval`. Any reply means it is
    /// alive (one without a check of its own answers `unknown method`); no reply
    /// `ping_misses` times in a row means it hangs, and `failed` that many times means it is
    /// broken inside: either way it is killed and goes the way of a crash.
    fn watch(&self, name: String, generation: u64, host: Weak<Host>) {
        let (me, root) = (self.me.clone(), self.root.clone());
        tokio::spawn(async move {
            let (mut misses, mut failures) = (0, 0);
            loop {
                tokio::time::sleep(supervise(&root, &name).ping_interval).await;
                let (Some(me), Some(host)) = (me.upgrade(), host.upgrade()) else { return };
                if !me.is_current(&name, generation) {
                    return;
                }
                let policy = supervise(&root, &name);
                match host.request_with("health", json!({}), policy.ping_timeout, None).await {
                    Err(e) if e.kind.as_deref() == Some("timeout") => {
                        misses += 1;
                        if misses >= policy.ping_misses {
                            host.stop("hang", &format!("no reply to {misses} health checks"));
                            return;
                        }
                    }
                    Err(e) if e.message == "the extension process exited" => return,
                    Err(_) => {
                        (misses, failures) = (0, 0);
                        me.set_health(&name, generation, None);
                    }
                    Ok(v) => {
                        misses = 0;
                        let detail = v["detail"].as_str().unwrap_or_default().to_string();
                        match v["status"].as_str().unwrap_or("ok") {
                            "failed" => {
                                failures += 1;
                                if failures >= policy.ping_misses {
                                    host.stop("unhealthy", if detail.is_empty() { "its health check failed" } else { &detail });
                                    return;
                                }
                                me.set_health(&name, generation, Some(("failed".into(), detail)));
                            }
                            "degraded" => {
                                failures = 0;
                                me.set_health(&name, generation, Some(("degraded".into(), detail)));
                            }
                            _ => {
                                failures = 0;
                                me.set_health(&name, generation, None);
                            }
                        }
                    }
                }
            }
        });
    }

    fn set_health(&self, name: &str, generation: u64, health: Option<(String, String)>) {
        let changed = {
            let mut slots = self.slots.write().unwrap();
            let Some(slot) = slots.iter_mut().find(|s| s.name == name && s.generation == generation) else { return };
            let changed = slot.health != health;
            if changed {
                slot.reason = if health.is_some() { "unhealthy".into() } else { "recovered".into() };
                slot.health = health;
            }
            changed
        };
        if changed {
            self.announce(name);
        }
    }

    /// Asks `name` how it is doing now: `{status, detail}` (`hung` when it doesn't answer).
    pub async fn check_health(&self, name: &str) -> Result<Value> {
        let host = self.running().into_iter().find(|(n, _)| n == name).map(|(_, h)| h);
        let host = host.ok_or_else(|| anyhow::anyhow!("extension {name} is not running"))?;
        Ok(match host.request_with("health", json!({}), supervise(&self.root, name).ping_timeout, None).await {
            Ok(v) => json!({"status": v["status"].as_str().unwrap_or("ok"), "detail": v["detail"]}),
            Err(e) if e.kind.as_deref() == Some("timeout") => json!({"status": "hung", "detail": e.message}),
            Err(_) => json!({"status": "ok", "detail": "it has no health check of its own"}),
        })
    }

    /// Stops every extension and starts what is on disk now (the gateway starting up).
    pub async fn start_all(&self) -> String {
        self.restart_all(None, "start").await
    }

    /// Stops every extension and starts what is on disk now. Returns the status report.
    pub async fn reload(&self) -> String {
        self.restart_all(None, "reload").await
    }

    /// Like `reload`, but extension `keep` (the one asking, mid-call) runs on as it is.
    pub async fn reload_except(&self, keep: Option<&str>) -> String {
        self.restart_all(keep, "reload").await
    }

    /// Starts in two waves: the defaults and extensions marked `early` (in their settings),
    /// then the rest, so hooks on launching (`extension_launch`, `extension_ready`) are in
    /// place before the extensions they watch start.
    async fn restart_all(&self, keep: Option<&str>, reason: &str) -> String {
        let (kept, old): (Vec<_>, Vec<_>) = self.running().into_iter().partition(|(name, _)| Some(name.as_str()) == keep);
        shut_down(old.into_iter().map(|(_, h)| h).collect()).await;
        if let Err(e) = self.prepare_defaults() {
            eprintln!("could not prepare default extensions: {e}");
        }
        let found = self.discover();
        let kept: Option<(u64, State)> = kept.into_iter().next().and_then(|(name, _)| {
            let slots = self.slots.read().unwrap();
            slots.iter().find(|s| s.name == name).map(|s| (s.generation, s.state.clone()))
        });
        let slots: Vec<Slot> = found
            .iter()
            .map(|(name, launch)| match (&kept, Some(name.as_str()) == keep, enabled(&self.root, name)) {
                (Some((generation, state)), true, _) => Slot { generation: *generation, ..Slot::new(name.clone(), launch.clone(), state.clone(), "kept") },
                (_, _, false) => Slot::new(name.clone(), launch.clone(), State::Disabled, "disable"),
                _ => Slot::new(name.clone(), launch.clone(), State::Starting, reason),
            })
            .collect();
        *self.slots.write().unwrap() = slots; // old processes are killed as they drop
        for early_wave in [true, false] {
            let wave: Vec<&(String, Launch)> = found
                .iter()
                .filter(|(name, launch)| {
                    let starting = self.slots.read().unwrap().iter().any(|s| &s.name == name && matches!(s.state, State::Starting));
                    starting && early(&self.root, name, launch) == early_wave
                })
                .collect();
            futures_util::future::join_all(wave.into_iter().map(|(name, launch)| async move {
                let launched = self.launch_one(name, launch, reason).await;
                self.put(name, launch, launched, 0);
            }))
            .await;
        }
        for name in self.slots.read().unwrap().iter().filter(|s| matches!(s.state, State::Disabled)).map(|s| s.name.clone()) {
            self.announce(&name);
        }
        self.status()
    }

    /// (Re)starts one extension after it was saved, enabling it. Returns its status line.
    pub async fn load(&self, name: &str) -> Result<String> {
        let launch = self.launch(name)?;
        self.root.set(&format!("extensions.{name}.enabled"), Value::Null)?;
        shut_down(self.running().into_iter().filter(|(n, _)| n == name).map(|(_, h)| h).collect()).await;
        {
            let mut slots = self.slots.write().unwrap();
            slots.retain(|s| s.name != name); // the process is killed as it drops
            slots.push(Slot::new(name.into(), launch.clone(), State::Starting, "enable"));
            slots.sort_by(|a, b| a.name.cmp(&b.name));
        }
        let launched = self.launch_one(name, &launch, "enable").await;
        let ok = matches!(launched.1, State::Running(_));
        self.put(name, &launch, launched, 0);
        let line = self.line(name);
        if let Some(core) = self.core.read().unwrap().clone() {
            core.changed();
        }
        if !ok {
            anyhow::bail!("{line}");
        }
        Ok(line)
    }

    fn launch(&self, name: &str) -> Result<Launch> {
        let found = self.discover().into_iter().find(|(n, _)| n == name);
        found.map(|(_, launch)| launch).ok_or_else(|| anyhow::anyhow!("no extension named `{name}`"))
    }

    /// Stops an extension and keeps it from starting until it is enabled or saved again.
    pub async fn disable(&self, name: &str) -> Result<()> {
        let launch = self.launch(name)?;
        shut_down(self.running().into_iter().filter(|(n, _)| n == name).map(|(_, h)| h).collect()).await;
        self.root.set(&format!("extensions.{name}.enabled"), json!(false))?;
        {
            let mut slots = self.slots.write().unwrap();
            slots.retain(|s| s.name != name); // the process is killed as it drops
            slots.push(Slot::new(name.into(), launch, State::Disabled, "disable"));
            slots.sort_by(|a, b| a.name.cmp(&b.name));
        }
        self.announce(name);
        Ok(())
    }

    /// Tells extensions (the `extension_state` event) what state `name` is in now.
    fn announce(&self, name: &str) {
        let data = {
            let slots = self.slots.read().unwrap();
            let Some(slot) = slots.iter().find(|s| s.name == name) else { return };
            let e = self.entry(slot, &HashMap::new());
            json!({"name": name, "state": e["state"], "reason": e["reason"], "error": e["error"], "detail": e["health"]["detail"],
                   "restarts": e["restarts"], "final": e["final"]})
        };
        Log::new(self.root.home(), name).note(&format!("{} ({})", data["state"].as_str().unwrap_or(""), data["reason"].as_str().unwrap_or("")));
        eprintln!("extension {name}: {} ({})", data["state"].as_str().unwrap_or(""), data["reason"].as_str().unwrap_or(""));
        self.tell(data);
    }

    /// Sends `extension_state` with `data` to whoever listens.
    fn tell(&self, data: Value) {
        if self.listens("extension_state") {
            self.emit_later("extension_state", data);
        }
    }

    /// Runs `event` through its handlers in the background.
    fn emit_later(&self, event: &'static str, data: Value) {
        let me = self.me.clone();
        tokio::spawn(async move {
            if let Some(me) = me.upgrade() {
                me.emit(event, data, &Origin::default()).await;
            }
        });
    }

    fn running(&self) -> Vec<(String, Arc<Host>)> {
        self.slots
            .read()
            .unwrap()
            .iter()
            .filter_map(|s| match &s.state {
                State::Running(h) => Some((s.name.clone(), h.clone())),
                State::Starting | State::Failed(_) | State::Disabled => None,
            })
            .collect()
    }

    /// Running extensions in the order their tools win over one another's of the same name:
    /// the user's own before the defaults (`tools`' `bash` gives way to a sandboxed one).
    fn tool_hosts(&self) -> Vec<(String, Arc<Host>)> {
        let defaults: HashSet<String> =
            self.slots.read().unwrap().iter().filter(|s| matches!(s.launch, Launch::Binary { .. })).map(|s| s.name.clone()).collect();
        let mut hosts = self.running();
        hosts.sort_by_key(|(n, _)| defaults.contains(n));
        hosts
    }

    /// Running extensions that may hook `event` (only those with `admin` the guarded ones).
    fn hookers(&self, event: &str) -> Vec<(String, Arc<Host>)> {
        let guarded = GUARDED.contains(&event);
        self.running()
            .into_iter()
            .filter(|(_, h)| {
                let m = h.manifest();
                m.events.iter().any(|e| e == event) && (!guarded || m.needs.iter().any(|n| n == "admin"))
            })
            .collect()
    }

    /// Model providers the running extensions offer, with the extension offering each.
    pub fn providers(&self) -> Vec<(String, ProviderInfo)> {
        self.running().iter().flat_map(|(name, h)| h.manifest().providers.iter().map(|p| (name.clone(), p.clone())).collect::<Vec<_>>()).collect()
    }

    /// Messengers the running extensions offer, with the extension offering each.
    pub fn messengers(&self) -> Vec<(String, crate::messengers::Description)> {
        self.running().iter().flat_map(|(name, h)| h.manifest().messengers.iter().map(|m| (name.clone(), m.clone())).collect::<Vec<_>>()).collect()
    }

    /// The extension offering messenger `id`, waiting a little for one still starting (it may
    /// hand in messages before August lists it as running).
    pub async fn messenger_owner(&self, id: &str) -> Option<String> {
        for _ in 0..300 {
            if let Some((ext, _)) = self.messengers().into_iter().find(|(_, d)| d.id == id) {
                return Some(ext);
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        None
    }

    /// Calls `method` (`messenger_send`, ...) on the extension that offers messenger `id`.
    pub async fn call_messenger(&self, id: &str, method: &str, mut params: Value, timeout: Duration) -> Result<Value> {
        let host = self.running().into_iter().find(|(_, h)| h.manifest().messengers.iter().any(|m| m.id == id)).map(|(_, h)| h);
        let host = host.ok_or_else(|| anyhow::anyhow!("messenger `{id}` is not running"))?;
        params["messenger"] = json!(id);
        host.request(method, params, timeout).await.map_err(|e| anyhow::anyhow!("{id}: {e}"))
    }

    /// Accounts the running extensions sign in to, with the extension of each.
    pub fn accounts(&self) -> Vec<(String, AccountInfo)> {
        self.running().iter().flat_map(|(name, h)| h.manifest().accounts.iter().map(|a| (name.clone(), a.clone())).collect::<Vec<_>>()).collect()
    }

    /// Calls `method` (`account_check`, `login`, `logout`) of extension `ext` for an account.
    pub async fn call_account(&self, ext: &str, method: &str, params: Value, chat: &Origin) -> Result<Value, String> {
        let host = self.running().into_iter().find(|(n, _)| n == ext).map(|(_, h)| h).ok_or_else(|| format!("extension {ext} is not running"))?;
        let mut params = params;
        params["ctx"] = ctx_json(chat);
        // A sign-in waits for the user (a browser, a code): give it time.
        host.request(method, params, Duration::from_secs(900)).await
    }

    /// Calls `method` on the extension that offers provider `id`, waiting for it to start.
    /// `stream` receives its `stream` notifications.
    pub async fn call_provider(
        &self,
        id: &str,
        method: &str,
        params: Value,
        stream: Option<tokio::sync::mpsc::UnboundedSender<Value>>,
    ) -> Result<Value, RpcError> {
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        let host = loop {
            if let Some((_, h)) = self.running().into_iter().find(|(_, h)| h.manifest().providers.iter().any(|p| p.id == id)) {
                break h;
            }
            if std::time::Instant::now() > deadline {
                let ids: Vec<_> = self.providers().into_iter().map(|(_, p)| p.id).collect();
                return Err(format!("no provider `{id}` (running: {})", ids.join(", ")).into());
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        };
        host.request_with(method, params, Duration::from_secs(600), stream).await
    }

    /// True when some extension handles `event` (so callers can skip building its data).
    pub fn listens(&self, event: &str) -> bool {
        !self.hookers(event).is_empty()
    }

    /// Runs `event` through every extension that handles it. Mutating events form a chain in
    /// the user's order (`hooks.order` in `august.json`, then the rest by name): the fields a
    /// handler returns are merged into the data the next one sees, and a `block` or `handled`
    /// result stops the chain. Observe-only events reach every handler at once and come back
    /// unchanged. Failing handlers are skipped.
    pub async fn emit(&self, event: &str, mut data: Value, chat: &Origin) -> Value {
        let mut hosts = self.hookers(event);
        let observe = self.observes(event);
        if observe {
            futures_util::future::join_all(hosts.iter().map(|(n, h)| hook(n, h, event, &data, chat, observe))).await;
            return data;
        }
        in_user_order(&self.root, &mut hosts);
        for (name, host) in hosts {
            if let (Some(Value::Object(changes)), Some(d)) = (hook(&name, &host, event, &data, chat, false).await, data.as_object_mut()) {
                d.extend(changes);
            }
            if stopped(&data) {
                break;
            }
        }
        data
    }

    /// The extension that does the core's `job` in its place (`takes`); of several, the first
    /// in the user's order.
    pub fn taker(&self, job: &str) -> Option<String> {
        let mut hosts: Vec<_> = self.running().into_iter().filter(|(_, h)| h.manifest().takes.iter().any(|t| t == job)).collect();
        in_user_order(&self.root, &mut hosts);
        hosts.into_iter().next().map(|(n, _)| n)
    }

    /// Hands extension `name` the `event` of a job it took and waits for its handler; `None`
    /// when it isn't running or failed.
    pub async fn run_job(&self, name: &str, event: &str, data: &Value, chat: &Origin) -> Option<Value> {
        let (_, host) = self.running().into_iter().find(|(n, _)| n == name)?;
        hook(name, &host, event, data, chat, false).await
    }

    /// Whether `event`'s handlers only observe: one of the core's observers, or an extension
    /// event declared `observe`.
    fn observes(&self, event: &str) -> bool {
        OBSERVERS.contains(&event) || self.declared(event).is_some_and(|d| d.observe)
    }

    /// The extension whose events go by `namespace`: one that replaces it, else its own.
    fn owner(&self, namespace: &str) -> Option<(String, Arc<Host>)> {
        let running = self.running();
        let replacer = running.iter().find(|(_, h)| h.manifest().replaces.iter().any(|r| r == namespace));
        replacer.or_else(|| running.iter().find(|(n, _)| n == namespace)).cloned()
    }

    /// The declaration of extension event `namespace:name`.
    fn declared(&self, event: &str) -> Option<host::EventDef> {
        let (namespace, name) = event.split_once(':')?;
        let (_, host) = self.owner(namespace)?;
        host.manifest().emits.iter().find(|d| d.name == name).cloned()
    }

    /// Extension `caller` emits `event` (`name`, or `namespace:name` for a namespace it
    /// replaces): its data is checked against the declared schema and run through the
    /// handlers of `<namespace>:<name>`. A chain returns the data the handlers leave; an
    /// observed event returns at once.
    pub async fn emit_own(&self, caller: &str, event: &str, data: Value, mut origin: Origin) -> Result<Value> {
        let (namespace, name) = event.split_once(':').unwrap_or((caller, event));
        match self.owner(namespace) {
            Some((owner, _)) if owner == caller => {}
            Some((owner, _)) => anyhow::bail!("events `{namespace}:*` belong to {owner}"),
            None => anyhow::bail!("events `{namespace}:*` belong to {namespace}, which isn't running"),
        }
        let full = format!("{namespace}:{name}");
        let def = self.declared(&full).ok_or_else(|| anyhow::anyhow!("declare `{name}` first (august.defineEvent)"))?;
        check(&def.schema, &data).map_err(|e| anyhow::anyhow!("`{full}`: {e}"))?;
        anyhow::ensure!(origin.depth < MAX_DEPTH, "`{full}`: events nest deeper than {MAX_DEPTH}; do handlers emit each other in a loop?");
        origin.depth += 1;
        if !def.observe {
            return Ok(self.emit(&full, data, &origin).await);
        }
        if let Some(me) = self.me.upgrade() {
            let echo = data.clone();
            tokio::spawn(async move { me.emit(&full, echo, &origin).await });
        }
        Ok(data)
    }

    /// Tools of all running extensions; a name an earlier extension took is skipped.
    pub fn tool_specs(&self) -> Vec<ToolSpec> {
        let mut seen = HashSet::new();
        self.tool_hosts()
            .iter()
            .flat_map(|(_, h)| h.manifest().tools.clone())
            .filter(|t| seen.insert(t.name.clone()))
            .collect()
    }

    /// The schema of a running extension's settings (null if none or not running).
    pub fn settings_schema(&self, name: &str) -> Value {
        self.running().into_iter().find(|(n, _)| n == name).map(|(_, h)| h.manifest().settings.clone()).unwrap_or(Value::Null)
    }

    /// The extension behind each extension tool (the first one, as in `tool_specs`).
    pub fn tool_owners(&self) -> std::collections::HashMap<String, String> {
        let mut owners = std::collections::HashMap::new();
        for (name, h) in self.tool_hosts() {
            for t in &h.manifest().tools {
                owners.entry(t.name.clone()).or_insert_with(|| name.clone());
            }
        }
        owners
    }

    pub fn has_tool(&self, name: &str) -> bool {
        self.running().iter().any(|(_, h)| h.manifest().tools.iter().any(|t| t.name == name))
    }

    /// Runs an extension tool; `None` if no extension has it.
    pub async fn call_tool(&self, name: &str, input: &Value, chat: &Origin) -> Option<Result<String, String>> {
        let host = self.tool_hosts().into_iter().find(|(_, h)| h.manifest().tools.iter().any(|t| t.name == name))?.1;
        let params = json!({"name": name, "input": input, "ctx": ctx_json(chat)});
        Some(host.request("tool", params, TOOL_TIMEOUT).await.map(|v| match v {
            Value::String(s) => s,
            other => other.to_string(),
        }))
    }

    /// Every running extension's prompt sections, as one block for the system prompt.
    pub fn prompt_sections(&self) -> String {
        self.running().iter().flat_map(|(_, h)| h.manifest().sections.clone()).map(|(_, text)| format!("\n\n{}", text.trim())).collect()
    }

    /// `(extension, name, description)` of extension commands; of two with one name, the first.
    pub fn commands(&self) -> Vec<(String, String, String)> {
        let mut seen = HashSet::new();
        self.running()
            .iter()
            .flat_map(|(owner, h)| h.manifest().commands.iter().map(|(n, d)| (owner.clone(), n.clone(), d.clone())).collect::<Vec<_>>())
            .filter(|(_, n, _)| seen.insert(n.clone()))
            .collect()
    }

    /// Runs `/name args`; `None` if no extension has the command, else the optional reply.
    pub async fn run_command(&self, name: &str, args: &str, chat: &Origin) -> Option<Result<Option<String>, String>> {
        let host = self.running().into_iter().find(|(_, h)| h.manifest().commands.iter().any(|(n, _)| n == name))?.1;
        let params = json!({"name": name, "args": args, "ctx": ctx_json(chat)});
        Some(host.request("command", params, COMMAND_TIMEOUT).await.map(|v| v.as_str().map(String::from)))
    }

    /// One extension as `list` shows it; `usage` is `(memory KB, CPU %)` by process group.
    fn entry(&self, slot: &Slot, usage: &HashMap<i32, (u64, f64)>) -> Value {
        let degraded = slot.health.as_ref().is_some_and(|(s, _)| s == "degraded");
        let (state, error) = match &slot.state {
            State::Starting => ("starting", None),
            State::Running(_) if degraded => ("degraded", None),
            State::Running(_) => ("running", None),
            State::Failed(e) => ("failed", Some(e.trim().to_string())),
            State::Disabled => ("disabled", None),
        };
        let origin = self.root.unit("extensions", &slot.name).ok().and_then(|v| v["origin"].as_str().map(String::from));
        let origin = origin.unwrap_or_else(|| match slot.launch {
            Launch::Binary { .. } => "default".into(),
            Launch::Script(_) | Launch::Command(_) | Launch::Linked(_) => "user".into(),
        });
        let mut v = json!({"name": slot.name, "state": state, "error": error, "origin": origin, "reason": slot.reason,
                           "restarts": slot.restarts, "final": slot.gave_up});
        if let Some((status, detail)) = &slot.health {
            v["health"] = json!({"status": status, "detail": detail});
        }
        if let State::Running(h) = &slot.state {
            v["pid"] = json!(h.pid());
            v["uptime_s"] = json!(h.started.elapsed().as_secs());
            if let Some((kb, cpu)) = usage.get(&h.pid()) {
                v["memory_kb"] = json!(kb);
                v["cpu"] = json!((cpu * 10.0).round() / 10.0);
            }
            let m = h.manifest();
            v["summary"] = json!(m.summary);
            v["details"] = json!(m.details);
            let tools: Vec<&str> = m.tools.iter().map(|t| t.name.as_str()).collect();
            v["tools"] = json!(tools);
            v["commands"] = json!(m.commands.iter().map(|c| &c.0).collect::<Vec<_>>());
            v["accounts"] = json!(m.accounts.iter().map(|a| &a.id).collect::<Vec<_>>());
            v["hooks"] = json!(m.events);
            v["needs"] = json!(m.needs);
            v["takes"] = json!(m.takes);
            v["sections"] = json!(m.sections.iter().map(|s| &s.0).collect::<Vec<_>>());
            let namespace = m.replaces.first().unwrap_or(&slot.name);
            v["events"] = Value::Array(
                m.emits
                    .iter()
                    .map(|d| json!({"name": format!("{namespace}:{}", d.name), "description": d.description, "schema": d.schema, "observe": d.observe}))
                    .collect(),
            );
        }
        v
    }

    fn line(&self, name: &str) -> String {
        let slots = self.slots.read().unwrap();
        slots.iter().find(|s| s.name == name).map(|s| status_line(&self.entry(s, &HashMap::new()))).unwrap_or_default()
    }

    /// Every extension in name order: `{name, state: starting|running|degraded|failed|disabled,
    /// reason, error, restarts, final, health, pid, uptime_s, memory_kb, cpu, tools, replaces,
    /// commands, hooks, needs, takes, sections, events}`.
    pub fn list(&self) -> Value {
        let usage = usage();
        Value::Array(self.slots.read().unwrap().iter().map(|s| self.entry(s, &usage)).collect())
    }

    /// One line per extension, as `/extensions` shows them.
    pub fn status(&self) -> String {
        status(&self.list(), &self.dir)
    }

    /// The extension-writing guide (op `guide`).
    pub fn guide(&self) -> String {
        guide(&self.dir)
    }

    pub fn root(&self) -> &Root {
        &self.root
    }

    /// The last `lines` lines of extension `name`'s log.
    pub fn log_tail(&self, name: &str, lines: usize) -> String {
        logs::tail(self.root.home(), name, lines)
    }
}

/// The status report (logged at start) for a `list()`.
fn status(list: &Value, dir: &Path) -> String {
    let all = list.as_array().cloned().unwrap_or_default();
    if all.is_empty() {
        return format!("No extensions. They live in `{}`.", dir.display());
    }
    all.iter().map(status_line).collect::<Vec<_>>().join("\n")
}

fn status_line(e: &Value) -> String {
    let name = e["name"].as_str().unwrap_or("");
    match e["state"].as_str() {
        Some("failed") => format!("❌ {name} — {}", e["error"].as_str().unwrap_or("")),
        Some("disabled") => format!("⏸ {name} — disabled"),
        Some("starting") => format!("⏳ {name} — starting"),
        _ => {
            let list = |k: &str, prefix: &str| -> Vec<String> {
                e[k].as_array().into_iter().flatten().filter_map(|x| x.as_str().or(x["name"].as_str())).map(|s| format!("{prefix}{s}")).collect()
            };
            let mut parts = Vec::new();
            for (key, label, prefix) in [
                ("tools", "tools", ""),
                ("commands", "commands", "/"),
                ("hooks", "hooks", ""),
                ("events", "emits", ""),
                ("needs", "needs", ""),
                ("takes", "takes", ""),
                ("sections", "prompt", ""),
            ] {
                let items = list(key, prefix);
                if !items.is_empty() {
                    parts.push(format!("{label}: {}", items.join(", ")));
                }
            }
            if parts.is_empty() {
                parts.push("registers nothing".into());
            }
            match e["health"]["detail"].as_str() {
                Some(detail) if e["state"] == "degraded" => format!("⚠️ {name} — degraded: {detail}; {}", parts.join("; ")),
                _ => format!("✅ {name} — {}", parts.join("; ")),
            }
        }
    }
}

/// Sorts extensions in the user's order (`hooks.order` in `august.json`), the rest after.
// ponytail: reads august.json on every call; cache it if that ever shows up.
fn in_user_order(root: &Root, hosts: &mut [(String, Arc<Host>)]) {
    let order = root.get("august.hooks.order").unwrap_or_default();
    let order: Vec<&str> = order.as_array().into_iter().flatten().filter_map(Value::as_str).collect();
    hosts.sort_by_key(|(n, _)| order.iter().position(|o| o == n).unwrap_or(usize::MAX));
}

/// Tells extensions they are about to stop (the `shutdown` event), so they can clean up;
/// each gets a couple of seconds.
async fn shut_down(hosts: Vec<Arc<Host>>) {
    let asked = hosts.iter().filter(|h| h.manifest().events.iter().any(|e| e == "shutdown")).map(|h| {
        let params = json!({"name": "shutdown", "data": {}, "ctx": ctx_json(&Origin::default())});
        h.request("event", params, SHUTDOWN_TIMEOUT)
    });
    futures_util::future::join_all(asked).await;
}

/// One extension's handler of `event`; `None` when it failed.
async fn hook(name: &str, host: &Host, event: &str, data: &Value, chat: &Origin, observe: bool) -> Option<Value> {
    let params = json!({"name": event, "data": data, "ctx": ctx_json(chat)});
    let default = match event {
        "message_in" => MESSAGE_TIMEOUT,
        // A launch hook may build the extension; ready and exit hooks may ask the user.
        "extension_launch" => Duration::from_secs(900),
        "extension_ready" | "extension_exit" => Duration::from_secs(300),
        // Nobody waits for these: let them finish what they started (a fork, a judge).
        _ if observe && event != "stop" => OBSERVER_TIMEOUT,
        _ => EVENT_TIMEOUT,
    };
    let timeout = host.manifest().timeouts.get(event).copied().map(Duration::from_millis).unwrap_or(default);
    host.request("event", params, timeout).await.inspect_err(|e| eprintln!("extension {name}: `{event}` hook failed: {e}")).ok()
}

fn ctx_json(origin: &Origin) -> Value {
    json!({
        "thread": origin.thread.as_ref().map(|t| json!({"messenger": t.messenger, "id": t.id})),
        "turn": origin.turn,
        "depth": origin.depth,
    })
}

/// Checks event data (or a messenger action's arguments) against its declared schema.
// ponytail: only the top level (required keys, property types); a full JSON Schema validator
// when contracts need nested checks.
pub(crate) fn check(schema: &Value, data: &Value) -> Result<()> {
    let Some(fields) = data.as_object() else { anyhow::bail!("must be an object") };
    for key in schema["required"].as_array().into_iter().flatten().filter_map(Value::as_str) {
        anyhow::ensure!(fields.contains_key(key), "missing `{key}`");
    }
    for (key, value) in fields {
        let Some(kind) = schema["properties"][key]["type"].as_str() else { continue };
        let ok = match kind {
            "string" => value.is_string(),
            "number" => value.is_number(),
            "integer" => value.is_i64() || value.is_u64(),
            "boolean" => value.is_boolean(),
            "object" => value.is_object(),
            "array" => value.is_array(),
            "null" => value.is_null(),
            _ => true,
        };
        anyhow::ensure!(ok, "`{key}` must be {kind}");
    }
    Ok(())
}
