//! `retry`: a model call that failed for a passing reason (rate limit, overload, a broken
//! connection) is tried again after a growing pause; when it keeps failing, the rest of the
//! turn goes to the `fallback` model, if one is set. A call that already showed part of its
//! reply is not repeated: the user would see it twice.

use august_ext::August;
use serde_json::json;

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let august = August::new();
    august.describe(
        "Retries model calls that fail for a passing reason; switches to a fallback model",
        "Rate limits, overloads and broken connections are retried after a growing pause \
         (`attempts`, `delay_ms`); then the turn goes on with `fallback` (provider:model), if set.",
    );
    august.settings_schema(json!({
        "type": "object",
        "properties": {
            "attempts": {"type": "integer", "default": 3, "description": "Tries of the active model before giving up or falling back"},
            "delay_ms": {"type": "integer", "default": 2000, "description": "Pause before the first retry; doubles with each"},
            "fallback": {"type": "string", "description": "Model the turn moves to when the active one keeps failing: provider:model"},
            "fallback_attempts": {"type": "integer", "default": 2, "description": "Tries of the fallback model"},
        },
    }));
    let me = august.clone();
    august.on("llm_error", move |data, _| {
        let august = me.clone();
        async move {
            let passing = ["rate_limit", "overloaded", "network"].iter().any(|k| data["error"]["kind"] == *k);
            if !passing || data["streamed"] == true {
                return Ok(None);
            }
            let s = august.settings().await?;
            let num = |k: &str| s[k].as_u64().unwrap_or(0);
            let (attempt, attempts) = (data["attempt"].as_u64().unwrap_or(1).max(1), num("attempts"));
            let delay = |n: u64| num("delay_ms").saturating_mul(1 << n.min(10));
            if attempt < attempts {
                return Ok(Some(json!({"retry": true, "delayMs": delay(attempt - 1)})));
            }
            let Some(fallback) = s["fallback"].as_str().filter(|f| !f.is_empty()) else { return Ok(None) };
            // `attempt` keeps counting on the fallback: the first try there comes right away.
            match attempt - attempts {
                0 => Ok(Some(json!({"retry": true, "model": fallback}))),
                n if n < num("fallback_attempts") => Ok(Some(json!({"retry": true, "delayMs": delay(n - 1)}))),
                _ => Ok(None),
            }
        }
    });
    august.run().await;
}
