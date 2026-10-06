//! Extensions: TypeScript files in `~/.august/extensions/<name>/index.ts` that add tools,
//! slash commands and hooks. Default extensions (the repo's `extensions/`) are built in and
//! written to `.runtime/defaults/` on load; a user extension of the same name replaces one. Each runs in its own bun process (see `host.rs`), so a
//! broken or hanging extension can't take the gateway down.

mod host;

use crate::llm::ToolSpec;
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
/// `(name, index.ts)` of the extensions that ship with August.
const DEFAULTS: &[(&str, &str)] = &[
    ("browser", include_str!("../../extensions/browser/index.ts")),
    ("web", include_str!("../../extensions/web/index.ts")),
    ("goal", include_str!("../../extensions/goal/index.ts")),
    ("subagents", include_str!("../../extensions/subagents/index.ts")),
    ("clarify", include_str!("../../extensions/clarify/index.ts")),
    ("mcp", include_str!("../../extensions/mcp/index.ts")),
    ("voice", include_str!("../../extensions/voice/index.ts")),
];

const EVENT_TIMEOUT: Duration = Duration::from_secs(10);
/// `message_in` may do real work on attachments (e.g. transcribe a voice note).
const MESSAGE_TIMEOUT: Duration = Duration::from_secs(120);
const COMMAND_TIMEOUT: Duration = Duration::from_secs(60);
const TOOL_TIMEOUT: Duration = Duration::from_secs(600);
/// Restarts after a crash before an extension stays down until `/reload`.
const MAX_RESTARTS: u32 = 3;
const ENTRIES: [&str; 3] = ["index.ts", "index.js", "index.mjs"];
/// Marker file in an extension folder that keeps it from starting.
const DISABLED: &str = "disabled";

/// What extensions can ask of August.
#[async_trait]
pub trait Core: Send + Sync {
    /// Sends a Markdown message to a chat.
    async fn send(&self, channel: &str, chat: &str, text: &str) -> Result<()>;
    /// Queues an agent turn in a chat.
    async fn prompt(&self, channel: &str, chat: &str, text: &str) -> Result<()>;
    /// Runs a sub-agent in a chat (fresh conversation, unattended) and returns its reply.
    async fn agent(&self, channel: &str, chat: &str, task: &str, opts: AgentOpts) -> Result<String>;
    /// Asks the user in a chat to pick one of `options`; `None` if they didn't answer.
    async fn ask(&self, channel: &str, chat: &str, question: &str, options: &[String]) -> Result<Option<String>>;
    /// Asks the user in a chat whether `action` may run.
    async fn approve(&self, channel: &str, chat: &str, action: &str) -> Result<bool>;
    /// Runs an agent tool in a chat (hooks and approvals included): `(output, is_error)`.
    async fn call_tool(&self, channel: &str, chat: &str, name: &str, input: &Value) -> Result<(String, bool)>;
    /// One completion without tools on the configured model.
    async fn llm(&self, prompt: &str, system: &str) -> Result<String>;
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

/// The chat a hook, tool or command runs for: `(channel, chat)`.
pub type ChatRef = Option<(String, String)>;

enum State {
    Running(Arc<Host>),
    Failed(String),
    Disabled,
}

struct Slot {
    name: String,
    entry: PathBuf,
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

/// `(name, entry file)` of every extension folder in `dirs`, sorted by name; the first
/// folder of a name wins.
fn discover(dirs: &[PathBuf]) -> Vec<(String, PathBuf)> {
    let mut found: Vec<(String, PathBuf)> = Vec::new();
    for e in dirs.iter().flat_map(std::fs::read_dir).flatten().filter_map(|e| e.ok()) {
        let name = e.file_name().to_string_lossy().to_string();
        if valid_name(&name) && !found.iter().any(|(n, _)| *n == name) && let Some(entry) = entry_of(&e.path()) {
            found.push((name, entry));
        }
    }
    found.sort();
    found
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

    /// Where the default extensions are written (a `disabled` marker there survives rewrites).
    fn defaults_dir(&self) -> PathBuf {
        self.dir.join(".runtime/defaults")
    }

    /// User extensions first, then the defaults.
    fn dirs(&self) -> [PathBuf; 2] {
        [self.dir.clone(), self.defaults_dir()]
    }

    fn write_defaults(&self) -> std::io::Result<()> {
        for (name, source) in DEFAULTS {
            let folder = self.defaults_dir().join(name);
            std::fs::create_dir_all(&folder)?;
            let entry = folder.join("index.ts");
            if std::fs::read_to_string(&entry).ok().as_deref() != Some(source) {
                std::fs::write(entry, source)?;
            }
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

    async fn spawn(&self, name: &str, entry: &Path) -> (u64, State) {
        let generation = self.generation.fetch_add(1, Ordering::Relaxed) + 1;
        let (bun, host_ts) = match self.runtime() {
            Ok(r) => r,
            Err(e) => return (generation, State::Failed(e)),
        };
        let core = self.core.read().unwrap().clone();
        let (me, n) = (self.me.clone(), name.to_string());
        let on_exit = Box::new(move |tail: String| {
            if let Some(me) = me.upgrade() {
                me.crashed(&n, generation, tail);
            }
        });
        let state = match Host::start(&bun, &host_ts, name, entry, core, on_exit).await {
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
        if restarts >= MAX_RESTARTS {
            eprintln!("extension {name}: crashed {restarts} times, not restarting until /reload");
            return;
        }
        let (me, name) = (self.me.clone(), name.to_string());
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(1 << restarts)).await;
            let Some(me) = me.upgrade() else { return };
            let Some(entry) = me.slot_entry(&name, generation) else { return };
            let (new_gen, state) = me.spawn(&name, &entry).await;
            let mut slots = me.slots.write().unwrap();
            if let Some(slot) = slots.iter_mut().find(|s| s.name == name && s.generation == generation) {
                eprintln!("extension {name}: restarted");
                *slot = Slot { name, entry, state, generation: new_gen, restarts: restarts + 1 };
            }
        });
    }

    fn slot_entry(&self, name: &str, generation: u64) -> Option<PathBuf> {
        let slots = self.slots.read().unwrap();
        slots.iter().find(|s| s.name == name && s.generation == generation).map(|s| s.entry.clone())
    }

    /// Stops every extension and starts what is on disk now. Returns the status report.
    pub async fn reload(&self) -> String {
        if let Err(e) = self.write_defaults() {
            eprintln!("could not write default extensions: {e}");
        }
        let found = discover(&self.dirs());
        let started = futures_util::future::join_all(found.iter().map(|(name, entry)| async move {
            match entry.with_file_name(DISABLED).exists() {
                true => (0, State::Disabled),
                false => self.spawn(name, entry).await,
            }
        }))
        .await;
        let slots = found
            .into_iter()
            .zip(started)
            .map(|((name, entry), (generation, state))| Slot { name, entry, state, generation, restarts: 0 })
            .collect();
        *self.slots.write().unwrap() = slots; // old processes are killed as they drop
        self.status()
    }

    /// (Re)starts one extension after it was saved, enabling it. Returns its status line.
    pub async fn load(&self, name: &str) -> Result<String> {
        let entry = self.entry(name)?;
        std::fs::remove_file(entry.with_file_name(DISABLED)).ok();
        let (generation, state) = self.spawn(name, &entry).await;
        let ok = matches!(state, State::Running(_));
        let slot = Slot { name: name.to_string(), entry, state, generation, restarts: 0 };
        let line = {
            let mut slots = self.slots.write().unwrap();
            slots.retain(|s| s.name != name);
            slots.push(slot);
            slots.sort_by(|a, b| a.name.cmp(&b.name));
            self.describe(slots.iter().find(|s| s.name == name).unwrap(), &builtin_tools())
        };
        if !ok {
            anyhow::bail!("{line}");
        }
        Ok(line)
    }

    fn entry(&self, name: &str) -> Result<PathBuf> {
        let found = discover(&self.dirs()).into_iter().find(|(n, _)| n == name);
        found.map(|(_, entry)| entry).ok_or_else(|| anyhow::anyhow!("no extension named `{name}`"))
    }

    /// Stops an extension and keeps it from starting until it is enabled or saved again.
    pub fn disable(&self, name: &str) -> Result<()> {
        let entry = self.entry(name)?;
        std::fs::write(entry.with_file_name(DISABLED), "")?;
        let mut slots = self.slots.write().unwrap();
        slots.retain(|s| s.name != name); // the process is killed as it drops
        slots.push(Slot { name: name.to_string(), entry, state: State::Disabled, generation: 0, restarts: 0 });
        slots.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(())
    }

    /// `/extensions [enable|disable <name>]`: the status, after the change if one was asked for.
    pub async fn command(&self, args: &str) -> String {
        let r = match args.split_whitespace().collect::<Vec<_>>()[..] {
            [] => return self.status(),
            ["enable", name] => self.load(name).await.map(|_| ()),
            ["disable", name] => self.disable(name),
            _ => return "Usage: /extensions [enable|disable <name>]".into(),
        };
        match r {
            Ok(()) => self.status(),
            Err(e) => format!("{e:#}\n\n{}", self.status()),
        }
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

    /// Runs `event` through every extension that handles it, in name order. Each one gets
    /// the data the previous one returned; a `block` or `handled` result stops the chain.
    /// Failing handlers are skipped.
    pub async fn emit(&self, event: &str, mut data: Value, chat: &ChatRef) -> Value {
        for (name, host) in self.running() {
            if !host.manifest().events.iter().any(|e| e == event) {
                continue;
            }
            let params = json!({"name": event, "data": data, "ctx": ctx_json(chat)});
            let timeout = if event == "message_in" { MESSAGE_TIMEOUT } else { EVENT_TIMEOUT };
            match host.request("event", params, timeout).await {
                Ok(v) if v.is_object() => data = v,
                Ok(_) => {}
                Err(e) => eprintln!("extension {name}: `{event}` hook failed: {e}"),
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

    pub fn has_tool(&self, name: &str) -> bool {
        self.running().iter().any(|(_, h)| h.manifest().tools.iter().any(|t| t.name == name))
    }

    /// Runs an extension tool; `None` if no extension has it.
    pub async fn call_tool(&self, name: &str, input: &Value, chat: &ChatRef) -> Option<Result<String, String>> {
        let host = self.running().into_iter().find(|(_, h)| h.manifest().tools.iter().any(|t| t.name == name))?.1;
        let params = json!({"name": name, "input": input, "ctx": ctx_json(chat)});
        Some(host.request("tool", params, TOOL_TIMEOUT).await.map(|v| match v {
            Value::String(s) => s,
            other => other.to_string(),
        }))
    }

    /// `(name, description)` of extension commands, minus names in `reserved`.
    pub fn commands(&self, reserved: &[&str]) -> Vec<(String, String)> {
        let mut seen: HashSet<String> = reserved.iter().map(|s| s.to_string()).collect();
        self.running()
            .iter()
            .flat_map(|(_, h)| h.manifest().commands.clone())
            .filter(|(n, _)| seen.insert(n.clone()))
            .collect()
    }

    /// Runs `/name args`; `None` if no extension has the command, else the optional reply.
    pub async fn run_command(&self, name: &str, args: &str, chat: &ChatRef) -> Option<Result<Option<String>, String>> {
        let host = self.running().into_iter().find(|(_, h)| h.manifest().commands.iter().any(|(n, _)| n == name))?.1;
        let params = json!({"name": name, "args": args, "ctx": ctx_json(chat)});
        Some(host.request("command", params, COMMAND_TIMEOUT).await.map(|v| v.as_str().map(String::from)))
    }

    fn describe(&self, slot: &Slot, builtin: &HashSet<String>) -> String {
        match &slot.state {
            State::Failed(e) => format!("❌ {} — {}", slot.name, e.trim()),
            State::Disabled => format!("⏸ {} — disabled", slot.name),
            State::Running(h) => {
                let m = h.manifest();
                let mut parts = Vec::new();
                let list = |v: Vec<String>| v.join(", ");
                if !m.tools.is_empty() {
                    parts.push(format!("tools: {}", list(m.tools.iter().map(|t| t.name.clone()).collect())));
                }
                if !m.commands.is_empty() {
                    parts.push(format!("commands: {}", list(m.commands.iter().map(|c| format!("/{}", c.0)).collect())));
                }
                if !m.events.is_empty() {
                    parts.push(format!("hooks: {}", list(m.events.clone())));
                }
                let replaced: Vec<_> = m.tools.iter().filter(|t| builtin.contains(&t.name)).map(|t| t.name.clone()).collect();
                if !replaced.is_empty() {
                    parts.push(format!("replaces built-in: {}", list(replaced)));
                }
                if parts.is_empty() {
                    parts.push("registers nothing".into());
                }
                format!("✅ {} — {}", slot.name, parts.join("; "))
            }
        }
    }

    /// One line per extension, for `/extensions`.
    pub fn status(&self) -> String {
        let slots = self.slots.read().unwrap();
        if slots.is_empty() {
            return format!("No extensions. They live in `{}`.", self.dir.display());
        }
        let builtin = builtin_tools();
        slots.iter().map(|s| self.describe(s, &builtin)).collect::<Vec<_>>().join("\n")
    }
}

fn ctx_json(chat: &ChatRef) -> Value {
    match chat {
        Some((channel, chat)) => json!({"chat": {"channel": channel, "chat": chat}}),
        None => json!({"chat": {"channel": "cli", "chat": "cli"}}),
    }
}
