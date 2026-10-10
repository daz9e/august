//! `core`: the agent calls any operation of the core's table (`ops`) through one tool,
//! `august`, the same way extensions do. It declares every permission but `user`, so the
//! agent acts as itself, never for the user; `approvals` asks before anything that changes.

use anyhow::bail;
use august_ext::{August, str_arg};
use serde_json::json;

#[tokio::main(flavor = "current_thread")]
async fn main() {
    serve(August::new()).await
}

async fn serve(august: August) {
    august.needs(&["messaging", "turns", "tools", "llm", "models", "sessions", "config", "admin"]);
    august.describe(
        "The `august` tool: call any of August's core operations (`ops` lists them)",
        "Passes `{op, params}` to the core as this extension's call and returns the result as \
         JSON. `params.thread` defaults to the thread the tool runs in. Operations that only \
         read pass at once; the rest go through `approvals`.",
    );
    let me = august.clone();
    august.register_tool(
        "august",
        "Call one of August's core operations: sessions, history, search, turns, models, \
         messengers, accounts, extensions, settings and more. `op: \"ops\"` lists every \
         operation with its parameters; `op: \"guide\"` explains them in depth. `params.thread` \
         defaults to the current chat.",
        json!({
            "type": "object",
            "properties": {
                "op": {"type": "string", "description": "Operation name, e.g. `ops`, `sessions`, `model_set`"},
                "params": {"type": "object", "description": "The operation's parameters"}
            },
            "required": ["op"],
            "additionalProperties": false
        }),
        move |input, ctx| {
            let august = me.clone();
            async move {
                let op = str_arg(&input, "op");
                if op.is_empty() {
                    bail!("missing string argument `op`");
                }
                let mut params = input["params"].clone();
                if !params.is_object() {
                    params = json!({});
                }
                if params["thread"].is_null()
                    && let Some(t) = &ctx.thread
                {
                    params["thread"] = json!(t);
                }
                // So the core knows the call comes from this turn (a new session in this
                // chat then waits for it to end).
                if let Some(turn) = &ctx.turn {
                    params["from_turn"] = json!(turn.id);
                }
                let out = august.call(op, params).await?;
                Ok(match out.as_str() {
                    Some(s) => s.to_string(),
                    None => serde_json::to_string_pretty(&out)?,
                })
            }
        },
    );
    august.run().await;
}

#[cfg(test)]
mod tests;
