//! How August talks to the user: who it is and how its replies are shown (the start of the
//! system prompt), the time on each message, who a message is from when not the user
//! (`[from <source>]`), the message one answers (quoted), and what happens to a message sent while it works (acknowledged,
//! marked for the model).

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
    serve(August::new()).await
}

async fn serve(august: August) {
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
            let from = match ctx.turn.as_ref().filter(|t| t.show).and_then(|t| t.source.as_deref()) {
                Some(s) if s != "user" => format!("[from {s}] "),
                _ => String::new(),
            };
            let text = format!("[{}] {from}{}", stamp(), data["text"].as_str().unwrap_or_default());
            Ok(Some(json!({"system": system.join("\n"), "text": text})))
        }
    });

    august.on("message_in", move |data, ctx| async move {
        let text = data["text"].as_str().unwrap_or_default();
        let text = &match data["reply_to"]["text"].as_str().filter(|q| !q.trim().is_empty()) {
            Some(quote) => {
                let whose = if data["reply_to"]["mine"] == true { "your" } else { "a" };
                let quoted: String = quote.lines().map(|l| format!("> {l}\n")).collect();
                format!("(replying to {whose} message)\n{quoted}\n{text}")
            }
            None => text.to_string(),
        };
        let from = match data["source"].as_str() {
            Some(s) if s != "user" => format!("[from {s}] "),
            _ => String::new(),
        };
        if data["steer"] != true {
            let changed = !from.is_empty() || !data["reply_to"].is_null();
            return Ok(changed.then(|| json!({"text": format!("{from}{text}")})));
        }
        if from.is_empty() {
            ctx.send("↪️ Got it, I'll take this into account.").await?;
        }
        Ok(Some(json!({"text": format!("[{}] [Sent while you were working] {from}{text}", stamp())})))
    });
    august.run().await;
}

#[cfg(test)]
mod tests;
