//! `delegate_task`: hands a piece of work to a background sub-agent with a fresh context;
//! its report comes back to the chat as a new message (joining the running turn if any).
//! /stop and /new drop the chat's running subtasks, so no report starts a turn after them.

use anyhow::bail;
use august_ext::{August, str_arg};
use serde_json::json;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::task::AbortHandle;

const SURFACE: &str = "You are a sub-agent doing one task for the main agent, which talks to the user. You see \
    only the task below, not their conversation, and nobody can answer questions: work \
    autonomously and make reasonable assumptions. Finish with a concise report for the main \
    agent: what you did, what you found, files you changed, and anything left open or uncertain.";

/// What a sub-agent may not do: talk to the user, change memory or skills, schedule or
/// delegate more work.
const EXCLUDE: &[&str] = &[
    "clarify", "delegate_task", "schedule_task", "list_tasks", "cancel_task", "remember", "forget", "send_file",
    "save_skill", "edit_skill", "save_extension",
];

/// Running subtasks by chat.
type Running = Arc<Mutex<HashMap<String, Vec<AbortHandle>>>>;

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let august = August::new();
    let count = Arc::new(AtomicU64::new(0));
    let running: Running = Arc::default();
    let r = running.clone();
    august.register_tool(
        "delegate_task",
        "Hand a self-contained piece of work to a sub-agent that runs in the background with a \
         fresh context and the same tools (research, a long build or investigation, several \
         independent parts at once — call it once per part). The sub-agent sees nothing of this \
         conversation: put everything it needs into `goal` and `context` (paths, names, \
         constraints, what a good result looks like). Its report arrives later as a new message; \
         meanwhile you can keep talking to the user.",
        json!({
            "type": "object",
            "properties": {
                "goal": {"type": "string", "description": "What to achieve, in one or two sentences"},
                "context": {"type": "string", "description": "Everything the sub-agent needs to know"},
            },
            "required": ["goal"],
            "additionalProperties": false,
        }),
        move |input, ctx| {
            let (count, running) = (count.clone(), r.clone());
            async move {
                let goal = str_arg(&input, "goal").trim().to_string();
                if goal.is_empty() {
                    bail!("`goal` is empty: say what the sub-agent should achieve");
                }
                let n = count.fetch_add(1, Ordering::Relaxed) + 1;
                let title: String = goal.chars().take(80).collect();
                let context = str_arg(&input, "context");
                let task = if context.trim().is_empty() { goal.clone() } else { format!("{goal}\n\nContext:\n{context}") };
                let key = ctx.key();
                // ponytail: not capped in number; add a cap if they get used heavily.
                let job = tokio::spawn(async move {
                    let note = match ctx.agent(&task, json!({"system": SURFACE, "exclude": EXCLUDE})).await {
                        Ok(report) => format!("[Subtask #{n} finished: {title}]\n{report}"),
                        Err(e) => format!("[Subtask #{n} failed: {title}]\n{e:#}"),
                    };
                    if let Err(e) = ctx.prompt(&note).await {
                        eprintln!("could not report subtask #{n}: {e:#}");
                    }
                });
                let mut running = running.lock().unwrap();
                let jobs = running.entry(key).or_default();
                jobs.retain(|j| !j.is_finished());
                jobs.push(job.abort_handle());
                Ok(format!("Started subtask #{n}. Its report will arrive as a new message; carry on with other work or end your turn."))
            }
        },
    );

    // ponytail: only the report is dropped; the core still finishes the sub-agent's run,
    // as an extension can't cancel `ctx.agent` yet.
    for event in ["stop", "session_start"] {
        let running = running.clone();
        august.on(event, move |_, ctx| {
            let jobs = running.lock().unwrap().remove(&ctx.key()).unwrap_or_default();
            async move {
                let dropped = jobs.iter().filter(|j| !j.is_finished()).count();
                jobs.iter().for_each(AbortHandle::abort);
                if dropped > 0 {
                    ctx.send(&format!("Dropped {dropped} running subtask(s); their reports won't arrive.")).await?;
                }
                Ok(None)
            }
        });
    }
    august.run().await;
}
