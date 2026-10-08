use super::*;
use serde_json::json;
use std::path::{Component, Path};

const MAX_READ: usize = 100_000;

/// Resolves `p` against the workspace and rejects anything that escapes it
/// (`..`, absolute paths elsewhere, symlinks pointing outside).
pub(crate) fn resolve(workspace: &Path, p: &str) -> Result<PathBuf> {
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
        anyhow::bail!("path is outside the workspace: {p}");
    }
    // Follow symlinks through the deepest existing ancestor.
    let mut existing = normal.as_path();
    while !existing.exists() {
        existing = existing.parent().unwrap_or(workspace);
    }
    if !inside(&existing.canonicalize()?) {
        anyhow::bail!("path resolves outside the workspace: {p}");
    }
    Ok(normal)
}

fn path_schema(with_content: bool) -> Value {
    let mut schema = json!({
        "type": "object",
        "properties": {
            "path": {"type": "string", "description": "Path relative to the workspace"}
        },
        "required": ["path"],
        "additionalProperties": false
    });
    if with_content {
        schema["properties"]["content"] =
            json!({"type": "string", "description": "Full file content"});
        schema["required"] = json!(["path", "content"]);
    }
    schema
}

pub struct ReadFile;

#[async_trait]
impl Tool for ReadFile {
    fn name(&self) -> &'static str {
        "read"
    }

    fn description(&self) -> &'static str {
        "Read a UTF-8 text file from the workspace."
    }

    fn input_schema(&self) -> Value {
        path_schema(false)
    }

    async fn call(&self, input: &Value, ctx: &ToolCtx) -> Result<String> {
        let path = resolve(&ctx.workspace, str_arg(input, "path")?)?;
        let text = tokio::fs::read_to_string(&path).await?;
        Ok(truncate(text, MAX_READ))
    }
}

pub struct WriteFile;

#[async_trait]
impl Tool for WriteFile {
    fn name(&self) -> &'static str {
        "write"
    }

    fn description(&self) -> &'static str {
        "Create or overwrite a text file in the workspace. Parent directories are created."
    }

    fn input_schema(&self) -> Value {
        path_schema(true)
    }

    async fn call(&self, input: &Value, ctx: &ToolCtx) -> Result<String> {
        let path = resolve(&ctx.workspace, str_arg(input, "path")?)?;
        let content = str_arg(input, "content")?;
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        tokio::fs::write(&path, content).await?;
        Ok(format!("wrote {} bytes to {}", content.len(), path.display()))
    }
}

pub struct EditFile;

#[async_trait]
impl Tool for EditFile {
    fn name(&self) -> &'static str {
        "edit"
    }

    fn description(&self) -> &'static str {
        "Edit a workspace file by replacing `old_string` with `new_string`. `old_string` must \
         match exactly once (include surrounding lines to make it unique) unless `replace_all` \
         is true. Prefer this over `write` for changes to existing files."
    }

    fn input_schema(&self) -> Value {
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
        })
    }

    async fn call(&self, input: &Value, ctx: &ToolCtx) -> Result<String> {
        let path = resolve(&ctx.workspace, str_arg(input, "path")?)?;
        let (old, new) = (str_arg(input, "old_string")?, str_arg(input, "new_string")?);
        let all = input["replace_all"].as_bool().unwrap_or(false);
        let text = tokio::fs::read_to_string(&path).await?;
        let updated = replace(&text, old, new, all)?;
        tokio::fs::write(&path, updated).await?;
        Ok(format!("edited {}", path.display()))
    }
}

fn replace(text: &str, old: &str, new: &str, all: bool) -> Result<String> {
    if old.is_empty() {
        anyhow::bail!("old_string is empty");
    }
    if old == new {
        anyhow::bail!("old_string and new_string are identical");
    }
    match text.matches(old).count() {
        0 => anyhow::bail!("old_string not found (it must match exactly, including whitespace)"),
        1 => Ok(text.replacen(old, new, 1)),
        _ if all => Ok(text.replace(old, new)),
        n => anyhow::bail!("old_string matches {n} places; add more context or set replace_all"),
    }
}

#[cfg(test)]
mod tests {
    use super::{replace, resolve};

    #[test]
    fn edit_requires_a_unique_match() {
        assert_eq!(replace("a b a", "b", "c", false).unwrap(), "a c a");
        assert!(replace("a b a", "a", "c", false).is_err());
        assert_eq!(replace("a b a", "a", "c", true).unwrap(), "c b c");
        assert!(replace("a", "x", "y", false).is_err());
        assert!(replace("a", "", "y", false).is_err());
    }

    #[test]
    fn rejects_escapes() {
        let ws = std::env::temp_dir().canonicalize().unwrap();
        assert!(resolve(&ws, "a/b.txt").is_ok());
        assert!(resolve(&ws, "a/../b.txt").is_ok());
        assert!(resolve(&ws, "../etc/passwd").is_err());
        assert!(resolve(&ws, "/etc/passwd").is_err());
    }
}
