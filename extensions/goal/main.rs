//! `/goal <text>`: after every turn a separate judging call checks the goal; while it isn't
//! reached the chat keeps working on it, up to AUGUST_GOAL_TURNS turns (default 20).
//! Goals live in memory: a restart, /goal clear, /new or /stop drops them.

use august_ext::{August, Ctx};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

const JUDGE: &str = "You check whether an AI assistant has reached a goal the user set. You get the goal and \
    the assistant's latest reply. Answer `DONE` if the reply shows the goal is fully achieved \
    (or can't be achieved and the assistant explained why), otherwise `CONTINUE: ` and one \
    sentence on what is still missing. Output only that line.";

#[derive(Clone)]
struct Goal {
    /// Tells a goal from a later one set in the same chat.
    id: u64,
    text: String,
    turns: u32,
}

#[derive(Default)]
struct Goals {
    by_chat: HashMap<String, Goal>,
    next_id: u64,
}

type Shared = Arc<Mutex<Goals>>;

/// After a turn: asks the model whether the goal is reached, then reports or continues.
async fn judge(goals: Shared, max_turns: u32, data: Value, ctx: Ctx) -> anyhow::Result<()> {
    let k = ctx.key();
    let Some(goal) = goals.lock().unwrap().by_chat.get(&k).cloned() else { return Ok(()) };
    if data["unattended"] == true || data["status"] == "cancelled" {
        return Ok(()); // not the user's turn, or /stop (which drops the goal itself)
    }
    if data["status"] == "error" {
        goals.lock().unwrap().by_chat.retain(|_, g| g.id != goal.id);
        let why = data["error"].as_str().unwrap_or("unknown error");
        return ctx.send(&format!("⏸ Goal paused: the last turn failed ({why}). Set it again with /goal to continue.")).await.map(drop);
    }
    let reply = data["reply"].as_str().unwrap_or_default();
    let verdict = match ctx.llm(&format!("Goal:\n{}\n\nThe assistant's latest reply:\n{reply}", goal.text), Some(JUDGE)).await {
        Ok(v) => v.trim().to_string(),
        Err(e) => {
            let ours = {
                let mut g = goals.lock().unwrap();
                let ours = g.by_chat.get(&k).is_some_and(|current| current.id == goal.id);
                if ours {
                    g.by_chat.remove(&k);
                }
                ours
            };
            if !ours {
                return Ok(()); // dropped or replaced while judging
            }
            return ctx.send(&format!("⚠️ Goal paused: could not check it ({e:#}). Set it again with /goal to continue.")).await.map(drop);
        }
    };
    let turns = {
        let mut g = goals.lock().unwrap();
        match g.by_chat.get_mut(&k) {
            Some(current) if current.id == goal.id => {
                if verdict.starts_with("DONE") {
                    g.by_chat.remove(&k);
                    None
                } else {
                    current.turns += 1;
                    Some(current.turns)
                }
            }
            _ => return Ok(()), // dropped or replaced while judging
        }
    };
    let Some(turns) = turns else {
        return ctx.send(&format!("🎯 Goal reached: {}", goal.text)).await.map(drop);
    };
    let rest = verdict.strip_prefix("CONTINUE").unwrap_or(&verdict);
    let rest = rest.trim_start_matches(|c: char| c == ':' || c.is_whitespace()).trim();
    let missing = if rest.is_empty() { "the goal isn't reached yet" } else { rest };
    if turns >= max_turns {
        goals.lock().unwrap().by_chat.remove(&k);
        return ctx.send(&format!("⏸ Goal paused after {turns} turns. Still missing: {missing}\nSet it again with /goal to continue.")).await.map(drop);
    }
    ctx.prompt(&format!("[Goal not reached yet (turn {turns} of {max_turns}): {missing}] Keep working towards the goal: {}", goal.text)).await
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let max_turns: u32 = std::env::var("AUGUST_GOAL_TURNS").ok().and_then(|v| v.parse().ok()).filter(|&n| n > 0).unwrap_or(20);
    let goals: Shared = Arc::default();
    let august = August::new();
    august.needs(&["llm", "messaging"]);

    let g = goals.clone();
    august.register_command("goal", "Keep working until a goal is reached (/goal clear to stop)", move |args, ctx| {
        let goals = g.clone();
        async move {
            let k = ctx.key();
            if args.is_empty() {
                return Ok(Some(match goals.lock().unwrap().by_chat.get(&k) {
                    Some(goal) => format!("🎯 Goal: {} ({} turns so far). /goal clear to drop it.", goal.text, goal.turns),
                    None => "No goal. Set one with /goal <what should be achieved>.".into(),
                }));
            }
            if args == "clear" {
                let dropped = goals.lock().unwrap().by_chat.remove(&k).is_some();
                return Ok(Some(if dropped { "Goal dropped." } else { "No goal to drop." }.into()));
            }
            {
                let mut g = goals.lock().unwrap();
                g.next_id += 1;
                let goal = Goal { id: g.next_id, text: args.clone(), turns: 0 };
                g.by_chat.insert(k, goal);
            }
            ctx.prompt(&format!("[New goal] {args}\nWork on it until it is achieved; I'll check after each turn.")).await?;
            Ok(Some(format!("🎯 Goal set: {args}")))
        }
    });

    // Otherwise the goal's loop would start the next turn.
    let g = goals.clone();
    august.on("stop", move |_, ctx| {
        let goals = g.clone();
        async move {
            let dropped = goals.lock().unwrap().by_chat.remove(&ctx.key()).is_some();
            if dropped {
                ctx.send("Goal dropped.").await?;
            }
            Ok(None)
        }
    });
    let g = goals.clone();
    august.on("session_changed", move |_, ctx| {
        let goals = g.clone();
        async move {
            goals.lock().unwrap().by_chat.remove(&ctx.key());
            Ok(None)
        }
    });
    august.on("turn_end", move |data, ctx| {
        let goals = goals.clone();
        async move { judge(goals, max_turns, data, ctx).await.map(|_| None) }
    });
    august.run().await;
}
