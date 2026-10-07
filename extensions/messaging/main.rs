//! The agent's own use of the messenger primitives: see which messengers and threads there
//! are (`messengers`) and write to any of them (`send_message`), e.g. from the terminal to
//! the user's Telegram.

use anyhow::bail;
use august_ext::{August, Thread, str_arg};
use serde_json::{Value, json};

/// One line per messenger: what it can do and its threads, the active one starred.
fn describe(all: &Value) -> String {
    let lines: Vec<String> = all
        .as_array()
        .into_iter()
        .flatten()
        .map(|m| {
            let c = &m["capabilities"];
            let can: Vec<&str> = [("buttons", c["buttons"].as_u64().unwrap_or(0) > 0), ("files", c["files_out"] == true), ("images", c["images"] == true), ("reactions", c["reactions"] == true)]
                .into_iter()
                .filter_map(|(name, has)| has.then_some(name))
                .collect();
            let threads: Vec<String> = m["threads"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|t| format!("{}{}", t["id"].as_str().unwrap_or("?"), if t["active"] == true { "*" } else { "" }))
                .collect();
            format!("{} ({}; {}) threads: {}", m["id"].as_str().unwrap_or("?"), m["name"].as_str().unwrap_or(""), can.join(", "), threads.join(", "))
        })
        .collect();
    format!("{}\n(* = where the user wrote last)", lines.join("\n"))
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let august = August::new();
    august.needs(&["messaging"]);
    let me = august.clone();
    august.register_tool(
        "messengers",
        "List the messengers August is connected to (Telegram, terminal windows, ...), what each can \
         show, and their threads; the one the user wrote in last is starred.",
        json!({"type": "object", "properties": {}, "additionalProperties": false}),
        move |_, _| {
            let august = me.clone();
            async move { Ok(describe(&august.messengers().await?)) }
        },
    );
    let me = august.clone();
    august.register_tool(
        "send_message",
        "Send a message to the user in another messenger or thread than this conversation (e.g. \
         from the terminal to Telegram); call `messengers` for the threads. Your normal reply already \
         goes to this conversation, so don't use this for it.",
        json!({
            "type": "object",
            "properties": {
                "messenger": {"type": "string", "description": "e.g. telegram, cli"},
                "thread": {"type": "string", "description": "the thread id from `messengers`"},
                "text": {"type": "string", "description": "Markdown"}
            },
            "required": ["messenger", "thread", "text"],
            "additionalProperties": false
        }),
        move |input, ctx| {
            let august = me.clone();
            async move {
                let thread = Thread { messenger: str_arg(&input, "messenger").into(), id: str_arg(&input, "thread").into() };
                let text = str_arg(&input, "text").trim();
                if text.is_empty() {
                    bail!("`text` is empty");
                }
                if ctx.thread.as_ref() == Some(&thread) {
                    bail!("that is this conversation; just reply");
                }
                august.send(&thread, text, &[]).await?;
                Ok(format!("sent to {}", thread.key()))
            }
        },
    );
    august.run().await;
}
