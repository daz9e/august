use super::fs::resolve;
use super::*;
use ignore::WalkBuilder;
use serde_json::json;

const MAX_MATCHES: usize = 200;
const MAX_FILES: usize = 500;
const MAX_OUTPUT: usize = 30_000;

/// Walks `root` honouring .gitignore; results are workspace-relative paths.
fn walker(root: &std::path::Path) -> ignore::Walk {
    WalkBuilder::new(root).hidden(false).filter_entry(|e| e.file_name() != ".git").build()
}

pub struct Grep;

#[async_trait]
impl Tool for Grep {
    fn name(&self) -> &'static str {
        "grep"
    }

    fn description(&self) -> &'static str {
        "Search file contents in the workspace with a regular expression (Rust regex syntax). \
         Respects .gitignore. Returns `path:line: text` for each match."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": {"type": "string", "description": "Regular expression"},
                "path": {"type": "string", "description": "Directory or file to search (default: workspace root)"},
                "glob": {"type": "string", "description": "Only search files matching this glob, e.g. *.rs"},
                "ignore_case": {"type": "boolean"}
            },
            "required": ["pattern"],
            "additionalProperties": false
        })
    }

    async fn call(&self, input: &Value, ctx: &ToolCtx) -> Result<String> {
        let re = regex::RegexBuilder::new(str_arg(input, "pattern")?)
            .case_insensitive(input["ignore_case"].as_bool().unwrap_or(false))
            .build()?;
        let root = resolve(&ctx.workspace, input["path"].as_str().unwrap_or("."))?;
        let glob = input["glob"].as_str().map(glob_matcher).transpose()?;
        let ctx_ws = ctx.workspace.clone();
        let out = tokio::task::spawn_blocking(move || {
            let mut lines = Vec::new();
            'files: for entry in walker(&root).flatten() {
                if !entry.file_type().is_some_and(|t| t.is_file()) {
                    continue;
                }
                if glob.as_ref().is_some_and(|g| !g.is_match(entry.path().file_name().unwrap_or_default())) {
                    continue;
                }
                let Ok(text) = std::fs::read_to_string(entry.path()) else {
                    continue; // binary or unreadable
                };
                for (i, line) in text.lines().enumerate() {
                    if re.is_match(line) {
                        let shown: String = line.trim().chars().take(300).collect();
                        let p = entry.path().strip_prefix(&ctx_ws).unwrap_or(entry.path());
                        lines.push(format!("{}:{}: {shown}", p.display(), i + 1));
                        if lines.len() >= MAX_MATCHES {
                            lines.push(format!("... stopped at {MAX_MATCHES} matches"));
                            break 'files;
                        }
                    }
                }
            }
            lines
        })
        .await?;
        Ok(if out.is_empty() { "no matches".into() } else { truncate(out.join("\n"), MAX_OUTPUT) })
    }
}

fn glob_matcher(pattern: &str) -> Result<globset::GlobMatcher> {
    Ok(globset::Glob::new(pattern)?.compile_matcher())
}

pub struct Glob;

#[async_trait]
impl Tool for Glob {
    fn name(&self) -> &'static str {
        "glob"
    }

    fn description(&self) -> &'static str {
        "Find files in the workspace by glob pattern relative to the workspace, e.g. \
         `src/**/*.rs` or `*.md`. Respects .gitignore."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {"pattern": {"type": "string", "description": "Glob pattern"}},
            "required": ["pattern"],
            "additionalProperties": false
        })
    }

    async fn call(&self, input: &Value, ctx: &ToolCtx) -> Result<String> {
        let m = glob_matcher(str_arg(input, "pattern")?)?;
        let root = ctx.workspace.clone();
        let mut found: Vec<String> = tokio::task::spawn_blocking(move || {
            walker(&root)
                .flatten()
                .filter(|e| e.file_type().is_some_and(|t| t.is_file()))
                .filter_map(|e| {
                    let p = e.path().strip_prefix(&root).ok()?.to_path_buf();
                    m.is_match(&p).then(|| p.display().to_string())
                })
                .take(MAX_FILES + 1)
                .collect()
        })
        .await?;
        found.sort();
        let more = found.len() > MAX_FILES;
        found.truncate(MAX_FILES);
        if found.is_empty() {
            return Ok("no files".into());
        }
        Ok(format!("{}{}", found.join("\n"), if more { format!("\n... more than {MAX_FILES} files") } else { String::new() }))
    }
}
