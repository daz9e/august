//! Token usage: every model call August makes (`llm_result`) counted for its conversation
//! and its day; `/usage` shows this conversation's and today's.

use anyhow::Result;
use august_ext::{August, Ctx};
use serde_json::{Value, json};
use std::sync::Arc;
use tokio::sync::Mutex;

const FIELDS: [(&str, &str); 4] =
    [("input", "inputTokens"), ("output", "outputTokens"), ("cache_read", "cacheReadTokens"), ("cache_write", "cacheWriteTokens")];

fn today() -> String {
    format!("day:{}", chrono::Local::now().format("%Y-%m-%d"))
}

/// Adds one call with `usage` to the totals at `key`.
async fn add(august: &August, key: &str, usage: &Value) -> Result<()> {
    let mut total = august.get(key).await?.unwrap_or_else(|| json!({"calls": 0}));
    total["calls"] = json!(total["calls"].as_u64().unwrap_or(0) + 1);
    for (mine, theirs) in FIELDS {
        total[mine] = json!(total[mine].as_u64().unwrap_or(0) + usage[theirs].as_u64().unwrap_or(0));
    }
    august.set(key, total).await
}

fn line(u: Option<Value>) -> String {
    let u = u.unwrap_or_default();
    let n = |k: &str| u[k].as_u64().unwrap_or(0);
    format!("{} calls · in {} · cache read {} · cache write {} · out {}", n("calls"), n("input"), n("cache_read"), n("cache_write"), n("output"))
}

async fn report(august: &August, ctx: &Ctx) -> Result<String> {
    let sessions = ctx.call("sessions", json!({})).await?;
    let key = ctx.key();
    let bound = |s: &&Value| s["bound"].as_array().is_some_and(|b| b.iter().any(|k| k == key.as_str()));
    let current = sessions.as_array().into_iter().flatten().find(bound).and_then(|s| s["id"].as_str().map(String::from));
    let session = match current {
        Some(id) => august.get(&format!("session:{id}")).await?,
        None => None,
    };
    Ok(format!("This session: {}\nToday, all chats: {}", line(session), line(august.get(&today()).await?)))
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    serve(August::new()).await
}

async fn serve(august: August) {
    august.needs(&["sessions"]);
    august.describe("Token usage", "Counts the tokens of every model call; /usage shows them.");
    // Totals are read, added to and written back: one call at a time.
    let lock = Arc::new(Mutex::new(()));
    let a = august.clone();
    august.on("llm_result", move |data, _| {
        let (a, lock) = (a.clone(), lock.clone());
        async move {
            let _one = lock.lock().await;
            if let Some(session) = data["session"].as_str() {
                add(&a, &format!("session:{session}"), &data["usage"]).await?;
            }
            add(&a, &today(), &data["usage"]).await?;
            Ok(None)
        }
    });
    let a = august.clone();
    august.register_command("usage", "Show token usage of this conversation and today", move |_, ctx| {
        let a = a.clone();
        async move { report(&a, &ctx).await.map(Some) }
    });
    august.run().await;
}

#[cfg(test)]
mod tests;
