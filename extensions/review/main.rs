//! Learning after a turn: from time to time a background pass looks back at the
//! conversation and saves what is worth keeping — facts about the user (`remember`) and
//! procedures (`save_skill` / `edit_skill`). It runs after the reply was delivered, as a
//! fork of the conversation (same prompt, history and tools, so the provider's cache covers
//! almost all of it) that may only use the memory and skill tools and writes without asking.
//!
//! Settings: `AUGUST_REVIEW=off`; `AUGUST_REVIEW_MEMORY_EVERY` (user turns between memory
//! reviews, default 10, 0 = never); `AUGUST_REVIEW_SKILLS_AFTER` (tool calls after which
//! skills are reviewed, default 15, 0 = never); `AUGUST_REVIEW_NOTIFY` (`off`, `on` (default):
//! what kind of thing changed, `verbose`: every change).

use august_ext::{August, Ctx};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// What the review may run; other tools answer with an error.
const ALLOWED: &[&str] = &[
    "remember", "forget", "search_history", "load_skill", "save_skill", "edit_skill", "read",
];

const MEMORY_PROMPT: &str = "Memory: durable facts that matter in every future conversation — who \
    the user is, their preferences and corrections, their environment, people and projects. Save \
    with `remember` (one short self-contained sentence each); update or merge stale facts via \
    `replaces`; `forget` facts that turned out wrong. Skip temporary details, things easy to look \
    up again, and anything already in the Memory section.";

const SKILLS_PROMPT: &str = "Skills: how to do a class of task for this user — the steps in \
    order, the commands and tools that work, how the result should look, and the pitfalls that \
    cost time. Signals worth acting on: the user corrected your approach, format or tone; a \
    non-trivial technique, fix or workaround emerged; a skill you used was wrong, incomplete or \
    outdated. Prefer, in order: fixing a skill used in this conversation (`load_skill` it first, \
    then `edit_skill` patch), extending an existing skill that covers the class of task, adding \
    a supporting file to one, and only then a new skill (`save_skill`) named for the class of \
    task, never for today's incident. Write lessons, not logs: a pitfall is a general rule plus \
    why, with no dates, ticket numbers or quotes. Do not record environment problems the user \
    can fix (missing tools or keys) as rules, do not claim a tool \"doesn't work\", and do not \
    write up attempts that never worked as a recipe. Fix wrong text in place instead of \
    appending corrections. Built-in skills can't be changed.";

fn setting(var: &str, default: usize) -> usize {
    std::env::var(var).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

/// User turns and tool calls since the last review of each kind, per thread.
#[derive(Default)]
struct Counters {
    turns: usize,
    tool_calls: usize,
}

/// What a tool call of the review changed, for the chat.
fn change(call: &Value) -> Option<String> {
    if call["isError"] == true {
        return None;
    }
    let input = &call["input"];
    let name = input["name"].as_str().unwrap_or("?");
    Some(match call["name"].as_str()? {
        "remember" => format!("remembered: {}", input["fact"].as_str().unwrap_or("")),
        "forget" => format!("forgot #{}", input["id"]),
        "save_skill" => format!("saved skill `{name}`"),
        "edit_skill" => match input["action"].as_str().unwrap_or("") {
            "archive" => format!("archived skill `{name}`"),
            "write_file" => format!("wrote `{}` in skill `{name}`", input["file"].as_str().unwrap_or("?")),
            "remove_file" => format!("removed `{}` from skill `{name}`", input["file"].as_str().unwrap_or("?")),
            _ => format!("edited skill `{name}`"),
        },
        _ => return None,
    })
}

/// The chat line about what a review saved.
fn notice(changes: &[String]) -> Option<String> {
    let mode = std::env::var("AUGUST_REVIEW_NOTIFY").unwrap_or_default();
    if changes.is_empty() || mode == "off" {
        return None;
    }
    if mode == "verbose" {
        return Some(changes.iter().map(|c| format!("💾 {c}")).collect::<Vec<_>>().join("\n"));
    }
    let memory = changes.iter().any(|c| c.starts_with("remembered") || c.starts_with("forgot"));
    let mut skills: Vec<&str> = changes.iter().filter_map(|c| c.split('`').nth(1).filter(|_| c.contains("skill"))).collect();
    skills.dedup();
    let mut parts = Vec::new();
    if memory {
        parts.push("memory updated".to_string());
    }
    if !skills.is_empty() {
        parts.push(format!("skill {} updated", skills.iter().map(|s| format!("`{s}`")).collect::<Vec<_>>().join(", ")));
    }
    let line = parts.join(" · ");
    Some(format!("💾 {}{}", line[..1].to_uppercase(), &line[1..]))
}

/// After one of the user's turns: reviews when one is due.
async fn after_turn(august: August, counters: Arc<Mutex<HashMap<String, Counters>>>, data: Value, ctx: Ctx) -> anyhow::Result<()> {
    let visible = ctx.turn.as_ref().is_some_and(|t| t.mode == "visible");
    if !visible || data["status"] != "ok" || std::env::var("AUGUST_REVIEW").is_ok_and(|v| v == "off") {
        return Ok(());
    }
    let (every, after) = (setting("AUGUST_REVIEW_MEMORY_EVERY", 10), setting("AUGUST_REVIEW_SKILLS_AFTER", 15));
    let (memory, skills) = {
        let mut all = counters.lock().unwrap();
        let c = all.entry(ctx.key()).or_default();
        c.turns += 1;
        c.tool_calls += data["toolCalls"].as_u64().unwrap_or(0) as usize;
        let (memory, skills) = (every > 0 && c.turns >= every, after > 0 && c.tool_calls >= after);
        if memory {
            c.turns = 0;
        }
        if skills {
            c.tool_calls = 0;
        }
        (memory, skills)
    };
    if !memory && !skills {
        return Ok(());
    }
    let mut ask = String::from(
        "[Background review] The conversation above is over for now. Review it and save what \
         is worth keeping for future conversations; the user doesn't see this exchange. If \
         nothing stands out, answer \"Nothing to save.\" and stop.\n\n",
    );
    if memory {
        ask += MEMORY_PROMPT;
        ask += "\n\n";
    }
    if skills {
        ask += SKILLS_PROMPT;
    }
    let thread = ctx.thread.clone().ok_or_else(|| anyhow::anyhow!("a turn without a thread"))?;
    let turn = json!({
        "text": ask.trim_end(), "mode": "fork", "source": "review", "parent": ctx.turn.as_ref().map(|t| t.id),
        "tools": ALLOWED, "meta": {"approve": "all"},
    });
    let id = august.start_turn(&thread, turn).await?;
    let out = august.wait_turn(id, Duration::from_secs(600)).await?;
    if out["status"] != "ok" {
        eprintln!("review failed: {}", out["error"].as_str().unwrap_or(out["status"].as_str().unwrap_or("?")));
        return Ok(());
    }
    let changes: Vec<String> = out["toolCalls"].as_array().into_iter().flatten().filter_map(change).collect();
    eprintln!("review: {}", if changes.is_empty() { "nothing to save".into() } else { changes.join("; ") });
    if let Some(note) = notice(&changes) {
        ctx.send(&note).await?;
    }
    Ok(())
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let august = August::new();
    august.needs(&["turns"]);
    let counters: Arc<Mutex<HashMap<String, Counters>>> = Arc::default();
    let me = august.clone();
    august.on("turn_end", move |data, ctx| {
        let (august, counters) = (me.clone(), counters.clone());
        async move { after_turn(august, counters, data, ctx).await.map(|_| None) }
    });
    august.run().await;
}
