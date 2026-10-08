//! `memory`: durable facts about the user, kept in this extension's store and shown in the
//! system prompt (from the next conversation, so the running prompt stays cacheable), plus
//! full-text search over every past conversation. `remember`, `forget`, `search_history`,
//! `/memory`. The facts may take `AUGUST_MEMORY_CHARS` characters in all (default 3000).

use anyhow::{Result, anyhow, bail};
use august_ext::{August, str_arg};
use chrono::TimeZone;
use serde_json::{Value, json};
use std::sync::Arc;
use tokio::sync::Mutex;

const DEFAULT_MEMORY_CHARS: usize = 3_000;

#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct Fact {
    id: i64,
    text: String,
}

fn limit() -> usize {
    std::env::var("AUGUST_MEMORY_CHARS").ok().and_then(|v| v.parse().ok()).unwrap_or(DEFAULT_MEMORY_CHARS)
}

/// The facts, behind one lock so parallel tool calls don't overwrite each other.
#[derive(Clone)]
struct Memory {
    august: August,
    lock: Arc<Mutex<()>>,
}

impl Memory {
    async fn facts(&self) -> Result<Vec<Fact>> {
        Ok(self.august.get("facts").await?.map(serde_json::from_value).transpose()?.unwrap_or_default())
    }

    /// Saves `facts` and shows them in the prompt of the next conversation.
    async fn save(&self, facts: &[Fact]) -> Result<()> {
        self.august.set("facts", json!(facts)).await?;
        self.show(facts);
        Ok(())
    }

    fn show(&self, facts: &[Fact]) {
        let mut s = String::from(
            "## Memory\nYou keep durable facts about the user with `remember`/`forget` and can \
             search every past conversation with `search_history`. Save facts proactively when the \
             user shares lasting preferences or details, and look things up instead of asking again.",
        );
        if !facts.is_empty() {
            s += "\nFacts you saved earlier (delete outdated ones with `forget`; facts saved during this \
                  conversation appear here from the next one):\n";
            for f in facts {
                s += &format!("- #{} {}\n", f.id, f.text);
            }
        }
        self.august.register_prompt_section("memory", &s);
    }

    async fn remember(&self, fact: &str, replaces: &[i64]) -> Result<String> {
        let _held = self.lock.lock().await;
        let mut facts = self.facts().await?;
        if replaces.is_empty() && facts.iter().any(|f| f.text == fact) {
            return Ok("already remembered".into());
        }
        if let Some(id) = replaces.iter().find(|id| !facts.iter().any(|f| f.id == **id)) {
            bail!("no fact #{id}");
        }
        let kept: usize = facts.iter().filter(|f| !replaces.contains(&f.id)).map(|f| f.text.chars().count()).sum();
        let (used, limit) = (kept + fact.chars().count(), limit());
        if used > limit {
            let list: Vec<String> = facts.iter().map(|f| format!("#{} {}", f.id, f.text)).collect();
            bail!(
                "memory is full: this would use {used}/{limit} characters. Call `remember` again with \
                 `replaces` listing outdated facts, or facts merged into this one (shorten them), to \
                 free at least {} characters. Current facts:\n{}",
                used - limit,
                list.join("\n")
            );
        }
        // Ids are never reused, so an old `#id` can't point at a newer fact.
        let next = self.august.get("next_id").await?.and_then(|v| v.as_i64());
        let id = next.unwrap_or(0).max(facts.iter().map(|f| f.id + 1).max().unwrap_or(1));
        facts.retain(|f| !replaces.contains(&f.id));
        facts.push(Fact { id, text: fact.into() });
        self.august.set("next_id", json!(id + 1)).await?;
        self.save(&facts).await?;
        Ok(format!("remembered as #{id} ({used}/{limit} characters used)"))
    }

    async fn forget(&self, id: i64) -> Result<String> {
        let _held = self.lock.lock().await;
        let mut facts = self.facts().await?;
        let before = facts.len();
        facts.retain(|f| f.id != id);
        if facts.len() == before {
            return Ok(format!("no fact #{id}"));
        }
        self.save(&facts).await?;
        Ok(format!("forgot #{id}"))
    }
}

fn when(ms: &Value) -> String {
    let ts = ms.as_i64().unwrap_or(0);
    chrono::Local.timestamp_opt(ts, 0).single().map(|t| t.format("%Y-%m-%d %H:%M").to_string()).unwrap_or_else(|| ts.to_string())
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let august = August::new();
    august.needs(&["sessions"]);
    let memory = Memory { august: august.clone(), lock: Arc::default() };
    memory.show(&[]);
    // Replies are read once `run` starts, so the saved facts load right after.
    // ponytail: a conversation started within those milliseconds misses them until its next session.
    let m = memory.clone();
    tokio::spawn(async move {
        match m.facts().await {
            Ok(facts) => m.show(&facts),
            Err(e) => eprintln!("memory: could not load facts: {e:#}"),
        }
    });

    let m = memory.clone();
    august.register_tool(
        "remember",
        "Save a durable fact about the user or their world (preferences, people, projects, \
         decisions) so it is available in every future conversation. One short self-contained \
         sentence per fact. Don't save temporary details or things already remembered. Memory \
         has a size limit: to update or merge facts, pass the ids they replace in `replaces`.",
        json!({
            "type": "object",
            "properties": {
                "fact": {"type": "string", "description": "The fact to remember"},
                "replaces": {"type": "array", "items": {"type": "integer"}, "description": "Ids of facts this one replaces (they are deleted)"}
            },
            "required": ["fact"],
            "additionalProperties": false
        }),
        move |input, _| {
            let m = m.clone();
            async move {
                let fact = str_arg(&input, "fact").trim().to_string();
                if fact.is_empty() {
                    bail!("empty fact");
                }
                let replaces: Vec<i64> = input["replaces"].as_array().into_iter().flatten().filter_map(Value::as_i64).collect();
                m.remember(&fact, &replaces).await
            }
        },
    );

    let m = memory.clone();
    august.register_tool(
        "forget",
        "Delete a remembered fact by its #id (shown in the Memory section of the system prompt), \
         e.g. when it is outdated or wrong.",
        json!({
            "type": "object",
            "properties": {"id": {"type": "integer", "description": "Fact id"}},
            "required": ["id"],
            "additionalProperties": false
        }),
        move |input, _| {
            let m = m.clone();
            async move { m.forget(input["id"].as_i64().ok_or_else(|| anyhow!("missing integer argument `id`"))?).await }
        },
    );

    let a = august.clone();
    august.register_tool(
        "search_history",
        "Full-text search over all past conversations (every chat, including ones that were \
         compacted or reset). Use it when the user refers to something said earlier that you \
         can't see in the current conversation.",
        json!({
            "type": "object",
            "properties": {
                "query": {"type": "string", "description": "Words to look for"},
                "limit": {"type": "integer", "description": "Max results (default 8)"}
            },
            "required": ["query"],
            "additionalProperties": false
        }),
        move |input, _| {
            let a = a.clone();
            async move {
                let limit = input["limit"].as_u64().unwrap_or(8).clamp(1, 20);
                let hits = a.call("search", json!({"query": str_arg(&input, "query"), "limit": limit})).await?;
                let out: Vec<String> = hits
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|h| {
                        let text: String = h["text"].as_str().unwrap_or_default().chars().take(500).collect();
                        format!("[{} {}] {text}", when(&h["at"]), h["role"].as_str().unwrap_or_default())
                    })
                    .collect();
                Ok(if out.is_empty() { "no matches".into() } else { out.join("\n---\n") })
            }
        },
    );

    let m = memory.clone();
    august.register_command("memory", "Show what I remember about you", move |_, _| {
        let m = m.clone();
        async move {
            let lines: Vec<String> = m.facts().await?.iter().map(|f| format!("#{} {}", f.id, f.text)).collect();
            Ok(Some(if lines.is_empty() { "I haven't saved any facts yet.".into() } else { lines.join("\n") }))
        }
    });
    august.run().await;
}
