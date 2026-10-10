//! How August talks to the user: who it is and how its replies are shown (the start of the
//! system prompt), the time on each message, and what happens to a message sent while it
//! works (acknowledged, marked for the model).

use august_ext::August;
use serde_json::{Value, json};

fn stamp() -> String {
    chrono::Local::now().format("%a %Y-%m-%d %H:%M").to_string()
}

/// The line on how replies are shown, from the messenger's description.
fn surface(m: &Value) -> String {
    let name = m["name"].as_str().unwrap_or("a messenger");
    if m["capabilities"]["markdown"] == true {
        format!(
            "The user reads your replies in {name}, which renders Markdown (bold, italic, `code`, \
             fenced code blocks, lists, links). Avoid tables and headings unless they really help."
        )
    } else {
        format!("The user reads your replies in {name}: plain text, Markdown is shown as is.")
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let august = August::new();
    august.needs(&["messaging"]);
    august.describe("How August talks", "The start of the system prompt, timestamps on messages, acknowledgements of messages sent mid-turn.");
    let intro = format!(
        "You are August, a personal assistant agent running on the user's machine.\n\
         Your workspace directory is {}; bash commands run there and file paths are relative to it.\n\
         Use tools to actually do things instead of describing how to do them. Keep replies \
         short. Reply in the user's language. Each user message starts with a [timestamp] in \
         the user's local time; it is added automatically, don't copy it into replies.",
        august.workspace().display()
    );

    let me = august.clone();
    august.on("before_turn", move |data, ctx| {
        let (august, intro) = (me.clone(), intro.clone());
        async move {
            let messenger = ctx.thread.as_ref().map(|t| t.messenger.clone()).unwrap_or_default();
            let all = august.messengers().await.unwrap_or_default();
            let line = all.as_array().into_iter().flatten().find(|m| m["id"] == messenger.as_str()).map(surface);
            let own = data["system"].as_str().unwrap_or_default().trim();
            let system: Vec<&str> = [intro.as_str(), line.as_deref().unwrap_or(""), own].into_iter().filter(|s| !s.is_empty()).collect();
            let text = format!("[{}] {}", stamp(), data["text"].as_str().unwrap_or_default());
            Ok(Some(json!({"system": system.join("\n"), "text": text})))
        }
    });

    august.on("message_in", move |data, ctx| async move {
        if data["steer"] != true {
            return Ok(None);
        }
        if data["source"] == "user" {
            ctx.send("↪️ Got it, I'll take this into account.").await?;
        }
        let text = format!("[{}] [Sent while you were working] {}", stamp(), data["text"].as_str().unwrap_or_default());
        Ok(Some(json!({"text": text})))
    });
    august.run().await;
}
