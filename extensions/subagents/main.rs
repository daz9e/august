//! `delegate_task`: hands a piece of work to a background sub-agent with a fresh context;
//! its report comes back to the chat as a new message (joining the running turn if any).

use anyhow::bail;
use august_ext::{August, str_arg};
use serde_json::json;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

const SURFACE: &str = "You are a sub-agent doing one task for the main agent, which talks to the user. You see \
    only the task below, not their conversation, and nobody can answer questions: work \
    autonomously and make reasonable assumptions. Finish with a concise report for the main \
    agent: what you did, what you found, files you changed, and anything left open or uncertain.";

/// What a sub-agent may not do: talk to the user, change memory or skills, schedule or
/// delegate more work.
const EXCLUDE: &[&str] = &[
    "delegate_task", "schedule_task", "list_tasks", "cancel_task", "remember", "forget", "send_file",
    "save_skill", "edit_skill", "save_extension",
];

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let august = August::new();
    let count = Arc::new(AtomicU64::new(0));
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
            let count = count.clone();
            async move {
                let goal = str_arg(&input, "goal").trim().to_string();
                if goal.is_empty() {
                    bail!("empty goal");
                }
                let n = count.fetch_add(1, Ordering::Relaxed) + 1;
                let title: String = goal.chars().take(80).collect();
                let context = str_arg(&input, "context");
                let task = if context.trim().is_empty() { goal.clone() } else { format!("{goal}\n\nContext:\n{context}") };
                // ponytail: sub-agents are not cancelled by /stop and not capped in number; add both if they get used heavily.
                tokio::spawn(async move {
                    let note = match ctx.agent(&task, json!({"system": SURFACE, "exclude": EXCLUDE})).await {
                        Ok(report) => format!("[Subtask #{n} finished: {title}]\n{report}"),
                        Err(e) => format!("[Subtask #{n} failed: {title}]\n{e:#}"),
                    };
                    if let Err(e) = ctx.prompt(&note).await {
                        eprintln!("could not report subtask #{n}: {e:#}");
                    }
                });
                Ok(format!("Started subtask #{n}. Its report will arrive as a new message; carry on with other work or end your turn."))
            }
        },
    );
    august.run().await;
}
