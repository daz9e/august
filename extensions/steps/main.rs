//! `steps`: how many model calls a turn may make. The core sets no limit; from the limit on
//! this offers the model no tools and asks it to report, so the turn ends with what was done
//! and what is left instead of running on. A turn's starter may set its own limit
//! (`meta.max_steps`, e.g. a review's fork).

#[cfg(test)]
mod tests;

use august_ext::August;
use serde_json::json;

const NOTE: &str = "[Step limit reached: you used all the steps of this turn. Don't call tools. Tell the \
    user briefly what you did, what is left, and how to continue.]";

#[tokio::main(flavor = "current_thread")]
async fn main() {
    serve(August::new()).await
}

async fn serve(august: August) {
    august.describe(
        "Ends a turn that makes too many model calls with a report",
        "From `max_steps` model calls on (or the turn's own `meta.max_steps`), the model is \
         offered no tools and asked to say what it did and what is left.",
    );
    august.settings_schema(json!({
        "type": "object",
        "properties": {
            "max_steps": {"type": "integer", "default": 150, "description": "Model calls one turn may make"},
        },
    }));
    let me = august.clone();
    august.on("llm_call", move |data, ctx| {
        let august = me.clone();
        async move {
            let own = ctx.turn.as_ref().and_then(|t| t.meta["max_steps"].as_u64());
            let limit = match own {
                Some(n) => n,
                None => august.settings().await?["max_steps"].as_u64().unwrap_or(150),
            };
            if data["step"].as_u64().unwrap_or(0) + 1 < limit {
                return Ok(None);
            }
            let system = format!("{}\n\n{NOTE}", data["system"].as_str().unwrap_or_default());
            Ok(Some(json!({"tools": [], "system": system})))
        }
    });
    august.run().await;
}
