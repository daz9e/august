use super::*;

pub struct SaveExtension;

#[async_trait]
impl Tool for SaveExtension {
    fn name(&self) -> &'static str {
        "save_extension"
    }

    fn description(&self) -> &'static str {
        "Create or replace an extension: TypeScript that adds tools, slash commands or hooks \
         to August itself. Load the `writing-extensions` skill first for the API. The \
         extension is started right away; the result lists what it registered or the error \
         to fix."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "name": {"type": "string", "description": "lowercase-with-dashes"},
                "code": {"type": "string", "description": "Contents of index.ts"}
            },
            "required": ["name", "code"],
            "additionalProperties": false
        })
    }

    async fn call(&self, input: &Value, ctx: &ToolCtx) -> Result<String> {
        let (name, code) = (str_arg(input, "name")?, str_arg(input, "code")?);
        let Some(ext) = &ctx.extensions else {
            anyhow::bail!("extensions are not available here");
        };
        if !crate::extensions::valid_name(name) {
            anyhow::bail!("extension names use lowercase letters, digits, `-` and `_` (max 64)");
        }
        let folder = ext.dir().join(name);
        std::fs::create_dir_all(&folder)?;
        std::fs::write(folder.join("index.ts"), code)?;
        // Written by the agent: the core says so, the extension can't.
        crate::config::set(&format!("extensions.{name}.origin"), json!("agent"))?;
        let status = ext.load(name).await.map_err(|e| anyhow::anyhow!("saved, but it failed to start:\n{e:#}"))?;
        Ok(format!("saved {} and started it.\n{status}", folder.join("index.ts").display()))
    }
}
