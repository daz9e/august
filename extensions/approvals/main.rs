//! `approvals`: built only from the core's primitives. It intercepts tool calls
//! (`tool_call`), lets a separate model call judge whether a bash command is safe, asks the
//! user in the thread (Allow / Deny, five minutes) when it isn't, and lets the call through
//! or blocks it. A yes in words allows; other words deny and, while a reply is being
//! written, reach the agent, so "no, do it this way" isn't lost.
//!
//! What it asks about: bash commands (read-only ones from `safe_commands` pass at once, the
//! rest go to the judge when `judge` is on), saving or editing skills and extensions,
//! turning extensions on and off, changing settings, scheduled tasks with a script, browser uploads. A turn started with
//! `meta: {approve: "all"}` passes without asking.

use anyhow::Result;
use august_ext::{August, Button, Ctx, Reply};
use serde_json::{Value, json};
use std::time::Duration;

const WAIT: Duration = Duration::from_secs(300);
/// Input shown in a question, at most.
const SHOWN_CHARS: usize = 3_000;

const SAFE_COMMANDS: &[&str] = &[
    "ls", "pwd", "cat", "head", "tail", "wc", "echo", "grep", "rg", "date", "whoami", "uname",
    "which", "file", "stat", "du", "df", "tree",
];

const JUDGE: &str = "You check a shell command an AI assistant wants to run on its user's machine. \
    Answer SAFE only if it can't do harm: it reads, or creates and changes files inside the \
    working directory as plainly intended. Answer ASK if it might delete or overwrite data the \
    user cares about, touch anything outside the working directory, install or remove software, \
    change system settings, reach the network to send data, reveal secrets, or if you are not \
    sure. Output one word: SAFE or ASK.";

/// Read-only by its first word, with no shell metacharacters.
fn is_safe(cmd: &str, safe: &[String]) -> bool {
    let has_meta = cmd.chars().any(|c| matches!(c, ';' | '&' | '|' | '>' | '<' | '`' | '$' | '\n' | '(' | ')'));
    let first = cmd.split_whitespace().next().unwrap_or("");
    !has_meta && safe.iter().any(|s| s == first)
}

fn is_yes(text: &str) -> bool {
    matches!(text.trim().to_lowercase().as_str(), "y" | "yes" | "ok" | "allow" | "1" | "да" | "ок")
}

/// What the user sees: the command, or the tool with its arguments.
fn shown(tool: &str, input: &Value) -> String {
    let text = match (tool, input["command"].as_str()) {
        ("bash", Some(cmd)) => format!("bash: {cmd}"),
        _ => {
            let args = input.as_object().into_iter().flatten().map(|(k, v)| format!("{k}: {}", v.as_str().map(String::from).unwrap_or_else(|| v.to_string())));
            std::iter::once(format!("{tool}:")).chain(args).collect::<Vec<_>>().join("\n")
        }
    };
    let text = text.replace("```", "'''");
    match text.char_indices().nth(SHOWN_CHARS) {
        Some((cut, _)) => format!("{}\n…", &text[..cut]),
        None => text,
    }
}

/// Whether this call needs a decision; bash commands go through the judge first.
async fn needs_asking(tool: &str, input: &Value, ctx: &Ctx, settings: &Value) -> bool {
    match tool {
        "bash" => {
            let cmd = input["command"].as_str().unwrap_or_default();
            let safe: Vec<String> = match settings["safe_commands"].as_array() {
                Some(list) => list.iter().filter_map(|v| v.as_str().map(String::from)).collect(),
                None => SAFE_COMMANDS.iter().map(|s| s.to_string()).collect(),
            };
            if is_safe(cmd, &safe) {
                return false;
            }
            if settings["judge"] == false {
                return true;
            }
            // A separate model call, so the conversation's own context can't talk it round.
            let verdict = ctx.llm(&format!("Command:\n{cmd}"), Some(JUDGE)).await.unwrap_or_default();
            !verdict.trim().to_uppercase().starts_with("SAFE")
        }
        "save_skill" | "edit_skill" | "save_extension" => true,
        "extensions" => input["action"] != "list",
        "config" => input["action"] == "set",
        "schedule_task" => input["script"].as_str().is_some_and(|s| !s.trim().is_empty()),
        "browser" => input["args"][0] == "upload",
        _ => false,
    }
}

/// Asks in the thread; `None` lets the call run, `Some(reason)` blocks it.
async fn ask(august: &August, ctx: &Ctx, action: &str) -> Result<Option<String>> {
    let Some(thread) = ctx.thread.clone() else {
        return Ok(Some("nobody to ask for approval here".into()));
    };
    let key = format!("ap.{:x}", std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_nanos());
    let (allow, deny) = (format!("{key}.0"), format!("{key}.1"));
    // Listen before sending, so a quick answer can't slip past.
    let listener = august.listen(&thread, &[&allow, &deny], true, WAIT + Duration::from_secs(5)).await?;
    let buttons = [Button { id: allow.clone(), label: "✅ Allow".into() }, Button { id: deny, label: "❌ Deny".into() }];
    let id = august.send(&thread, &format!("⚠️ **Approval needed**\n```\n{action}\n```"), &buttons).await?;
    let (verdict, blocked) = match august.next(listener, WAIT).await? {
        Reply::Press(p) if p == allow => ("✅ Allowed", None),
        Reply::Press(_) => ("❌ Denied", Some("the user denied this")),
        Reply::Text(t) if is_yes(&t) => ("✅ Allowed", None),
        Reply::Text(t) => {
            if ctx.turn.as_ref().is_some_and(|t| t.mode == "visible") {
                ctx.prompt_with(&t, json!({"source": "user"})).await.ok();
            }
            ("❌ Denied", Some("the user denied this and wrote instead"))
        }
        Reply::Cancelled(_) => ("⏹ Cancelled", Some("cancelled")),
        Reply::Timeout => ("⌛ Timed out, denied", Some("the user didn't answer; denied")),
    };
    // Messengers without edits keep the question as it was.
    august.edit(&thread, &id, &format!("{verdict}\n```\n{action}\n```")).await.ok();
    Ok(blocked.map(String::from))
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let august = August::new();
    august.needs(&["messaging", "llm"]);
    august.settings_schema(json!({
        "type": "object",
        "properties": {
            "judge": {"type": "boolean", "default": true, "description": "Let a separate model call pass harmless bash commands without asking"},
            "safe_commands": {"type": "array", "items": {"type": "string"}, "default": SAFE_COMMANDS, "description": "Commands that run without asking when they have no shell metacharacters"},
        },
    }));
    august.hook_timeout("tool_call", WAIT + Duration::from_secs(60));
    let a = august.clone();
    august.on("tool_call", move |data, ctx| {
        let a = a.clone();
        async move {
            if ctx.turn.as_ref().is_some_and(|t| t.meta["approve"] == "all") {
                return Ok(None);
            }
            let (tool, input) = (data["tool"].as_str().unwrap_or_default(), &data["input"]);
            if !needs_asking(tool, input, &ctx, &a.settings().await.unwrap_or_default()).await {
                return Ok(None);
            }
            Ok(ask(&a, &ctx, &shown(tool, input)).await?.map(|reason| json!({"block": reason})))
        }
    });
    august.run().await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safe_detection() {
        let safe: Vec<String> = SAFE_COMMANDS.iter().map(|s| s.to_string()).collect();
        assert!(is_safe("ls -la", &safe));
        assert!(is_safe("cat notes.md", &safe));
        assert!(!is_safe("rm -rf /", &safe));
        assert!(!is_safe("ls; rm x", &safe));
        assert!(!is_safe("cat a > b", &safe));
        assert!(!is_safe("echo $(whoami)", &safe));
    }
}
