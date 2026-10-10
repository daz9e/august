//! `extend`: the agent writes extensions for August itself (`save_extension`); the core
//! starts it and marks it as the agent's. `extensions` lists them and turns them on and off.
//! It brings the TypeScript SDK (`extensions/sdk-ts`): the host that runs an `index.ts` under
//! bun, its types, and the guide, which ships as the `writing-extensions` skill.

#[cfg(test)]
mod tests;

use anyhow::bail;
use august_ext::{August, str_arg};
use serde_json::json;
use std::path::{Path, PathBuf};

const HOST: &str = include_str!("../sdk-ts/host.ts");
const TYPES: &str = include_str!("../sdk-ts/august.d.ts");
const GUIDE: &str = include_str!("../sdk-ts/guide.md");

fn valid_name(name: &str) -> bool {
    !name.is_empty() && name.len() <= 64 && name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    serve(August::new()).await;
}

async fn serve(august: August) {
    august.needs(&["admin"]);
    let dir = PathBuf::from(august.env("AUGUST_EXTENSIONS").unwrap_or_default());
    let home = dir.display().to_string();
    let skills = dir.join(".runtime/skills/writing-extensions");
    let host = dir.join(".runtime/ts/host.ts");
    if let Err(e) = write_sdk(&host, &dir, &skills) {
        eprintln!("could not write the TypeScript SDK: {e:#}");
    }
    let me = august.clone();
    august.register_tool(
        "save_extension",
        "Create or replace an extension that adds tools, slash commands or hooks to August \
         itself. Load the `writing-extensions` skill first for the API. TypeScript: `code` (its \
         index.ts). Any other language: `files` by path, with `extension.json` ({command, env, \
         setup}) — `setup` steps build it or install what it needs, August runs them; don't \
         build by hand. The extension is set up and started right away; the result is what it \
         registered, or the error and its log to fix. It must describe itself (summary).",
        json!({
            "type": "object",
            "properties": {
                "name": {"type": "string", "description": "lowercase-with-dashes"},
                "code": {"type": "string", "description": "TypeScript: contents of index.ts"},
                "files": {"type": "object", "additionalProperties": {"type": "string"},
                          "description": "Any language: file contents by path inside the extension's folder, including extension.json"}
            },
            "required": ["name"],
            "additionalProperties": false
        }),
        move |input, _| {
            let (august, folder, host) = (me.clone(), dir.join(str_arg(&input, "name")), host.clone());
            async move {
                let name = str_arg(&input, "name");
                if !valid_name(name) {
                    bail!("extension names use lowercase letters, digits, `-` and `_` (max 64)");
                }
                let files: Vec<(String, String)> = match (input["code"].as_str(), input["files"].as_object()) {
                    (Some(code), None) => {
                        let command = json!({"command": ["bun", "run", host.display().to_string(), "index.ts", name]});
                        vec![("index.ts".into(), code.into()), ("extension.json".into(), serde_json::to_string_pretty(&command)?)]
                    }
                    (None, Some(files)) => files.iter().map(|(p, c)| (p.clone(), c.as_str().unwrap_or_default().to_string())).collect(),
                    _ => bail!("give either `code` (TypeScript) or `files` (any language, with extension.json)"),
                };
                if !files.iter().any(|(p, _)| p == "extension.json") {
                    bail!("`files` needs an `extension.json` with the `command` that runs it");
                }
                for (path, _) in &files {
                    let inside = std::path::Path::new(path).components().all(|c| matches!(c, std::path::Component::Normal(_)));
                    if path.is_empty() || !inside {
                        bail!("`{path}`: paths are relative and stay inside the extension's folder");
                    }
                }
                std::fs::create_dir_all(&folder)?;
                for (path, content) in &files {
                    let file = folder.join(path);
                    std::fs::create_dir_all(file.parent().unwrap_or(&folder))?;
                    std::fs::write(&file, content)?;
                }
                let started = august.call("extension_enable", json!({"name": name})).await;
                let status = match started {
                    Ok(status) => status,
                    Err(e) => {
                        let log = august.call("extension_logs", json!({"name": name, "lines": 30})).await.unwrap_or_default();
                        let path = PathBuf::from(august.env("AUGUST_HOME").unwrap_or_default()).join(format!("logs/extensions/{name}.log"));
                        bail!("saved, but it failed to start:\n{e:#}\n\nIts log ({}):\n{}", path.display(), log.as_str().unwrap_or_default());
                    }
                };
                let list = august.call("extensions", json!({})).await?;
                let undescribed = list.as_array().into_iter().flatten().any(|e| e["name"] == name && e["summary"].as_str().unwrap_or_default().is_empty());
                if undescribed {
                    august.call("extension_disable", json!({"name": name})).await?;
                    bail!("saved, but it doesn't describe itself, so it was turned off: give it a summary (`august.describe(summary, details)`, or `summary` in its ready message) and save again");
                }
                let saved: Vec<&str> = files.iter().map(|(p, _)| p.as_str()).collect();
                Ok(format!("saved {} in {} and started it.\n{}", saved.join(", "), folder.display(), status.as_str().unwrap_or_default()))
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
    august.run().await;
}

/// Writes the TypeScript host and its types at `host`, and the guide as the
/// `writing-extensions` skill in `skills`.
fn write_sdk(host: &Path, dir: &Path, skills: &Path) -> anyhow::Result<()> {
    let rt = host.parent().unwrap_or(dir);
    std::fs::create_dir_all(rt)?;
    std::fs::write(host, HOST)?;
    std::fs::write(rt.join("august.d.ts"), TYPES)?;
    std::fs::create_dir_all(skills)?;
    let about = "How to extend August itself with extensions in TypeScript or any language (tools, slash commands, hooks); read before `save_extension`";
    let guide = GUIDE.replace("{host}", &host.display().to_string());
    let text = format!("---\nname: writing-extensions\ndescription: {about}\n---\nExtensions live in {}.\n\n{guide}\n```ts\n{TYPES}```\n", dir.display());
    std::fs::write(skills.join("SKILL.md"), text)?;
    Ok(())
}
