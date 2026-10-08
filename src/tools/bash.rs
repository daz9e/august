use super::*;
use serde_json::json;
use std::time::Duration;

const TIMEOUT: Duration = Duration::from_secs(120);
const MAX_OUTPUT: usize = 30_000;

/// Read-only commands that run without asking, if the command has no shell metacharacters.
const SAFE_COMMANDS: &[&str] = &[
    "ls", "pwd", "cat", "head", "tail", "wc", "echo", "grep", "rg", "date", "whoami", "uname",
    "which", "file", "stat", "du", "df", "tree",
];

fn is_safe(cmd: &str) -> bool {
    let has_meta = cmd
        .chars()
        .any(|c| matches!(c, ';' | '&' | '|' | '>' | '<' | '`' | '$' | '\n' | '(' | ')'));
    let first = cmd.split_whitespace().next().unwrap_or("");
    !has_meta && SAFE_COMMANDS.contains(&first)
}

pub struct Bash;

#[async_trait]
impl Tool for Bash {
    fn name(&self) -> &'static str {
        "bash"
    }

    fn description(&self) -> &'static str {
        "Run a bash command in the workspace directory. Returns exit code, stdout and \
         stderr. Times out after 120s. Non-read-only commands require the user's approval."
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
        if !is_safe(cmd) && !ctx.approver.approve(&format!("bash: {cmd}")).await {
            anyhow::bail!("the user denied running this command");
        }
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

#[cfg(test)]
mod tests {
    use super::is_safe;

    #[test]
    fn safe_detection() {
        assert!(is_safe("ls -la"));
        assert!(is_safe("cat notes.md"));
        assert!(!is_safe("rm -rf /"));
        assert!(!is_safe("ls; rm x"));
        assert!(!is_safe("cat a > b"));
        assert!(!is_safe("echo $(whoami)"));
    }
}
