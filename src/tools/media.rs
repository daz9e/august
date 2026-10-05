use super::*;
use serde_json::json;

pub struct SendFile;

#[async_trait]
impl Tool for SendFile {
    fn name(&self) -> &'static str {
        "send_file"
    }

    fn description(&self) -> &'static str {
        "Send a file from the workspace to the user in the chat (images are shown inline). \
         Use it to deliver files you created or downloaded, not to show text you can write in the reply."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "Path relative to the workspace"},
                "caption": {"type": "string", "description": "Optional short caption"}
            },
            "required": ["path"],
            "additionalProperties": false
        })
    }

    async fn call(&self, input: &Value, ctx: &ToolCtx) -> Result<String> {
        let rel = str_arg(input, "path")?;
        let path = fs::resolve(&ctx.workspace, rel)?;
        if !path.is_file() {
            anyhow::bail!("not a file: {rel}");
        }
        let Some(sink) = &ctx.files else {
            anyhow::bail!("there is no chat to send files to; tell the user the path instead: {rel}");
        };
        sink.send_file(&path, input["caption"].as_str().unwrap_or("")).await?;
        Ok(format!("sent {rel} to the chat"))
    }
}
