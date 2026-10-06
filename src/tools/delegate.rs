use super::*;
use serde_json::json;

pub struct DelegateTask;

#[async_trait]
impl Tool for DelegateTask {
    fn name(&self) -> &'static str {
        "delegate_task"
    }

    fn description(&self) -> &'static str {
        "Hand a self-contained piece of work to a sub-agent that runs in the background with a \
         fresh context and the same tools (research, a long build or investigation, several \
         independent parts at once — call it once per part). The sub-agent sees nothing of \
         this conversation: put everything it needs into `goal` and `context` (paths, names, \
         constraints, what a good result looks like). Its report arrives later as a new \
         message; meanwhile you can keep talking to the user."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "goal": {"type": "string", "description": "What to achieve, in one or two sentences"},
                "context": {"type": "string", "description": "Everything the sub-agent needs to know"}
            },
            "required": ["goal"],
            "additionalProperties": false
        })
    }

    async fn call(&self, input: &Value, ctx: &ToolCtx) -> Result<String> {
        let goal = str_arg(input, "goal")?.trim();
        if goal.is_empty() {
            anyhow::bail!("empty goal");
        }
        let Some(d) = ctx.delegate.as_ref().filter(|_| !ctx.unattended) else {
            anyhow::bail!("subtasks can't be started here");
        };
        d.delegate(goal, input["context"].as_str().unwrap_or("")).await
    }
}
