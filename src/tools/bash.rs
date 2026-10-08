use super::*;
use serde_json::json;
use std::time::Duration;

const TIMEOUT: Duration = Duration::from_secs(120);
const MAX_OUTPUT: usize = 30_000;

pub struct Bash;

#[async_trait]
impl Tool for Bash {
    fn name(&self) -> &'static str {
        "bash"
    }

    fn description(&self) -> &'static str {
        "Run a bash command in the workspace directory. Returns exit code, stdout and \
         stderr. Times out after 120s."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "command": {"type": "string", "description": "The command to run"}
            },
            "required": ["command"],
            "additionalProperties": false
        })
    }

    async fn call(&self, input: &Value, ctx: &ToolCtx) -> Result<String> {
        let cmd = str_arg(input, "command")?;
        let out = august_ext::sh(cmd, &ctx.workspace, TIMEOUT).await?;
        let text = format!(
            "exit code: {}\nstdout:\n{}\nstderr:\n{}",
            out.status.code().map_or("killed".into(), |c| c.to_string()),
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr),
        );
        Ok(truncate(text, MAX_OUTPUT))
    }
}
