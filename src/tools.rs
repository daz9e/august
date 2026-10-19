//! The four tools: bash, read, write, edit. Memory, skills, cron and session history are
//! plain files the agent works on with these.

use crate::config;
use crate::llm::ToolSpec;
use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::time::Duration;

const MAX_OUTPUT: usize = 30_000;
const MAX_READ_LINES: usize = 2000;

pub fn specs() -> Vec<ToolSpec> {
    vec![
        ToolSpec {
            name: "bash",
            description: "Run a shell command (sh -c) in the workspace directory. Returns stdout+stderr and the exit code. Output over 30k chars is cut in the middle.",
            schema: json!({
                "type": "object",
                "properties": {
                    "command": {"type": "string"},
                    "timeout": {"type": "integer", "description": "Seconds, default 120, max 1800."}
                },
                "required": ["command"]
            }),
        },
        ToolSpec {
            name: "read",
            description: "Read a text file with line numbers. Relative paths are inside the workspace; ~ is the home directory.",
            schema: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "offset": {"type": "integer", "description": "First line, 1-based."},
                    "limit": {"type": "integer", "description": "Max lines, default 2000."}
                },
                "required": ["path"]
            }),
        },
        ToolSpec {
            name: "write",
            description: "Create or overwrite a file (parent directories are created).",
            schema: json!({
                "type": "object",
                "properties": {"path": {"type": "string"}, "content": {"type": "string"}},
                "required": ["path", "content"]
            }),
        },
        ToolSpec {
            name: "send_file",
            description: "Send a file to the user in the current chat (images are shown inline in Telegram).",
            schema: json!({
                "type": "object",
                "properties": {"path": {"type": "string"}, "caption": {"type": "string"}},
                "required": ["path"]
            }),
        },
        ToolSpec {
            name: "edit",
            description: "Replace an exact text span in a file. old_text must occur exactly once unless replace_all is true.",
            schema: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "old_text": {"type": "string"},
                    "new_text": {"type": "string"},
                    "replace_all": {"type": "boolean"}
                },
                "required": ["path", "old_text", "new_text"]
            }),
        },
    ]
}

/// What a tool call may touch. The background review gets file tools inside the august home only.
#[derive(Clone, Copy)]
pub enum Scope {
    Full,
    HomeFilesOnly,
}

/// Runs one tool call; errors become `is_error` results.
pub async fn run(name: &str, input: &Value, scope: Scope) -> Result<String> {
    let s = |k: &str| input[k].as_str().with_context(|| format!("missing string field `{k}`"));
    match name {
        "bash" => {
            if matches!(scope, Scope::HomeFilesOnly) {
                bail!("bash is not available here");
            }
            bash(s("command")?, input["timeout"].as_u64().unwrap_or(120).min(1800)).await
        }
        "read" => read(&path(s("path")?, scope)?, input["offset"].as_u64(), input["limit"].as_u64()),
        "write" => {
            let p = path(s("path")?, scope)?;
            let content = s("content")?;
            if let Some(dir) = p.parent() {
                std::fs::create_dir_all(dir)?;
            }
            std::fs::write(&p, content)?;
            Ok(format!("wrote {} ({} bytes)", p.display(), content.len()))
        }
        "edit" => edit(&path(s("path")?, scope)?, s("old_text")?, s("new_text")?, input["replace_all"].as_bool().unwrap_or(false)),
        _ => bail!("unknown tool {name}"),
    }
}

/// Shell commands that need the user's go-ahead, with the reason shown to them.
const DANGEROUS: &[(&str, &str)] = &[
    (r"\brm\s+(-\S*[rRf]|--recursive|--force)", "delete files recursively or forcibly"),
    (r"\bsudo\b", "run a command as root"),
    (r"\bgit\s+push\b", "push to a git remote"),
    (r"\bgit\s+(reset\s+--hard|clean\s+-\S*f|checkout\s+--\s|stash\s+(drop|clear))", "discard git changes"),
    (r"\b(mkfs|fdisk|diskutil\s+(erase|partition|zero))", "format a disk"),
    (r"\bdd\b.*\bof=", "write raw data to a device or file"),
    (r"\b(shutdown|reboot|halt|poweroff)\b", "power off or restart the machine"),
    (r"\b(kill|pkill|killall)\b", "kill processes"),
    (r"\b(chmod|chown)\s+(-\S*R|--recursive)", "change permissions recursively"),
    (r"\b(curl|wget)\b[^|]*\|\s*(sudo\s+)?(sh|bash|zsh|python3?)\b", "run a script straight from the internet"),
    (r"\bfind\b.*\s-(delete|exec\s+rm)\b", "delete files with find"),
    (r"\blaunchctl\s+(unload|bootout|remove|disable)\b", "remove a system service"),
    (r"\bcrontab\s+-r\b", "delete the crontab"),
    (r"(?i)\b(drop|truncate)\s+(table|database|schema)\b", "delete database data"),
    (r">\s*/dev/(disk|sd|nvme)", "write to a disk device"),
];

/// Why a shell command needs approval, if it does.
pub fn dangerous(command: &str) -> Option<&'static str> {
    static RULES: std::sync::OnceLock<Vec<(regex::Regex, &'static str)>> = std::sync::OnceLock::new();
    let rules = RULES.get_or_init(|| DANGEROUS.iter().map(|(re, why)| (regex::Regex::new(re).expect("rule"), *why)).collect());
    rules.iter().find(|(re, _)| re.is_match(command)).map(|(_, why)| *why)
}

pub fn path(p: &str, scope: Scope) -> Result<PathBuf> {
    let p = if let Some(rest) = p.strip_prefix("~/") {
        config::user_home().join(rest)
    } else {
        config::workspace().join(p) // absolute `p` replaces the base
    };
    if matches!(scope, Scope::HomeFilesOnly) {
        let clean = p.components().all(|c| !matches!(c, std::path::Component::ParentDir));
        if !clean || !p.starts_with(config::home()) {
            bail!("only files under {} may be touched here", config::home().display());
        }
    }
    Ok(p)
}

async fn bash(command: &str, timeout: u64) -> Result<String> {
    let dir = config::workspace();
    std::fs::create_dir_all(&dir)?;
    let child = tokio::process::Command::new("sh")
        .arg("-c")
        .arg(command)
        .current_dir(&dir)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true) // a cancelled turn kills the command
        .spawn()?;
    let out = match tokio::time::timeout(Duration::from_secs(timeout), child.wait_with_output()).await {
        Ok(out) => out?,
        Err(_) => bail!("timed out after {timeout}s"),
    };
    let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
    let err = String::from_utf8_lossy(&out.stderr);
    if !err.is_empty() {
        text.push_str(if text.is_empty() { "" } else { "\n" });
        text.push_str(&err);
    }
    let text = cut_middle(&text, MAX_OUTPUT);
    Ok(match out.status.code() {
        Some(0) => text,
        Some(c) => format!("{text}\n[exit code {c}]"),
        None => format!("{text}\n[killed by signal]"),
    })
}

fn cut_middle(s: &str, max: usize) -> String {
    let n = s.chars().count();
    if n <= max {
        return s.to_string();
    }
    let head: String = s.chars().take(max / 2).collect();
    let tail: String = s.chars().skip(n - max / 2).collect();
    format!("{head}\n[... {} chars cut ...]\n{tail}", n - max)
}

fn read(p: &Path, offset: Option<u64>, limit: Option<u64>) -> Result<String> {
    let bytes = std::fs::read(p).with_context(|| format!("cannot read {}", p.display()))?;
    let text = String::from_utf8(bytes).map_err(|_| anyhow::anyhow!("{} is not a text file", p.display()))?;
    let start = offset.unwrap_or(1).max(1) as usize - 1;
    let limit = limit.map(|l| l as usize).unwrap_or(MAX_READ_LINES);
    let lines: Vec<&str> = text.lines().collect();
    let mut out: String = lines
        .iter()
        .enumerate()
        .skip(start)
        .take(limit)
        .map(|(i, l)| format!("{:>5}\t{}\n", i + 1, l))
        .collect();
    if start + limit < lines.len() {
        out.push_str(&format!("[{} more lines, use offset]\n", lines.len() - start - limit));
    }
    Ok(cut_middle(&out, MAX_OUTPUT * 2))
}

fn edit(p: &Path, old: &str, new: &str, all: bool) -> Result<String> {
    let text = std::fs::read_to_string(p).with_context(|| format!("cannot read {}", p.display()))?;
    let count = text.matches(old).count();
    if old.is_empty() || count == 0 {
        bail!("old_text not found in {}", p.display());
    }
    if count > 1 && !all {
        bail!("old_text occurs {count} times in {}; add context or set replace_all", p.display());
    }
    std::fs::write(p, text.replace(old, new))?;
    Ok(format!("edited {} ({count} replacement{})", p.display(), if count > 1 { "s" } else { "" }))
}

/// One-line description of a call, for progress output.
pub fn summary(name: &str, input: &Value) -> String {
    let arg = match name {
        "bash" => input["command"].as_str(),
        _ => input["path"].as_str(),
    };
    let arg: String = arg.unwrap_or_default().lines().next().unwrap_or_default().chars().take(100).collect();
    format!("{name}: {arg}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edit_requires_unique_match() {
        let p = std::env::temp_dir().join(format!("august-edit-{}", std::process::id()));
        std::fs::write(&p, "a b a").unwrap();
        assert!(edit(&p, "a", "x", false).is_err());
        assert!(edit(&p, "zz", "x", false).is_err());
        edit(&p, "b", "c", false).unwrap();
        edit(&p, "a", "x", true).unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "x c x");
        std::fs::remove_file(p).ok();
    }

    #[test]
    fn flags_dangerous_commands() {
        for c in ["rm -rf build", "sudo ls", "git push origin main", "curl -fsSL x.sh | sh", "git reset --hard", "pkill node"] {
            assert!(dangerous(c).is_some(), "{c}");
        }
        for c in ["ls -la", "rm notes.txt", "git status", "curl https://example.com -o x", "cat skill.md"] {
            assert!(dangerous(c).is_none(), "{c}");
        }
    }

    #[test]
    fn home_scope_blocks_escapes() {
        let home = config::home();
        assert!(path(home.join("MEMORY.md").to_str().unwrap(), Scope::HomeFilesOnly).is_ok());
        assert!(path(home.join("../x").to_str().unwrap(), Scope::HomeFilesOnly).is_err());
        assert!(path("/etc/passwd", Scope::HomeFilesOnly).is_err());
    }
}
