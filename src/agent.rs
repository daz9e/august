//! Sessions (one JSONL file each), the system prompt, the turn loop, compaction and the
//! background review that keeps memory and skills up to date.

use crate::channel::{Channel, Decision, Input, Silent};
use crate::config::{self, Config};
use crate::llm::{self, Block, Llm, Message, Role, Stop, Usage};
use crate::tools::{self, Scope};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tokio_util::sync::CancellationToken;

const MAX_STEPS: usize = 200;
const MEMORY_LIMIT: usize = 2200;
const USER_LIMIT: usize = 1375;

/// Token use of one session since this process started.
#[derive(Debug, Clone, Copy, Default)]
pub struct Stats {
    /// The latest request (its context size drives compaction).
    pub last: Usage,
    pub total: Usage,
    /// Dollars, when the provider bills per token.
    pub cost: Option<f64>,
}

pub struct App {
    pub llm: Llm,
    pub stats: Mutex<HashMap<String, Stats>>,
    /// (session, reason) pairs the user chose to always allow.
    always: Mutex<HashSet<(String, &'static str)>>,
}

impl App {
    pub fn new() -> Arc<Self> {
        Arc::new(Self { llm: Llm::new(), stats: Default::default(), always: Default::default() })
    }

    pub fn stats(&self, session: &str) -> Stats {
        self.stats.lock().unwrap().get(session).copied().unwrap_or_default()
    }

    fn record(&self, session: &str, cfg: &Config, u: Usage) {
        let mut all = self.stats.lock().unwrap();
        let s = all.entry(session.into()).or_default();
        s.last = u;
        s.total += u;
        if let Some(c) = llm::cost(&cfg.provider, &cfg.model, &u) {
            *s.cost.get_or_insert(0.0) += c;
        }
    }

    /// Forgets per-session state when a conversation is archived.
    pub fn reset(&self, session: &str) {
        self.always.lock().unwrap().retain(|(s, _)| s != session);
    }
}

/// The turn was stopped by the user.
#[derive(Debug)]
pub struct Cancelled;

impl std::fmt::Display for Cancelled {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str("stopped")
    }
}
impl std::error::Error for Cancelled {}

// ---------- sessions ----------

#[derive(Serialize, Deserialize)]
#[serde(untagged)]
enum Line {
    System { system: String },
    Message(Message),
}

pub struct Session {
    pub id: String,
    /// Frozen when the session starts so the prompt cache holds for its whole life.
    pub system: String,
    pub messages: Vec<Message>,
}

fn sessions_dir() -> PathBuf {
    config::home().join("sessions")
}

fn session_path(id: &str) -> PathBuf {
    sessions_dir().join(format!("{id}.jsonl"))
}

impl Session {
    /// Loads a session, starting a new one (with a fresh system prompt) if there is none.
    pub fn load(id: &str) -> Result<Self> {
        let path = session_path(id);
        let mut s = Session { id: id.into(), system: String::new(), messages: Vec::new() };
        match std::fs::read_to_string(&path) {
            Ok(text) => {
                for (i, line) in text.lines().enumerate().filter(|(_, l)| !l.trim().is_empty()) {
                    match serde_json::from_str(line).with_context(|| format!("{}:{}", path.display(), i + 1))? {
                        Line::System { system } => s.system = system,
                        Line::Message(m) => s.messages.push(m),
                    }
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                s.system = system_prompt();
                std::fs::create_dir_all(sessions_dir())?;
                std::fs::write(&path, serde_json::to_string(&Line::System { system: s.system.clone() })? + "\n")?;
            }
            Err(e) => return Err(e.into()),
        }
        // A crash between a tool call and its result would leave the API rejecting the history.
        if let Some(fix) = s.messages.last().and_then(interrupted_results) {
            s.append(std::slice::from_ref(&fix))?;
            s.messages.push(fix);
        }
        Ok(s)
    }

    fn append(&self, messages: &[Message]) -> Result<()> {
        append_to(&session_path(&self.id), messages)
    }

    /// User messages typed by a person (not tool results).
    fn user_messages(&self) -> usize {
        self.messages.iter().filter(|m| m.role == Role::User && !m.text().is_empty()).count()
    }
}

fn append_to(path: &std::path::Path, messages: &[Message]) -> Result<()> {
    let mut f = std::fs::OpenOptions::new().append(true).create(true).open(path)?;
    for m in messages {
        writeln!(f, "{}", serde_json::to_string(m)?)?;
    }
    Ok(())
}

/// Results for tool calls in `last` that never got one.
fn interrupted_results(last: &Message) -> Option<Message> {
    let results: Vec<Block> = last
        .content
        .iter()
        .filter_map(|b| match b {
            Block::ToolUse { id, .. } if last.role == Role::Assistant => {
                Some(Block::ToolResult { tool_use_id: id.clone(), content: "[interrupted: August stopped before this finished]".into(), is_error: true })
            }
            _ => None,
        })
        .collect();
    (!results.is_empty()).then_some(Message { role: Role::User, content: results })
}

/// A conversation's messages; with a file, each message is written the moment it exists.
struct History<'a> {
    msgs: &'a mut Vec<Message>,
    file: Option<PathBuf>,
}

impl History<'_> {
    fn push(&mut self, m: Message) -> Result<()> {
        if let Some(f) = &self.file {
            append_to(f, std::slice::from_ref(&m))?;
        }
        self.msgs.push(m);
        Ok(())
    }
}

/// Moves the session to `sessions/archive/`; the next message starts a new one.
pub fn archive(id: &str) -> Result<bool> {
    let path = session_path(id);
    if !path.exists() {
        return Ok(false);
    }
    let dir = sessions_dir().join("archive");
    std::fs::create_dir_all(&dir)?;
    let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S");
    std::fs::rename(&path, dir.join(format!("{id}-{stamp}.jsonl")))?;
    Ok(true)
}

// ---------- system prompt ----------

fn read_or_empty(p: PathBuf) -> String {
    std::fs::read_to_string(p).unwrap_or_default().trim().to_string()
}

/// `name: description (path)` for every `skills/*/SKILL.md`.
fn skills() -> Vec<String> {
    let dir = config::home().join("skills");
    let mut out = Vec::new();
    for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
        let path = entry.path().join("SKILL.md");
        let Ok(text) = std::fs::read_to_string(&path) else { continue };
        let field = |k: &str| {
            text.lines()
                .skip(1)
                .take_while(|l| l.trim() != "---")
                .find_map(|l| l.strip_prefix(k).and_then(|r| r.strip_prefix(':')))
                .map(|v| v.trim().trim_matches('"').to_string())
        };
        let name = field("name").unwrap_or_else(|| entry.file_name().to_string_lossy().into());
        out.push(format!("- {name}: {} ({})", field("description").unwrap_or_default(), path.display()));
    }
    out.sort();
    out
}

fn memory_section(title: &str, file: &str, limit: usize) -> String {
    let text = read_or_empty(config::home().join(file));
    let n = text.chars().count();
    let over = if n > limit { " OVER LIMIT: consolidate it now" } else { "" };
    format!("## {title} ({file}, {n}/{limit} chars{over})\n{}", if text.is_empty() { "(empty)" } else { &text })
}

pub fn system_prompt() -> String {
    let home = config::home();
    let skills = skills();
    format!(
        r#"You are August, a personal AI agent running on the user's own computer ({os}). The user talks to you from Telegram or the terminal.

Be direct: match the length of the reply to the weight of the ask. No filler, no restating the request, no narrating tool calls. Finished work gets a short report of what changed, what's verified and what's left. When unsure, say so. Reply in the user's language.

You act through tools: bash, read, write, edit, send_file, and web search when it is available. Commands run in the workspace {workspace}; files the user sends you are saved in its inbox/ folder. Each user message starts with its local timestamp. Messages the user sends while you are working are added after your tool results; take them into account right away. Dangerous shell commands (deleting files recursively, sudo, git push, killing processes, ...) ask the user first; if they say no, don't look for a way around it.

Your files live in {home}:
- MEMORY.md (your notes: environment facts, conventions, tool quirks) and USER.md (who the user is: name, preferences, style). Both are loaded at the start of every session, so keep them small: hard limits {MEMORY_LIMIT} and {USER_LIMIT} chars. Write declarative facts ("User prefers short answers"), not orders to yourself. When a file is full, merge or drop stale entries. Only facts that matter in every session belong here.
- skills/<name>/SKILL.md: how to do a class of task for this user. Frontmatter with `name` and `description`, then the procedure, commands and pitfalls. Read the skill before doing a task it covers. When you work out a non-trivial workflow, or the user corrects how you did something, create or fix the skill. Task-specific knowledge goes into skills, not memory.
- sessions/*.jsonl and sessions/archive/*.jsonl: past conversations. When the user refers to something from before, grep them instead of asking again.
- cron.json: scheduled jobs, a JSON array of {{"id": "...", "schedule": "<cron expression, local time>" or "at": "<RFC 3339 time>", "prompt": "...", "chat": <optional Telegram chat id>}}. A job runs in a fresh session with no memory of this chat, so its prompt must be self-contained; its final reply is sent to the user on Telegram, and a reply of exactly [SILENT] sends nothing. One-shot "at" jobs are removed after they run. Keep the file valid JSON.

{memory}

{user}

## Skills
{skills}
"#,
        os = std::env::consts::OS,
        workspace = config::workspace().display(),
        home = home.display(),
        memory = memory_section("Memory", "MEMORY.md", MEMORY_LIMIT),
        user = memory_section("User", "USER.md", USER_LIMIT),
        skills = if skills.is_empty() { "(none yet)".to_string() } else { skills.join("\n") },
    )
}

// ---------- the turn ----------

fn stamp() -> String {
    chrono::Local::now().format("%Y-%m-%d %H:%M %a %:z").to_string()
}

fn user_blocks(input: &Input, prefix: &str) -> Vec<Block> {
    let mut blocks = vec![Block::Text { text: format!("{prefix}{}", input.text) }];
    for p in &input.images {
        if let Some(media_type) = llm::image_type(p) {
            blocks.push(Block::Image { media_type: media_type.into(), path: p.display().to_string() });
        }
    }
    blocks
}

/// Runs one user message to the final reply, persisting everything it produced.
pub async fn turn(app: &Arc<App>, id: &str, input: Input, cancel: &CancellationToken, ch: &dyn Channel) -> Result<String> {
    let cfg = Config::load()?;
    let mut s = Session::load(id)?;
    let mut h = History { msgs: &mut s.messages, file: Some(session_path(id)) };
    h.push(Message { role: Role::User, content: user_blocks(&input, &format!("[{}] ", stamp())) })?;
    let reply = run(app, id, &cfg, &s.system, &mut h, Scope::Full, cancel, ch).await?;

    if cfg.review_every > 0 && s.user_messages() % cfg.review_every == 0 {
        tokio::spawn(review(app.clone(), id.to_string(), cfg.clone(), s.system.clone(), s.messages.clone()));
    }
    let context = app.stats(id).last.context();
    if context > cfg.context_tokens {
        log(&format!("{id}: context {context} > {}, compacting", cfg.context_tokens));
        if let Err(e) = compact(app, id).await {
            log(&format!("{id}: compaction failed: {e:#}"));
        }
    }
    Ok(reply)
}

/// The model/tool loop. Every message goes into `h` as it is produced, also when stopped or failing.
#[allow(clippy::too_many_arguments)]
async fn run(
    app: &App,
    id: &str,
    cfg: &Config,
    system: &str,
    h: &mut History<'_>,
    scope: Scope,
    cancel: &CancellationToken,
    ch: &dyn Channel,
) -> Result<String> {
    let specs = tools::specs();
    for _ in 0..MAX_STEPS {
        let mut on_text = |t: &str| ch.text(t);
        let c = tokio::select! {
            _ = cancel.cancelled() => bail!(Cancelled),
            r = app.llm.complete(cfg, system, h.msgs, &specs, &mut on_text) => r?,
        };
        app.record(id, cfg, c.usage);
        for summary in c.message.content.iter().filter_map(llm::activity) {
            ch.tool(&summary);
            ch.tool_done("", false);
        }
        let calls: Vec<(String, String, Value)> = c
            .message
            .content
            .iter()
            .filter_map(|b| match b {
                Block::ToolUse { id, name, input } => Some((id.clone(), name.clone(), input.clone())),
                _ => None,
            })
            .collect();
        let text = c.message.text();
        h.push(c.message)?;
        if calls.is_empty() {
            match c.stop {
                // A long server-side tool run (web search) paused; let the model continue.
                Stop::Other(r) if r == "pause_turn" => continue,
                Stop::MaxTokens => return Ok(format!("{text}\n\n[cut off: output limit reached]")),
                Stop::Other(r) if text.is_empty() => return Ok(format!("[stopped: {r}]")),
                _ => return Ok(text),
            }
        }
        let mut results = Vec::new();
        for (call_id, name, input) in calls {
            ch.tool(&tools::summary(&name, &input));
            let r = tokio::select! {
                _ = cancel.cancelled() => Err(anyhow::anyhow!("cancelled by the user")),
                r = call(app, id, cfg, &name, &input, scope, ch) => r,
            };
            let is_error = r.is_err();
            let content = r.unwrap_or_else(|e| format!("error: {e:#}"));
            ch.tool_done(&content, is_error);
            results.push(Block::ToolResult { tool_use_id: call_id, content, is_error });
        }
        for input in ch.steer() {
            results.extend(user_blocks(&input, "[The user sent this while you were working]\n"));
        }
        h.push(Message { role: Role::User, content: results })?;
        if cancel.is_cancelled() {
            bail!(Cancelled);
        }
    }
    Ok(format!("[stopped after {MAX_STEPS} steps]"))
}

/// One tool call, with approval for dangerous commands and channel-bound tools.
async fn call(app: &App, id: &str, cfg: &Config, name: &str, input: &Value, scope: Scope, ch: &dyn Channel) -> Result<String> {
    match name {
        "send_file" => {
            let path = tools::path(input["path"].as_str().context("missing `path`")?, scope)?;
            anyhow::ensure!(path.is_file(), "{} is not a file", path.display());
            ch.send_file(&path, input["caption"].as_str().unwrap_or_default()).await
        }
        "bash" if cfg.approvals && matches!(scope, Scope::Full) => {
            let command = input["command"].as_str().unwrap_or_default();
            if let Some(reason) = tools::dangerous(command) {
                let key = (id.to_string(), reason);
                if !app.always.lock().unwrap().contains(&key) {
                    match ch.approve(command, reason).await {
                        Decision::Yes => {}
                        Decision::Always => {
                            app.always.lock().unwrap().insert(key);
                        }
                        Decision::No => bail!("the user did not allow this command ({reason})"),
                    }
                }
            }
            tools::run(name, input, scope).await
        }
        _ => tools::run(name, input, scope).await,
    }
}

// ---------- compaction ----------

const COMPACT_PROMPT: &str = "Your context is full. Write a summary of this conversation for your future self, who will continue it without the history. Include: what the user wants and is working on, decisions made, important facts, files and commands involved, what is done and what is pending, and anything the user asked you to remember. Be specific and complete but concise. Do not call tools; reply with the summary only.";

/// Replaces the session with a new one that starts from a summary of it.
pub async fn compact(app: &App, id: &str) -> Result<()> {
    log(&format!("{id}: compacting"));
    let cfg = Config::load()?;
    let mut s = Session::load(id)?;
    if s.user_messages() == 0 {
        bail!("nothing to compact");
    }
    s.messages.push(Message::user(COMPACT_PROMPT));
    // Same tools as the session so the cached prefix is reused.
    let c = app.llm.complete(&cfg, &s.system, &s.messages, &tools::specs(), &mut |_: &str| {}).await?;
    app.record(id, &cfg, c.usage);
    let summary = c.message.text();
    if summary.trim().is_empty() {
        bail!("the model returned an empty summary");
    }
    archive(id)?;
    let fresh = Session::load(id)?;
    fresh.append(&[
        Message::user(format!("[Summary of the earlier conversation]\n\n{summary}")),
        Message { role: Role::Assistant, content: vec![Block::Text { text: "Got it, continuing from the summary.".into() }] },
    ])?;
    Ok(())
}

// ---------- background review ----------

const REVIEW_PROMPT: &str = r#"Background review (the user does not see this). Look back over the conversation above and update your memory and skills so the next session starts out knowing what this one learned.

- USER.md: who the user is: preferences, communication and work style, personal details they shared, expectations of you.
- MEMORY.md: facts about the environment: paths, tool quirks, conventions, config gotchas.
One fact goes to one file. Keep both within their limits by merging or dropping stale entries. Write declarative facts, not orders.

- Skills (skills/<name>/SKILL.md): the procedure for a class of task, as this user wants it done. Signals: the user corrected your style, format or approach; a non-trivial technique or fix emerged; a skill you used turned out wrong or incomplete. Prefer fixing an existing skill over creating a new one; name new skills by class of task, never after today's incident. Read a file before editing it.

Do not save: errors caused by missing setup, claims that some tool "does not work", one-off task details, or approaches that never actually worked.

Only file tools inside the august home are available. If nothing is worth saving, reply "Nothing to save." Otherwise make the edits and reply with one line on what changed."#;

async fn review(app: Arc<App>, id: String, cfg: Config, system: String, mut msgs: Vec<Message>) {
    msgs.push(Message::user(REVIEW_PROMPT));
    let cancel = CancellationToken::new();
    let mut h = History { msgs: &mut msgs, file: None };
    match run(&app, &id, &cfg, &system, &mut h, Scope::HomeFilesOnly, &cancel, &Silent).await {
        Ok(reply) => log(&format!("{id}: review: {}", reply.lines().next().unwrap_or_default())),
        Err(e) => log(&format!("review failed: {e:#}")),
    }
}

// ---------- log ----------

pub static VERBOSE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Appends to `august.log`; `august serve` also prints it.
pub fn log(msg: &str) {
    let line = format!("{} {msg}", chrono::Local::now().format("%Y-%m-%d %H:%M:%S"));
    if VERBOSE.load(std::sync::atomic::Ordering::Relaxed) {
        eprintln!("{line}");
    }
    if let Ok(mut f) = std::fs::OpenOptions::new().append(true).create(true).open(config::home().join("august.log")) {
        writeln!(f, "{line}").ok();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn closes_dangling_tool_calls() {
        let call = Message {
            role: Role::Assistant,
            content: vec![
                Block::Text { text: "checking".into() },
                Block::ToolUse { id: "a".into(), name: "bash".into(), input: json!({}) },
                Block::ToolUse { id: "b".into(), name: "read".into(), input: json!({}) },
            ],
        };
        let fix = interrupted_results(&call).unwrap();
        assert_eq!(fix.role, Role::User);
        let ids: Vec<&str> = fix.content.iter().filter_map(|b| match b {
            Block::ToolResult { tool_use_id, .. } => Some(tool_use_id.as_str()),
            _ => None,
        }).collect();
        assert_eq!(ids, ["a", "b"]);
        assert!(interrupted_results(&Message::user("hi")).is_none());
        assert!(interrupted_results(&fix).is_none());
    }
}
