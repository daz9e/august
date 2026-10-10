//! `config`: the agent reads and changes August's settings, its own (`august.*`) and every
//! extension's (`extensions.<name>.settings.*`), through one tool. `list` shows what can be
//! set, with descriptions, defaults and current values; secrets stay masked.

use anyhow::bail;
use august_ext::{August, str_arg};
use serde_json::{Value, json};

/// One line per setting: `path = value (default …) — description`.
fn lines(list: &Value) -> String {
    let rows: Vec<String> = list
        .as_array()
        .into_iter()
        .flatten()
        .map(|s| {
            let mut line = format!("{} = {}", str_arg(s, "path"), s["value"]);
            if !s["default"].is_null() {
                line += &format!(" (default {})", s["default"]);
            }
            if let Some(d) = s["description"].as_str().filter(|d| !d.is_empty()) {
                line += &format!(" — {d}");
            }
            line
        })
        .collect();
    if rows.is_empty() { "no settings there".into() } else { rows.join("\n") }
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    serve(August::new()).await
}

async fn serve(august: August) {
    august.needs(&["config"]);
    august.describe(
        "The `config` tool: list, read and change August's and extensions' settings",
        "Settings are addressed by dotted paths: `august.<key>` for August's own (provider, model, \
         effort, home) and `extensions.<name>.settings.<key>` for an extension's. `list` \
         shows the known ones under a prefix with descriptions, defaults and current values; `set` \
         with null deletes a value. Secrets are shown masked. Turning extensions on and off is the \
         `extensions` tool's job (the user's call).",
    );
    let me = august.clone();
    august.register_tool(
        "config",
        "August's settings by dotted path: `august.<key>` (its own: provider, model, effort, \
         home) and `extensions.<name>.settings.<key>`. `list` shows what exists under \
         `path` (a prefix; empty: everything) with descriptions, defaults and values; `get` one \
         value; `set` changes it (null deletes).",
        json!({
            "type": "object",
            "properties": {
                "action": {"type": "string", "enum": ["list", "get", "set"]},
                "path": {"type": "string", "description": "e.g. `august.model`, `extensions.web`, `extensions.web.settings.brave_key`"},
                "value": {"description": "For set: any JSON value; null deletes"}
            },
            "required": ["action"],
            "additionalProperties": false
        }),
        move |input, _| {
            let august = me.clone();
            async move {
                let path = str_arg(&input, "path");
                match str_arg(&input, "action") {
                    "list" => Ok(lines(&august.call("config_list", json!({"prefix": path})).await?)),
                    "get" | "set" if path.is_empty() => bail!("needs a `path`"),
                    "get" => Ok(serde_json::to_string_pretty(&august.call("config_get", json!({"path": path})).await?)?),
                    "set" => {
                        august.call("config_set", json!({"path": path, "value": input["value"]})).await?;
                        let now = august.call("config_get", json!({"path": path})).await?;
                        Ok(format!("{path} = {now}"))
                    }
                    other => bail!("unknown action `{other}`"),
                }
            }
        },
    );
    august.run().await;
}

#[cfg(test)]
mod tests;
