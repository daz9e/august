//! `extend`: the agent writes extensions for August itself (`save_extension`); the core
//! starts it and marks it as the agent's. `extensions` lists them and turns them on and off.
//! The core's extension guide ships as the `writing-extensions` skill.

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
    let home = dir.display().to_string();
    let skills = dir.join(".runtime/skills/writing-extensions");
    let me = august.clone();
    august.register_tool(
        "save_extension",
        "Create or replace an extension: TypeScript that adds tools, slash commands or hooks \
         to August itself. Load the `writing-extensions` skill first for the API. The \
         extension is started right away; the result lists what it registered or the error \
         to fix. It must call `august.describe(summary, details)`.",
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
                let list = august.call("extensions", json!({})).await?;
                let undescribed = list.as_array().into_iter().flatten().any(|e| e["name"] == name && e["summary"].as_str().unwrap_or_default().is_empty());
                if undescribed {
                    august.call("extension_disable", json!({"name": name})).await?;
                    bail!("saved, but it doesn't describe itself, so it was turned off: call `august.describe(summary, details)` in setup and save again");
                }
                Ok(format!("saved {} and started it.\n{}", file.display(), status.as_str().unwrap_or_default()))
            }
        },
    );
    let me = august.clone();
    august.register_tool(
        "extensions",
        "List August's extensions with their state and what they register, or enable or \
         disable one by name (disabled stays off across restarts).",
        json!({
            "type": "object",
            "properties": {
                "action": {"type": "string", "enum": ["list", "enable", "disable"]},
                "name": {"type": "string", "description": "The extension, for enable and disable"}
            },
            "required": ["action"],
            "additionalProperties": false
        }),
        move |input, _| {
            let august = me.clone();
            async move {
                let (action, name) = (str_arg(&input, "action"), str_arg(&input, "name"));
                match action {
                    "list" => {}
                    "enable" | "disable" if !name.is_empty() => {
                        august.call(&format!("extension_{action}"), json!({"name": name})).await?;
                    }
                    "enable" | "disable" => bail!("`{action}` needs a `name`"),
                    _ => bail!("unknown action `{action}`"),
                }
                Ok(serde_json::to_string(&august.call("extensions", json!({})).await?)?)
            }
        },
    );
    // The model is told which extensions of the user's are running, each turn, so it
    // knows its behavior may come from them (and from one it saved a minute ago).
    let me = august.clone();
    august.on("before_turn", move |data, _| {
        let (august, home) = (me.clone(), home.clone());
        async move {
            let list = august.call("extensions", json!({})).await?;
            let lines: Vec<String> = list
                .as_array()
                .into_iter()
                .flatten()
                .filter(|e| e["state"] == "running" && e["origin"] != "default")
                .map(|e| {
                    let name = e["name"].as_str().unwrap_or_default();
                    if let Some(summary) = e["summary"].as_str().filter(|s| !s.is_empty()) {
                        return format!("- {name}: {summary}");
                    }
                    let parts: Vec<String> = [("tools", ""), ("commands", "/"), ("hooks", "")]
                        .iter()
                        .filter_map(|(key, prefix)| {
                            let names: Vec<String> = e[key].as_array()?.iter().filter_map(|n| Some(format!("{prefix}{}", n.as_str()?))).collect();
                            (!names.is_empty()).then(|| format!("{key}: {}", names.join(", ")))
                        })
                        .collect();
                    format!("- {name} ({})", parts.join("; "))
                })
                .collect();
            if lines.is_empty() {
                return Ok(None);
            }
            let system = format!(
                "{}\n\nExtensions the user or you installed are running and change how you and this chat \
                 behave. When something happens that you didn't do (a message, a new session, a blocked \
                 tool), check them first (`extensions` lists what each does in detail, their code is in {}), not August's source:\n{}",
                data["system"].as_str().unwrap_or_default(),
                home,
                lines.join("\n")
            );
            Ok(Some(json!({"system": system})))
        }
    });
    // The guide comes from the core, so it waits for the connection `run` serves.
    let me = august.clone();
    tokio::spawn(async move {
        let write = async {
            let guide = me.call("guide", json!({})).await?;
            std::fs::create_dir_all(&skills)?;
            let about = "How to extend August itself with TypeScript extensions (tools, slash commands, hooks); read before `save_extension`";
            let text = format!("---\nname: writing-extensions\ndescription: {about}\n---\n{}", guide.as_str().unwrap_or_default());
            std::fs::write(skills.join("SKILL.md"), text)?;
            anyhow::Ok(())
        };
        if let Err(e) = write.await {
            eprintln!("could not write the writing-extensions skill: {e:#}");
        }
    });
    august.run().await;
}
