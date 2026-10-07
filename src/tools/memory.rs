use super::*;
use serde_json::json;

/// Total characters all facts may take in the system prompt (override: `AUGUST_MEMORY_CHARS`).
const DEFAULT_MEMORY_CHARS: usize = 3_000;

fn memory_limit() -> usize {
    std::env::var("AUGUST_MEMORY_CHARS").ok().and_then(|v| v.parse().ok()).unwrap_or(DEFAULT_MEMORY_CHARS)
}

pub struct Remember;

#[async_trait]
impl Tool for Remember {
    fn name(&self) -> &'static str {
        "remember"
    }

    fn description(&self) -> &'static str {
        "Save a durable fact about the user or their world (preferences, people, projects, \
         decisions) so it is available in every future conversation. One short self-contained \
         sentence per fact. Don't save temporary details or things already remembered. Memory \
         has a size limit: to update or merge facts, pass the ids they replace in `replaces`."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "fact": {"type": "string", "description": "The fact to remember"},
                "replaces": {"type": "array", "items": {"type": "integer"}, "description": "Ids of facts this one replaces (they are deleted)"}
            },
            "required": ["fact"],
            "additionalProperties": false
        })
    }

    async fn call(&self, input: &Value, ctx: &ToolCtx) -> Result<String> {
        let fact = str_arg(input, "fact")?.trim();
        if fact.is_empty() {
            anyhow::bail!("empty fact");
        }
        let replaces: Vec<i64> = input["replaces"].as_array().into_iter().flatten().filter_map(|v| v.as_i64()).collect();
        let facts = ctx.db.facts()?;
        if replaces.is_empty() && facts.iter().any(|f| f.text == fact) {
            return Ok("already remembered".into());
        }
        let kept: usize = facts.iter().filter(|f| !replaces.contains(&f.id)).map(|f| f.text.chars().count()).sum();
        let (used, limit) = (kept + fact.chars().count(), memory_limit());
        if used > limit {
            let list: Vec<String> = facts.iter().map(|f| format!("#{} {}", f.id, f.text)).collect();
            anyhow::bail!(
                "memory is full: this would use {used}/{limit} characters. Call `remember` again with \
                 `replaces` listing outdated facts, or facts merged into this one (shorten them), to \
                 free at least {} characters. Current facts:\n{}",
                used - limit,
                list.join("\n")
            );
        }
        let id = ctx.db.replace_facts(&replaces, fact)?;
        Ok(format!("remembered as #{id} ({used}/{limit} characters used)"))
    }
}

pub struct Forget;

#[async_trait]
impl Tool for Forget {
    fn name(&self) -> &'static str {
        "forget"
    }

    fn description(&self) -> &'static str {
        "Delete a remembered fact by its #id (shown in the Memory section of the system prompt), \
         e.g. when it is outdated or wrong."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {"id": {"type": "integer", "description": "Fact id"}},
            "required": ["id"],
            "additionalProperties": false
        })
    }

    async fn call(&self, input: &Value, ctx: &ToolCtx) -> Result<String> {
        let id = input["id"].as_i64().ok_or_else(|| anyhow::anyhow!("missing integer argument `id`"))?;
        Ok(if ctx.db.delete_fact(id)? { format!("forgot #{id}") } else { format!("no fact #{id}") })
    }
}

pub struct SearchHistory;

#[async_trait]
impl Tool for SearchHistory {
    fn name(&self) -> &'static str {
        "search_history"
    }

    fn description(&self) -> &'static str {
        "Full-text search over all past conversations (every chat, including ones that were \
         compacted or reset). Use it when the user refers to something said earlier that you \
         can't see in the current conversation."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": {"type": "string", "description": "Words to look for"},
                "limit": {"type": "integer", "description": "Max results (default 8)"}
            },
            "required": ["query"],
            "additionalProperties": false
        })
    }

    async fn call(&self, input: &Value, ctx: &ToolCtx) -> Result<String> {
        let limit = input["limit"].as_u64().unwrap_or(8).clamp(1, 20) as usize;
        let hits = ctx.db.search(str_arg(input, "query")?, limit)?;
        if hits.is_empty() {
            return Ok("no matches".into());
        }
        let out: Vec<String> = hits
            .iter()
            .map(|h| {
                let text: String = h.text.chars().take(500).collect();
                format!("[{} {}] {}", crate::util::fmt_time(h.at), h.role, text)
            })
            .collect();
        Ok(out.join("\n---\n"))
    }
}
