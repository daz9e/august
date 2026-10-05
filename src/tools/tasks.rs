use super::*;
use crate::scheduler::schedule::{Schedule, fmt_time};
use serde_json::json;

fn origin(ctx: &ToolCtx) -> Result<(&str, &str)> {
    ctx.origin
        .as_ref()
        .map(|(c, h)| (c.as_str(), h.as_str()))
        .ok_or_else(|| anyhow::anyhow!("scheduled tasks are only available in messenger chats (run `august serve`)"))
}

pub struct ScheduleTask;

#[async_trait]
impl Tool for ScheduleTask {
    fn name(&self) -> &'static str {
        "schedule_task"
    }

    fn description(&self) -> &'static str {
        "Schedule something to happen later or repeatedly in this chat: reminders, daily \
         briefings, periodic checks. When it fires, you receive `prompt` as a new message and \
         act on it (with all tools), and your reply is sent to the user. Schedule formats \
         (local time): `every 30m` / `every 2h` / `every 1d`; `at 2026-10-06 09:00` (once); or \
         5 cron fields `min hour day month weekday`, e.g. `0 9 * * 1-5`. Write `prompt` as a \
         self-contained instruction to your future self, e.g. \"Remind the user to call the \
         dentist\"."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "schedule": {"type": "string", "description": "every …, at …, or a cron expression"},
                "prompt": {"type": "string", "description": "What to do when it fires"}
            },
            "required": ["schedule", "prompt"],
            "additionalProperties": false
        })
    }

    async fn call(&self, input: &Value, ctx: &ToolCtx) -> Result<String> {
        let (channel, chat) = origin(ctx)?;
        let (spec, prompt) = (str_arg(input, "schedule")?.trim(), str_arg(input, "prompt")?.trim());
        if prompt.is_empty() {
            anyhow::bail!("empty prompt");
        }
        let next = Schedule::parse(spec)?
            .next_after(crate::db::now())
            .ok_or_else(|| anyhow::anyhow!("that time is in the past"))?;
        let id = ctx.db.add_task(channel, chat, spec, prompt, next)?;
        Ok(format!("scheduled #{id}, first run {}", fmt_time(next)))
    }
}

pub struct ListTasks;

#[async_trait]
impl Tool for ListTasks {
    fn name(&self) -> &'static str {
        "list_tasks"
    }

    fn description(&self) -> &'static str {
        "List the scheduled tasks of this chat."
    }

    fn input_schema(&self) -> Value {
        json!({"type": "object", "properties": {}, "additionalProperties": false})
    }

    async fn call(&self, _input: &Value, ctx: &ToolCtx) -> Result<String> {
        let tasks = ctx.db.tasks(Some(origin(ctx)?))?;
        Ok(format_tasks(&tasks))
    }
}

pub fn format_tasks(tasks: &[crate::db::Task]) -> String {
    if tasks.is_empty() {
        return "no scheduled tasks".into();
    }
    tasks
        .iter()
        .map(|t| {
            let next = t.next_run.map(fmt_time).unwrap_or_else(|| "finished".into());
            let last = t.last_run.map(|l| format!(", last: {}", fmt_time(l))).unwrap_or_default();
            format!("#{} [{}] next: {}{} — {}", t.id, t.schedule, next, last, t.prompt)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

pub struct CancelTask;

#[async_trait]
impl Tool for CancelTask {
    fn name(&self) -> &'static str {
        "cancel_task"
    }

    fn description(&self) -> &'static str {
        "Delete a scheduled task of this chat by its #id."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {"id": {"type": "integer"}},
            "required": ["id"],
            "additionalProperties": false
        })
    }

    async fn call(&self, input: &Value, ctx: &ToolCtx) -> Result<String> {
        let id = input["id"].as_i64().ok_or_else(|| anyhow::anyhow!("missing integer argument `id`"))?;
        Ok(if ctx.db.delete_task(id, Some(origin(ctx)?))? { format!("cancelled #{id}") } else { format!("no task #{id} in this chat") })
    }
}
