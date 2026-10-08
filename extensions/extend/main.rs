//! `extend`: the agent writes extensions for August itself (`save_extension`); the core
//! starts it and marks it as the agent's.

use anyhow::{anyhow, bail};
use august_ext::{August, str_arg};
use serde_json::json;
use std::path::PathBuf;

fn valid_name(name: &str) -> bool {
    !name.is_empty() && name.len() <= 64 && name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let august = August::new();
    august.needs(&["admin"]);
    let dir = PathBuf::from(std::env::var("AUGUST_EXTENSIONS").unwrap_or_default());
    let me = august.clone();
    august.register_tool(
        "save_extension",
        "Create or replace an extension: TypeScript that adds tools, slash commands or hooks \
         to August itself. Load the `writing-extensions` skill first for the API. The \
         extension is started right away; the result lists what it registered or the error \
         to fix.",
        json!({
            "type": "object",
            "properties": {
                "name": {"type": "string", "description": "lowercase-with-dashes"},
                "code": {"type": "string", "description": "Contents of index.ts"}
            },
            "required": ["name", "code"],
            "additionalProperties": false
        }),
        move |input, _| {
            let (august, folder) = (me.clone(), dir.join(str_arg(&input, "name")));
            async move {
                let (name, code) = (str_arg(&input, "name"), str_arg(&input, "code"));
                if !valid_name(name) {
                    bail!("extension names use lowercase letters, digits, `-` and `_` (max 64)");
                }
                std::fs::create_dir_all(&folder)?;
                let file = folder.join("index.ts");
                std::fs::write(&file, code)?;
                let status = august.call("extension_enable", json!({"name": name})).await.map_err(|e| anyhow!("saved, but it failed to start:\n{e:#}"))?;
                Ok(format!("saved {} and started it.\n{}", file.display(), status.as_str().unwrap_or_default()))
            }
        },
    );
    august.run().await;
}
