//! Extensions add tools, slash commands and hooks. User extensions are TypeScript files in
//! `~/.august/extensions/<name>/index.ts`, run by bun through `host.ts`. Default extensions
//! (the repo's `extensions/<name>/main.rs`) are Rust binaries `august-ext-<name>` next to
//! `august`, speaking the same protocol; a user extension of the same name replaces one.
//! Each extension is its own process (see `host.rs`), so a broken or hanging one can't take
//! the gateway down.

mod host;

use crate::llm::ToolSpec;
use crate::messengers::Thread;
use anyhow::Result;
use async_trait::async_trait;
use host::Host;
use serde_json::{Value, json};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock, Weak};
use std::time::Duration;

const HOST_TS: &str = include_str!("host.ts");
const TYPES: &str = include_str!("august.d.ts");
const GUIDE: &str = include_str!("guide.md");
/// The extensions that ship with August, as binaries `august-ext-<name>` next to `august`.
const DEFAULTS: &[&str] = &["approvals", "browser", "clarify", "commands", "extend", "goal", "mcp", "memory", "messaging", "review", "scheduler", "subagents", "voice", "web"];

const EVENT_TIMEOUT: Duration = Duration::from_secs(10);
/// `message_in` may do real work on attachments (e.g. transcribe a voice note).
const MESSAGE_TIMEOUT: Duration = Duration::from_secs(120);
/// Long enough for `/compact` to summarise a big conversation.
const COMMAND_TIMEOUT: Duration = Duration::from_secs(600);
/// Observe-only hooks run in the background, so they may take long.
const OBSERVER_TIMEOUT: Duration = Duration::from_secs(3600);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);
const TOOL_TIMEOUT: Duration = Duration::from_secs(600);
/// Restarts after a crash before an extension stays down until `/reload`.
const MAX_RESTARTS: u32 = 3;
/// Events whose handlers only observe: what they return is ignored.
const OBSERVERS: &[&str] = &["turn_end", "turn_settled", "turn_event", "llm_result", "session_changed", "compaction", "reaction", "extension_state", "config_changed", "stop"];
const ENTRIES: [&str; 3] = ["index.ts", "index.js", "index.mjs"];

/// What extensions can ask of August: the operations of the core's table.
#[async_trait]
pub trait Core: Send + Sync {
    /// Runs operation `op` for extension `ext`, whose permission was already checked.
    async fn call(&self, ext: &str, op: &str, params: &Value) -> Result<Value>;
    /// What extensions offer changed (one was loaded, or registered something at runtime).
    fn changed(&self);
}

/// Options of `ctx.agent`.
#[derive(Default, serde::Deserialize)]
pub struct AgentOpts {
    /// Instructions added to the base system prompt.
    pub system: Option<String>,
    /// Only these tools.
    pub tools: Option<Vec<String>>,
    /// Never these tools.
    #[serde(default)]
    pub exclude: Vec<String>,
}

/// Where a hook, tool or command runs: its thread, and the turn when inside one.
#[derive(Debug, Clone, Default)]
pub struct Origin {
    pub thread: Option<Thread>,
    pub turn: Option<crate::agent::TurnTag>,
}

impl Origin {
    pub fn thread(thread: Thread) -> Self {
        Self { thread: Some(thread), turn: None }
    }
}

#[derive(Clone)]
enum State {
    Running(Arc<Host>),
    Failed(String),
    Disabled,
}

/// How an extension is started.
#[derive(Clone)]
enum Launch {
    /// A TypeScript or JavaScript entry file, run by bun through `host.ts`.
    Script(PathBuf),
    /// A default extension's binary; `dir` is its folder (state files).
    Binary { exe: PathBuf, dir: PathBuf },
}

impl Launch {
    fn dir(&self) -> PathBuf {
        match self {
            Launch::Script(entry) => entry.parent().unwrap_or(Path::new(".")).to_path_buf(),
            Launch::Binary { dir, .. } => dir.clone(),
        }
    }
}

struct Slot {
    name: String,
    launch: Launch,
    state: State,
    /// Identifies the process; exits of replaced processes are ignored.
    generation: u64,
    restarts: u32,
}

pub struct Extensions {
    dir: PathBuf,
    core: RwLock<Option<Arc<dyn Core>>>,
    slots: RwLock<Vec<Slot>>,
    generation: AtomicU64,
    me: Weak<Extensions>,
}

pub fn dir() -> PathBuf {
    crate::config::home().join("extensions")
}

/// The extension-writing guide with the API types, served as a built-in skill.
pub fn guide() -> String {
    format!("{GUIDE}\nExtensions live in {}.\n\n```ts\n{TYPES}```\n", dir().display())
}

pub fn valid_name(name: &str) -> bool {
    crate::skills::valid_name(name)
}

/// `$AUGUST_BUN`, `bun` on PATH, or the usual install locations.
fn find_bun() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("AUGUST_BUN") {
        return Some(PathBuf::from(p));
    }
    let home = PathBuf::from(std::env::var("HOME").unwrap_or_default());
    std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).map(|d| d.join("bun")).collect::<Vec<_>>())
        .unwrap_or_default()
        .into_iter()
        .chain([home.join(".bun/bin/bun"), "/opt/homebrew/bin/bun".into(), "/usr/local/bin/bun".into()])
        .find(|p| p.is_file())
}

fn entry_of(folder: &Path) -> Option<PathBuf> {
    ENTRIES.iter().map(|e| folder.join(e)).find(|p| p.is_file())
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
fn enabled(name: &str) -> bool {
    crate::config::unit("extensions", name).map_or(true, |v| v["enabled"] != false)
}

/// `august-ext-<name>` next to the running binary.
fn default_binary(name: &str) -> PathBuf {
    let exe = std::env::current_exe().unwrap_or_default();
    exe.parent().unwrap_or(Path::new(".")).join(format!("august-ext-{name}"))
}

fn stopped(data: &Value) -> bool {
    let block = &data["block"];
    block == true || block.as_str().is_some_and(|s| !s.is_empty()) || data["handled"] == true
}

/// Built-in tool names (an extension tool of the same name replaces the built-in).
fn builtin_tools() -> HashSet<String> {
    crate::tools::ToolRegistry::builtin_names().into_iter().map(String::from).collect()
}

impl Extensions {
    pub fn new(dir: PathBuf) -> Arc<Self> {
        Arc::new_cyclic(|me| Self {
            dir,
            core: RwLock::new(None),
            slots: RwLock::new(Vec::new()),
            generation: AtomicU64::new(0),
            me: me.clone(),
        })
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
            if valid_name(&name) && let Some(entry) = entry_of(&e.path()) {
                found.push((name, Launch::Script(entry)));
            }
        }
        for name in DEFAULTS {
            if !found.iter().any(|(n, _)| n == name) {
                let launch = Launch::Binary { exe: default_binary(name), dir: self.defaults_dir().join(name) };
                found.push((name.to_string(), launch));
            }
        }
        found.sort_by(|a, b| a.0.cmp(&b.0));
        found
    }

    /// Folders of the defaults; drops the TypeScript copies earlier versions wrote there.
    fn prepare_defaults(&self) -> std::io::Result<()> {
        for name in DEFAULTS {
            let folder = self.defaults_dir().join(name);
            std::fs::create_dir_all(&folder)?;
            std::fs::remove_file(folder.join("index.ts")).ok();
        }
        Ok(())
    }

    /// Writes `host.ts` next to the extensions and returns `(bun, host.ts)`.
    fn runtime(&self) -> Result<(PathBuf, PathBuf), String> {
        let bun = find_bun().ok_or("bun is not installed (https://bun.sh); set AUGUST_BUN to its path")?;
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

    async fn spawn(&self, name: &str, launch: &Launch) -> (u64, State) {
        let generation = self.generation.fetch_add(1, Ordering::Relaxed) + 1;
        let command = match launch {
            Launch::Script(entry) => match self.runtime() {
                Ok((bun, host_ts)) => {
                    let mut c = tokio::process::Command::new(bun);
                    c.arg("run").arg(host_ts).arg(entry).arg(name).current_dir(launch.dir());
                    c
                }
                Err(e) => return (generation, State::Failed(e)),
            },
            Launch::Binary { exe, dir } => {
                if !exe.is_file() {
                    return (generation, State::Failed(format!("{} is missing; build it with `cargo build`", exe.display())));
                }
                let mut c = tokio::process::Command::new(exe);
                c.current_dir(dir).env("AUGUST_EXTENSION_DIR", dir).env("AUGUST_EXTENSIONS", &self.dir);
                c
            }
        };
        let core = self.core.read().unwrap().clone();
        let (me, n) = (self.me.clone(), name.to_string());
        let on_exit = Box::new(move |tail: String| {
            if let Some(me) = me.upgrade() {
                me.crashed(&n, generation, tail);
            }
        });
        let state = match Host::start(command, name, core, on_exit).await {
            Ok(h) => State::Running(Arc::new(h)),
            Err(e) => State::Failed(e),
        };
        (generation, state)
    }

    /// Restarts a crashed extension with a growing delay, up to `MAX_RESTARTS` times.
    fn crashed(&self, name: &str, generation: u64, tail: String) {
        let restarts = {
            let mut slots = self.slots.write().unwrap();
            let Some(slot) = slots.iter_mut().find(|s| s.name == name && s.generation == generation) else {
                return; // replaced by a reload
            };
            eprintln!("extension {name}: crashed");
            slot.state = State::Failed(format!("crashed: {tail}"));
            slot.restarts
        };
        self.announce(name);
        if restarts >= MAX_RESTARTS {
            eprintln!("extension {name}: crashed {restarts} times, not restarting until /reload");
            return;
        }
        let (me, name) = (self.me.clone(), name.to_string());
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(1 << restarts)).await;
            let Some(me) = me.upgrade() else { return };
            let Some(launch) = me.slot_launch(&name, generation) else { return };
            let (new_gen, state) = me.spawn(&name, &launch).await;
            let replaced = {
                let mut slots = me.slots.write().unwrap();
                let slot = slots.iter_mut().find(|s| s.name == name && s.generation == generation);
                slot.map(|slot| *slot = Slot { name: name.clone(), launch, state, generation: new_gen, restarts: restarts + 1 }).is_some()
            };
            if replaced {
                eprintln!("extension {name}: restarted");
                me.announce(&name);
            }
        });
    }

    fn slot_launch(&self, name: &str, generation: u64) -> Option<Launch> {
        let slots = self.slots.read().unwrap();
        slots.iter().find(|s| s.name == name && s.generation == generation).map(|s| s.launch.clone())
    }

    /// Stops every extension and starts what is on disk now. Returns the status report.
    pub async fn reload(&self) -> String {
        self.reload_except(None).await
    }

    /// Like `reload`, but extension `keep` (the one asking, mid-call) runs on as it is.
    pub async fn reload_except(&self, keep: Option<&str>) -> String {
        let (kept, old): (Vec<_>, Vec<_>) = self.running().into_iter().partition(|(name, _)| Some(name.as_str()) == keep);
        shut_down(old.into_iter().map(|(_, h)| h).collect()).await;
        let kept: Option<(u64, State)> = kept.into_iter().next().and_then(|(name, _)| {
            let slots = self.slots.read().unwrap();
            slots.iter().find(|s| s.name == name).map(|s| (s.generation, s.state.clone()))
        });
        if let Err(e) = self.prepare_defaults() {
            eprintln!("could not prepare default extensions: {e}");
        }
        let found = self.discover();
        let started = futures_util::future::join_all(found.iter().map(|(name, launch)| {
            let kept = kept.clone().filter(|_| Some(name.as_str()) == keep);
            async move {
                match (kept, enabled(name)) {
                    (Some(kept), _) => kept,
                    (None, false) => (0, State::Disabled),
                    (None, true) => self.spawn(name, launch).await,
                }
            }
        }))
        .await;
        let slots = found
            .into_iter()
            .zip(started)
            .map(|((name, launch), (generation, state))| Slot { name, launch, state, generation, restarts: 0 })
            .collect();
        *self.slots.write().unwrap() = slots; // old processes are killed as they drop
        for name in self.slots.read().unwrap().iter().map(|s| s.name.clone()) {
            self.announce(&name);
        }
        self.status()
    }

    /// (Re)starts one extension after it was saved, enabling it. Returns its status line.
    pub async fn load(&self, name: &str) -> Result<String> {
        let launch = self.launch(name)?;
        crate::config::set(&format!("extensions.{name}.enabled"), Value::Null)?;
        shut_down(self.running().into_iter().filter(|(n, _)| n == name).map(|(_, h)| h).collect()).await;
        let (generation, state) = self.spawn(name, &launch).await;
        let ok = matches!(state, State::Running(_));
        let slot = Slot { name: name.to_string(), launch, state, generation, restarts: 0 };
        {
            let mut slots = self.slots.write().unwrap();
            slots.retain(|s| s.name != name);
            slots.push(slot);
            slots.sort_by(|a, b| a.name.cmp(&b.name));
        }
        let line = self.line(name);
        self.announce(name);
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
        crate::config::set(&format!("extensions.{name}.enabled"), json!(false))?;
        {
            let mut slots = self.slots.write().unwrap();
            slots.retain(|s| s.name != name); // the process is killed as it drops
            slots.push(Slot { name: name.to_string(), launch, state: State::Disabled, generation: 0, restarts: 0 });
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
            let e = self.entry(slot, &HashSet::new());
            json!({"name": name, "state": e["state"], "error": e["error"]})
        };
        if !self.listens("extension_state") {
            return;
        }
        let me = self.me.clone();
        tokio::spawn(async move {
            if let Some(me) = me.upgrade() {
                me.emit("extension_state", data, &Origin::default()).await;
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
                State::Failed(_) | State::Disabled => None,
            })
            .collect()
    }

    /// True when some extension handles `event` (so callers can skip building its data).
    pub fn listens(&self, event: &str) -> bool {
        self.running().iter().any(|(_, h)| h.manifest().events.iter().any(|e| e == event))
    }

    /// Runs `event` through every extension that handles it. Mutating events form a chain in
    /// the user's order (`hooks.order` in `august.json`, then the rest by name): the fields a
    /// handler returns are merged into the data the next one sees, and a `block` or `handled`
    /// result stops the chain. Observe-only events reach every handler at once and come back
    /// unchanged. Failing handlers are skipped.
    pub async fn emit(&self, event: &str, mut data: Value, chat: &Origin) -> Value {
        let mut hosts: Vec<_> = self.running().into_iter().filter(|(_, h)| h.manifest().events.iter().any(|e| e == event)).collect();
        if OBSERVERS.contains(&event) {
            futures_util::future::join_all(hosts.iter().map(|(n, h)| hook(n, h, event, &data, chat))).await;
            return data;
        }
        // ponytail: reads august.json on every chained event; cache it if that ever shows up.
        let order = crate::config::get("august.hooks.order").unwrap_or_default();
        let order: Vec<&str> = order.as_array().into_iter().flatten().filter_map(Value::as_str).collect();
        hosts.sort_by_key(|(n, _)| order.iter().position(|o| o == n).unwrap_or(usize::MAX));
        for (name, host) in hosts {
            if let (Some(Value::Object(changes)), Some(d)) = (hook(&name, &host, event, &data, chat).await, data.as_object_mut()) {
                d.extend(changes);
            }
            if stopped(&data) {
                break;
            }
        }
        data
    }

    /// Tools of all running extensions; a name an earlier extension took is skipped.
    pub fn tool_specs(&self) -> Vec<ToolSpec> {
        let mut seen = HashSet::new();
        self.running()
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
        for (name, h) in self.running() {
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
        let host = self.running().into_iter().find(|(_, h)| h.manifest().tools.iter().any(|t| t.name == name))?.1;
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

    fn entry(&self, slot: &Slot, builtin: &HashSet<String>) -> Value {
        let (state, error) = match &slot.state {
            State::Running(_) => ("running", None),
            State::Failed(e) => ("failed", Some(e.trim().to_string())),
            State::Disabled => ("disabled", None),
        };
        let origin = crate::config::unit("extensions", &slot.name).ok().and_then(|v| v["origin"].as_str().map(String::from));
        let origin = origin.unwrap_or_else(|| match slot.launch {
            Launch::Binary { .. } => "default".into(),
            Launch::Script(_) => "user".into(),
        });
        let mut v = json!({"name": slot.name, "state": state, "error": error, "origin": origin});
        if let State::Running(h) = &slot.state {
            let m = h.manifest();
            let tools: Vec<&str> = m.tools.iter().map(|t| t.name.as_str()).collect();
            v["tools"] = json!(tools);
            v["replaces"] = json!(tools.iter().filter(|t| builtin.contains(**t)).collect::<Vec<_>>());
            v["commands"] = json!(m.commands.iter().map(|c| &c.0).collect::<Vec<_>>());
            v["hooks"] = json!(m.events);
            v["needs"] = json!(m.needs);
            v["sections"] = json!(m.sections.iter().map(|s| &s.0).collect::<Vec<_>>());
        }
        v
    }

    fn line(&self, name: &str) -> String {
        let slots = self.slots.read().unwrap();
        slots.iter().find(|s| s.name == name).map(|s| status_line(&self.entry(s, &builtin_tools()))).unwrap_or_default()
    }

    /// Every extension in name order: `{name, state: running|failed|disabled, error, tools,
    /// replaces, commands, hooks, needs, sections}`.
    pub fn list(&self) -> Value {
        let builtin = builtin_tools();
        Value::Array(self.slots.read().unwrap().iter().map(|s| self.entry(s, &builtin)).collect())
    }

    /// One line per extension, as `/extensions` shows them.
    pub fn status(&self) -> String {
        status(&self.list())
    }
}

/// The status report (logged at start) for a `list()`.
fn status(list: &Value) -> String {
    let all = list.as_array().cloned().unwrap_or_default();
    if all.is_empty() {
        return format!("No extensions. They live in `{}`.", dir().display());
    }
    all.iter().map(status_line).collect::<Vec<_>>().join("\n")
}

fn status_line(e: &Value) -> String {
    let name = e["name"].as_str().unwrap_or("");
    match e["state"].as_str() {
        Some("failed") => format!("❌ {name} — {}", e["error"].as_str().unwrap_or("")),
        Some("disabled") => format!("⏸ {name} — disabled"),
        _ => {
            let list = |k: &str, prefix: &str| -> Vec<String> {
                e[k].as_array().into_iter().flatten().filter_map(Value::as_str).map(|s| format!("{prefix}{s}")).collect()
            };
            let mut parts = Vec::new();
            for (key, label, prefix) in [
                ("tools", "tools", ""),
                ("commands", "commands", "/"),
                ("hooks", "hooks", ""),
                ("needs", "needs", ""),
                ("sections", "prompt", ""),
                ("replaces", "replaces built-in", ""),
            ] {
                let items = list(key, prefix);
                if !items.is_empty() {
                    parts.push(format!("{label}: {}", items.join(", ")));
                }
            }
            if parts.is_empty() {
                parts.push("registers nothing".into());
            }
            format!("✅ {name} — {}", parts.join("; "))
        }
    }
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
async fn hook(name: &str, host: &Host, event: &str, data: &Value, chat: &Origin) -> Option<Value> {
    let params = json!({"name": event, "data": data, "ctx": ctx_json(chat)});
    let default = match event {
        "message_in" => MESSAGE_TIMEOUT,
        // Nobody waits for these: let them finish what they started (a fork, a judge).
        _ if OBSERVERS.contains(&event) && event != "stop" => OBSERVER_TIMEOUT,
        _ => EVENT_TIMEOUT,
    };
    let timeout = host.manifest().timeouts.get(event).copied().map(Duration::from_millis).unwrap_or(default);
    host.request("event", params, timeout).await.inspect_err(|e| eprintln!("extension {name}: `{event}` hook failed: {e}")).ok()
}

fn ctx_json(origin: &Origin) -> Value {
    json!({
        "thread": origin.thread.as_ref().map(|t| json!({"messenger": t.messenger, "id": t.id})),
        "turn": origin.turn,
    })
}
