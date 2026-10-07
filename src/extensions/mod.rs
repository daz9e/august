//! Extensions add tools, slash commands and hooks. User extensions are TypeScript files in
//! `~/.august/extensions/<name>/index.ts`, run by bun through `host.ts`. Default extensions
//! (the repo's `extensions/<name>/main.rs`) are Rust binaries `august-ext-<name>` next to
//! `august`, speaking the same protocol; a user extension of the same name replaces one.
//! Each extension is its own process (see `host.rs`), so a broken or hanging one can't take
//! the gateway down.

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
/// The extensions that ship with August, as binaries `august-ext-<name>` next to `august`.
const DEFAULTS: &[&str] = &["browser", "clarify", "goal", "mcp", "subagents", "voice", "web"];

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

/// How an extension is started.
#[derive(Clone)]
enum Launch {
    /// A TypeScript or JavaScript entry file, run by bun through `host.ts`.
    Script(PathBuf),
    /// A default extension's binary; `dir` is its folder (state, the `disabled` marker).
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

    /// Where the default extensions are written (a `disabled` marker there survives rewrites).
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
                c.current_dir(dir).env("AUGUST_EXTENSION_DIR", dir);
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
            let mut slots = me.slots.write().unwrap();
            if let Some(slot) = slots.iter_mut().find(|s| s.name == name && s.generation == generation) {
                eprintln!("extension {name}: restarted");
                *slot = Slot { name, launch, state, generation: new_gen, restarts: restarts + 1 };
            }
        });
    }

    fn slot_launch(&self, name: &str, generation: u64) -> Option<Launch> {
        let slots = self.slots.read().unwrap();
        slots.iter().find(|s| s.name == name && s.generation == generation).map(|s| s.launch.clone())
    }

    /// Stops every extension and starts what is on disk now. Returns the status report.
    pub async fn reload(&self) -> String {
        if let Err(e) = self.prepare_defaults() {
            eprintln!("could not prepare default extensions: {e}");
        }
        let found = self.discover();
        let started = futures_util::future::join_all(found.iter().map(|(name, launch)| async move {
            match launch.dir().join(DISABLED).exists() {
                true => (0, State::Disabled),
                false => self.spawn(name, launch).await,
            }
        }))
        .await;
        let slots = found
            .into_iter()
            .zip(started)
            .map(|((name, launch), (generation, state))| Slot { name, launch, state, generation, restarts: 0 })
            .collect();
        *self.slots.write().unwrap() = slots; // old processes are killed as they drop
        self.status()
    }

    /// (Re)starts one extension after it was saved, enabling it. Returns its status line.
    pub async fn load(&self, name: &str) -> Result<String> {
        let launch = self.launch(name)?;
        std::fs::remove_file(launch.dir().join(DISABLED)).ok();
        let (generation, state) = self.spawn(name, &launch).await;
        let ok = matches!(state, State::Running(_));
        let slot = Slot { name: name.to_string(), launch, state, generation, restarts: 0 };
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

    fn launch(&self, name: &str) -> Result<Launch> {
        let found = self.discover().into_iter().find(|(n, _)| n == name);
        found.map(|(_, launch)| launch).ok_or_else(|| anyhow::anyhow!("no extension named `{name}`"))
    }

    /// Stops an extension and keeps it from starting until it is enabled or saved again.
    pub fn disable(&self, name: &str) -> Result<()> {
        let launch = self.launch(name)?;
        std::fs::write(launch.dir().join(DISABLED), "")?;
        let mut slots = self.slots.write().unwrap();
        slots.retain(|s| s.name != name); // the process is killed as it drops
        slots.push(Slot { name: name.to_string(), launch, state: State::Disabled, generation: 0, restarts: 0 });
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
