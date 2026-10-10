//! The agent's hands in the workspace: `bash`, `read`, `write` and `edit`. Paths are
//! relative to the workspace and may not leave it.

use anyhow::{Result, bail};
use august_ext::{August, sh, str_arg, truncate};
use serde_json::{Value, json};
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

const BASH_TIMEOUT: Duration = Duration::from_secs(120);
const MAX_OUTPUT: usize = 30_000;
const MAX_READ: usize = 100_000;

/// Resolves `p` against the workspace and rejects anything that escapes it
/// (`..`, absolute paths elsewhere, symlinks pointing outside).
fn resolve(workspace: &Path, p: &str) -> Result<PathBuf> {
    if p.is_empty() {
        bail!("missing string argument `path`");
    }
    let joined = workspace.join(p);
    let mut normal = PathBuf::new();
    for c in joined.components() {
        match c {
            Component::ParentDir => {
                normal.pop();
            }
            Component::CurDir => {}
            other => normal.push(other),
        }
    }
    let inside = |path: &Path| path.starts_with(workspace);
    if !inside(&normal) {
        bail!("path is outside the workspace: {p}");
    }
    // Follow symlinks through the deepest existing ancestor.
    let mut existing = normal.as_path();
    while !existing.exists() {
        existing = existing.parent().unwrap_or(workspace);
    }
    if !inside(&existing.canonicalize()?) {
        bail!("path resolves outside the workspace: {p}");
    }
    Ok(normal)
}

fn replace(text: &str, old: &str, new: &str, all: bool) -> Result<String> {
    if old.is_empty() {
        bail!("old_string is empty");
    }
    if old == new {
        bail!("old_string and new_string are identical");
    }
    match text.matches(old).count() {
        0 => bail!("old_string not found (it must match exactly, including whitespace)"),
        1 => Ok(text.replacen(old, new, 1)),
        _ if all => Ok(text.replace(old, new)),
        n => bail!("old_string matches {n} places; add more context or set replace_all"),
    }
}

fn path_schema(with_content: bool) -> Value {
    let mut schema = json!({
        "type": "object",
        "properties": {"path": {"type": "string", "description": "Path relative to the workspace"}},
        "required": ["path"],
        "additionalProperties": false
    });
    if with_content {
        schema["properties"]["content"] = json!({"type": "string", "description": "Full file content"});
        schema["required"] = json!(["path", "content"]);
    }
    schema
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let august = August::new();
    let ws = august.workspace().canonicalize().unwrap_or_else(|_| august.workspace().clone());

    let dir = ws.clone();
    august.register_tool(
        "bash",
        "Run a bash command in the workspace directory. Returns exit code, stdout and stderr. \
         Times out after 120s.",
        json!({
            "type": "object",
            "properties": {"command": {"type": "string", "description": "The command to run"}},
            "required": ["command"],
            "additionalProperties": false
        }),
        move |input, _| {
            let dir = dir.clone();
            async move {
                let cmd = str_arg(&input, "command");
                if cmd.is_empty() {
                    bail!("missing string argument `command`");
                }
                let out = sh(cmd, &dir, BASH_TIMEOUT).await?;
                let text = format!(
                    "exit code: {}\nstdout:\n{}\nstderr:\n{}",
                    out.status.code().map_or("killed".into(), |c| c.to_string()),
                    String::from_utf8_lossy(&out.stdout),
                    String::from_utf8_lossy(&out.stderr),
                );
                Ok(truncate(text, MAX_OUTPUT))
            }
        },
    );

    let dir = ws.clone();
    august.register_tool("read", "Read a UTF-8 text file from the workspace.", path_schema(false), move |input, _| {
        let dir = dir.clone();
        async move {
            let path = resolve(&dir, str_arg(&input, "path"))?;
            Ok(truncate(tokio::fs::read_to_string(&path).await?, MAX_READ))
        }
    });

    let dir = ws.clone();
    august.register_tool(
        "write",
        "Create or overwrite a text file in the workspace. Parent directories are created.",
        path_schema(true),
        move |input, _| {
            let dir = dir.clone();
            async move {
                let path = resolve(&dir, str_arg(&input, "path"))?;
                let content = input["content"].as_str().ok_or_else(|| anyhow::anyhow!("missing string argument `content`"))?;
                if let Some(parent) = path.parent() {
                    tokio::fs::create_dir_all(parent).await?;
                }
                tokio::fs::write(&path, content).await?;
                Ok(format!("wrote {} bytes to {}", content.len(), path.display()))
            }
        },
    );

    let dir = ws;
    august.register_tool(
        "edit",
        "Edit a workspace file by replacing `old_string` with `new_string`. `old_string` must \
         match exactly once (include surrounding lines to make it unique) unless `replace_all` \
         is true. Prefer this over `write` for changes to existing files.",
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "Path relative to the workspace"},
                "old_string": {"type": "string", "description": "Exact text to replace"},
                "new_string": {"type": "string", "description": "Replacement text"},
                "replace_all": {"type": "boolean", "description": "Replace every occurrence (default false)"}
            },
            "required": ["path", "old_string", "new_string"],
            "additionalProperties": false
        }),
        move |input, _| {
            let dir = dir.clone();
            async move {
                let path = resolve(&dir, str_arg(&input, "path"))?;
                let all = input["replace_all"].as_bool().unwrap_or(false);
                let text = tokio::fs::read_to_string(&path).await?;
                let updated = replace(&text, str_arg(&input, "old_string"), str_arg(&input, "new_string"), all)?;
                tokio::fs::write(&path, updated).await?;
                Ok(format!("edited {}", path.display()))
            }
        },
    );
    august.run().await;
}
