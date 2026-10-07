//! Scheduled tasks: reminders, recurring jobs and checks that fire as turns in the thread
//! that created them. A task runs as a quiet turn in the thread's conversation (or, when
//! `isolated`, as a fresh one), with the instructions of its skills and the output of its
//! script; only the final reply reaches the thread, and `[SILENT]` sends nothing. Tasks
//! live in this extension's store; a run missed while August was down fires once when it
//! is back. `AUGUST_SCHEDULER_TICK` sets how often (seconds) due tasks are checked.

mod schedule;

use anyhow::{Result, bail};
use august_ext::{August, Ctx, Thread, str_arg, truncate};
use schedule::{Schedule, fmt_time};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::time::Duration;

const TICK: Duration = Duration::from_secs(20);
const SCRIPT_TIMEOUT: Duration = Duration::from_secs(120);

const SECTION: &str = "## Tasks\nUse `schedule_task` for reminders and recurring jobs; `list_tasks` \
    and `cancel_task` manage this chat's tasks.";

#[derive(Serialize, Deserialize, Clone)]
struct Task {
    id: u64,
    thread: Thread,
    schedule: String,
    prompt: String,
    /// Unix seconds; `None` once a one-shot task has run.
    next_run: Option<i64>,
    last_run: Option<i64>,
    #[serde(default)]
    skills: Vec<String>,
    script: Option<String>,
    #[serde(default)]
    isolated: bool,
}

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

fn key(id: u64) -> String {
    format!("task:{id:08}")
}

async fn tasks(august: &August) -> Result<Vec<Task>> {
    let all = august.list("task:").await?;
    Ok(all.into_iter().filter_map(|(_, v)| serde_json::from_value(v).ok()).collect())
}

async fn save(august: &August, task: &Task) -> Result<()> {
    august.set(&key(task.id), serde_json::to_value(task)?).await
}

/// The thread a tool call manages tasks of; refused outside the user's own turns.
fn thread_of(ctx: &Ctx) -> Result<Thread> {
    if ctx.turn.as_ref().is_some_and(|t| t.mode != "visible") {
        bail!("tasks can't be managed from a scheduled task or a subtask");
    }
    ctx.thread.clone().ok_or_else(|| anyhow::anyhow!("tasks belong to a chat; this call has none"))
}

fn format_tasks(tasks: &[Task]) -> String {
    if tasks.is_empty() {
        return "no scheduled tasks".into();
    }
    let line = |t: &Task| {
        let next = t.next_run.map(fmt_time).unwrap_or_else(|| "finished".into());
        let last = t.last_run.map(|l| format!(", last: {}", fmt_time(l))).unwrap_or_default();
        let mut extra = String::new();
        if !t.skills.is_empty() {
            extra += &format!(" [skills: {}]", t.skills.join(", "));
        }
        if let Some(s) = &t.script {
            extra += &format!(" [script: {s}]");
        }
        if t.isolated {
            extra += " [isolated]";
        }
        format!("#{} [{}] next: {}{} — {}{extra}", t.id, t.schedule, next, last, t.prompt)
    };
    tasks.iter().map(line).collect::<Vec<_>>().join("\n")
}

async fn schedule_task(august: &August, input: Value, ctx: Ctx) -> Result<String> {
    let thread = thread_of(&ctx)?;
    let (spec, prompt) = (str_arg(&input, "schedule").trim(), str_arg(&input, "prompt").trim());
    if prompt.is_empty() {
        bail!("empty prompt");
    }
    let spec = Schedule::normalize(spec, now())?;
    let next = Schedule::parse(&spec)?.next_after(now()).ok_or_else(|| anyhow::anyhow!("that time is in the past"))?;
    let skills: Vec<String> = input["skills"].as_array().into_iter().flatten().filter_map(|v| v.as_str().map(String::from)).collect();
    for name in &skills {
        if ctx.call_tool("load_skill", json!({"name": name})).await?.1 {
            bail!("no skill named `{name}`");
        }
    }
    let script = input["script"].as_str().map(str::trim).filter(|s| !s.is_empty()).map(String::from);
    if let Some(cmd) = &script
        && !ctx.approve(&format!("run before every run of the task `{spec}` (unattended):\n{cmd}")).await?
    {
        bail!("the user denied the script");
    }
    let id = {
        // Calls run concurrently; one at a time takes the next id.
        static NEXT_ID: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
        let _one = NEXT_ID.lock().await;
        let id = august.get("next_id").await?.and_then(|v| v.as_u64()).unwrap_or(1);
        august.set("next_id", json!(id + 1)).await?;
        id
    };
    let isolated = input["isolated"].as_bool().unwrap_or(false);
    let task = Task { id, thread, schedule: spec, prompt: prompt.into(), next_run: Some(next), last_run: None, skills, script, isolated };
    save(august, &task).await?;
    Ok(format!("scheduled #{id}, first run {}", fmt_time(next)))
}

/// Runs a task's script in the workspace; its output (or why there is none) for the prompt.
async fn run_script(august: &August, script: &str) -> String {
    match august_ext::sh(script, august.workspace(), SCRIPT_TIMEOUT).await {
        Ok(out) => {
            let mut s = String::from_utf8_lossy(&out.stdout).to_string();
            if !out.status.success() {
                s += &format!("\n[exit status {}] {}", out.status, String::from_utf8_lossy(&out.stderr));
            }
            truncate(s, 20_000)
        }
        Err(e) => format!("[the script did not finish: {e:#}]"),
    }
}

/// Fires a task as a turn in its thread and sends the reply unless it is `[SILENT]`.
async fn fire(august: August, task: Task) -> Result<()> {
    eprintln!("task #{} fires in {}", task.id, task.thread.key());
    let mut text = format!(
        "[Scheduled task #{} fired; your reply goes to the user, or reply exactly [SILENT] if \
         there is nothing worth telling them]\n{}",
        task.id, task.prompt
    );
    for name in &task.skills {
        match august.call_tool(&task.thread, "load_skill", json!({"name": name})).await? {
            (body, false) => text += &format!("\n\n[Skill `{name}`]\n{body}"),
            (why, true) => text += &format!("\n\n[Skill `{name}` could not be loaded: {why}]"),
        }
    }
    if let Some(script) = &task.script {
        text += &format!("\n\n[Output of the task's script `{script}`]\n{}", run_script(&august, script).await);
    }
    let mode = if task.isolated { "fresh" } else { "quiet" };
    let id = august.start_turn(&task.thread, json!({"text": text, "mode": mode, "source": "scheduler"})).await?;
    let out = august.wait_turn(id, Duration::from_secs(3600)).await?;
    let reply = out["reply"].as_str().unwrap_or_default().trim();
    match out["status"].as_str() {
        Some("ok") if reply.starts_with("[SILENT]") => {}
        Some("ok") => drop(august.send(&task.thread, if reply.is_empty() { "(empty reply)" } else { reply }, &[]).await?),
        Some("error") => {
            let why = out["error"].as_str().unwrap_or("unknown error");
            august.send(&task.thread, &format!("⚠️ Scheduled task #{} failed: {why}", task.id), &[]).await?;
        }
        _ => {}
    }
    Ok(())
}

/// Fires every due task once, moving its schedule forward first so a slow or failing run
/// can't fire twice.
async fn tick(august: &August) -> Result<()> {
    let now = now();
    for mut task in tasks(august).await? {
        if task.next_run.is_none_or(|t| t > now) {
            continue;
        }
        task.last_run = Some(now);
        task.next_run = Schedule::parse(&task.schedule).ok().and_then(|s| s.next_after(now));
        save(august, &task).await?;
        let august = august.clone();
        tokio::spawn(async move {
            let id = task.id;
            if let Err(e) = fire(august, task).await {
                eprintln!("task #{id}: {e:#}");
            }
        });
    }
    Ok(())
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let august = August::new();
    august.needs(&["turns", "messaging", "tools"]);
    august.register_prompt_section("tasks", SECTION);

    let me = august.clone();
    august.register_tool(
        "schedule_task",
        "Schedule something to happen later or repeatedly in this chat: reminders, daily \
         briefings, periodic checks. When it fires, you receive `prompt` as a new message and \
         act on it (with all tools), and your reply is sent to the user (a reply of just \
         `[SILENT]` sends nothing, for checks that only sometimes matter). Schedule formats \
         (local time): `every 30m` / `every 2h` / `every 1d`; `at 2026-10-06 09:00` or `in 20m` \
         (once); or 5 cron fields `min hour day month weekday`, e.g. `0 9 * * 1-5`. Write \
         `prompt` as a self-contained instruction to your future self, e.g. \"Remind the user to \
         call the dentist\". Optional: `skills` whose instructions come with the prompt, a \
         shell `script` run first in the workspace whose output comes with the prompt (e.g. to \
         fetch data or detect changes; needs the user's approval), and `isolated` to run in a \
         fresh conversation each time instead of this chat's history.",
        json!({
            "type": "object",
            "properties": {
                "schedule": {"type": "string", "description": "every …, at …, in …, or a cron expression"},
                "prompt": {"type": "string", "description": "What to do when it fires"},
                "skills": {"type": "array", "items": {"type": "string"}, "description": "Skills to load for the task"},
                "script": {"type": "string", "description": "Shell command to run first; its output is given to you"},
                "isolated": {"type": "boolean", "description": "Fresh conversation per run (default: this chat's)"}
            },
            "required": ["schedule", "prompt"],
            "additionalProperties": false
        }),
        move |input, ctx| {
            let august = me.clone();
            async move { schedule_task(&august, input, ctx).await }
        },
    );

    let me = august.clone();
    august.register_tool(
        "list_tasks",
        "List the scheduled tasks of this chat.",
        json!({"type": "object", "properties": {}, "additionalProperties": false}),
        move |_, ctx| {
            let august = me.clone();
            async move {
                let thread = thread_of(&ctx)?;
                let mine: Vec<Task> = tasks(&august).await?.into_iter().filter(|t| t.thread == thread).collect();
                Ok(format_tasks(&mine))
            }
        },
    );

    let me = august.clone();
    august.register_tool(
        "cancel_task",
        "Delete a scheduled task of this chat by its #id.",
        json!({"type": "object", "properties": {"id": {"type": "integer"}}, "required": ["id"], "additionalProperties": false}),
        move |input, ctx| {
            let august = me.clone();
            async move {
                let thread = thread_of(&ctx)?;
                let id = input["id"].as_u64().ok_or_else(|| anyhow::anyhow!("missing integer argument `id`"))?;
                let found = august.get(&key(id)).await?.and_then(|v| serde_json::from_value::<Task>(v).ok());
                Ok(match found {
                    Some(t) if t.thread == thread => {
                        august.set(&key(id), Value::Null).await?;
                        format!("cancelled #{id}")
                    }
                    _ => format!("no task #{id} in this chat"),
                })
            }
        },
    );

    let me = august.clone();
    august.register_command("tasks", "List scheduled tasks", move |_, ctx| {
        let august = me.clone();
        async move {
            let thread = ctx.thread.clone();
            let mine: Vec<Task> = tasks(&august).await?.into_iter().filter(|t| Some(&t.thread) == thread.as_ref()).collect();
            Ok(Some(format_tasks(&mine)))
        }
    });

    let ticker = august.clone();
    tokio::spawn(async move {
        let every = std::env::var("AUGUST_SCHEDULER_TICK").ok().and_then(|v| v.parse().ok()).map(Duration::from_secs).unwrap_or(TICK);
        loop {
            if let Err(e) = tick(&ticker).await {
                eprintln!("scheduler: {e:#}");
            }
            tokio::time::sleep(every).await;
        }
    });
    august.run().await;
}
