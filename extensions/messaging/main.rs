//! The agent's own use of the messenger primitives: see which messengers and threads there
//! are (`messengers`), write to any of them (`send_message`), e.g. from the terminal to
//! the user's Telegram, hand the user a file from the workspace (`send_file`), open a thread
//! (`open_thread`) and use what one messenger alone can do (`messenger_action`).

use anyhow::bail;
use august_ext::{August, Thread, str_arg};
use serde_json::{Value, json};

/// A thread's place, e.g. ` (group "Family")`; nothing for a private chat.
fn place(p: &Value) -> String {
    let kind = p["kind"].as_str().unwrap_or("dm");
    if kind == "dm" {
        return String::new();
    }
    let title = p["title"].as_str().map(|t| format!(" \"{t}\"")).unwrap_or_default();
    let parent = p["parent"].as_str().map(|t| format!(" in {t}")).unwrap_or_default();
    format!(" ({kind}{title}{parent})")
}

/// Per messenger: what it can do, its notes, actions and threads, the active one starred.
fn describe(all: &Value) -> String {
    let lines: Vec<String> = all
        .as_array()
        .into_iter()
        .flatten()
        .map(|m| {
            let c = &m["capabilities"];
            let can: Vec<&str> = [("buttons", c["buttons"].as_u64().unwrap_or(0) > 0), ("files", c["files_out"] == true), ("images", c["images"] == true), ("reactions", c["reactions"] == true), ("open_thread", c["open_thread"] == true)]
                .into_iter()
                .filter_map(|(name, has)| has.then_some(name))
                .collect();
            let threads: Vec<String> = m["threads"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|t| format!("{}{}{}", t["id"].as_str().unwrap_or("?"), place(&t["place"]), if t["active"] == true { "*" } else { "" }))
                .collect();
            let mut out = format!("{} ({}; {}) threads: {}", m["id"].as_str().unwrap_or("?"), m["name"].as_str().unwrap_or(""), can.join(", "), threads.join(", "));
            if let Some(notes) = m["notes"].as_str().filter(|n| !n.is_empty()) {
                out += &format!("\n  {notes}");
            }
            for a in m["actions"].as_array().into_iter().flatten() {
                out += &format!("\n  action {}: {} args {}", a["name"].as_str().unwrap_or("?"), a["description"].as_str().unwrap_or(""), a["input_schema"]);
            }
            out
        })
        .collect();
    format!("{}\n(* = where the user wrote last)", lines.join("\n"))
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    serve(August::new()).await
}

async fn serve(august: August) {
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
    let me = august.clone();
    august.register_tool(
        "open_thread",
        "Open a new thread (e.g. a forum topic) inside a thread of a messenger that can \
         (`open_thread` in `messengers`), to keep a task's conversation apart; returns its id.",
        json!({
            "type": "object",
            "properties": {
                "messenger": {"type": "string"},
                "thread": {"type": "string", "description": "the thread to open it in"},
                "title": {"type": "string"}
            },
            "required": ["messenger", "thread", "title"],
            "additionalProperties": false
        }),
        move |input, _| {
            let august = me.clone();
            async move {
                let thread = json!({"messenger": str_arg(&input, "messenger"), "id": str_arg(&input, "thread")});
                let opened = august.call("open_thread", json!({"thread": thread, "title": str_arg(&input, "title")})).await?;
                Ok(format!("opened thread {}", opened["id"].as_str().unwrap_or("?")))
            }
        },
    );
    let me = august.clone();
    august.register_tool(
        "messenger_action",
        "Do something only one messenger can (pin a message, ...): an action `messengers` lists \
         for it, with arguments matching its schema.",
        json!({
            "type": "object",
            "properties": {
                "messenger": {"type": "string"},
                "thread": {"type": "string"},
                "action": {"type": "string"},
                "args": {"type": "object"}
            },
            "required": ["messenger", "thread", "action"],
            "additionalProperties": false
        }),
        move |input, _| {
            let august = me.clone();
            async move {
                let thread = json!({"messenger": str_arg(&input, "messenger"), "id": str_arg(&input, "thread")});
                let params = json!({"thread": thread, "action": str_arg(&input, "action"), "args": input["args"]});
                let result = august.call("action", params).await?;
                Ok(if result.is_null() { "done".to_string() } else { result.to_string() })
            }
        },
    );
    let me = august.clone();
    august.register_tool(
        "send_file",
        "Send a file from the workspace to the user in the chat (images are shown inline). \
         Use it to deliver files you created or downloaded, not to show text you can write in the reply.",
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "Path relative to the workspace"},
                "caption": {"type": "string", "description": "Optional short caption"}
            },
            "required": ["path"],
            "additionalProperties": false
        }),
        move |input, ctx| {
            let workspace = me.workspace().clone();
            async move {
                let rel = str_arg(&input, "path");
                let path = workspace.join(rel).canonicalize().map_err(|_| anyhow::anyhow!("no such file: {rel}"))?;
                if !path.starts_with(workspace.canonicalize()?) || !path.is_file() {
                    bail!("not a file in the workspace: {rel}");
                }
                if ctx.thread.is_none() {
                    bail!("there is no chat to send files to; tell the user the path instead: {rel}");
                }
                let message = json!({"text": str_arg(&input, "caption"), "files": [path]});
                ctx.call("send", json!({"message": message})).await?;
                Ok(format!("sent {rel} to the chat"))
            }
        },
    );
    august.run().await;
}

#[cfg(test)]
mod tests;
