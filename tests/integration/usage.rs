//! Token accounting: every model call (turns, tool steps, compaction summaries) is
//! recorded per session, and `/usage` reports the sums.

use crate::support::*;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::time::Duration;

const TIMEOUT: Duration = Duration::from_secs(30);

fn command(id: i64, text: &str) -> Value {
    message(id, json!({"text": text, "entities": [{"type": "bot_command", "offset": 0, "length": text.len()}]}))
}

/// Sends an update and waits until the bot has answered it with a final message.
async fn send(fake: &Fake, update: Value, done: &str) {
    let n = fake.sent_texts().len();
    fake.push_updates(vec![update]);
    fake.wait_for(TIMEOUT, |f| f.sent_texts()[n..].iter().any(|t| t.contains(done))).await;
}

#[tokio::test]
async fn usage_sums_every_model_call_of_the_session() {
    // Every call: 100 prompt tokens (40 of them cached) and 10 completion tokens.
    let llm: Llm = Box::new(|req| {
        let msgs = req["messages"].as_array().unwrap();
        let last = msgs.last().unwrap();
        let reply = if msgs[0]["content"].as_str().unwrap_or("").contains("compress conversations") {
            reply_text("summary of the chat")
        } else if last["role"] == "tool" {
            reply_text("done")
        } else if last["content"].to_string().contains("look") {
            reply_tool("list_dir", json!({"path": "."}))
        } else {
            reply_text("ok")
        };
        with_usage(reply, 100, 10, 40)
    });
    let fake = Fake::start(vec![], HashMap::new(), Some(llm)).await;
    let _gw = spawn_gateway(&fake, LlmSetup::Fake, &[]);

    send(&fake, message(1, json!({"text": "look around"})), "done").await;
    send(&fake, message(2, json!({"text": "look again"})), "done").await;
    send(&fake, message(3, json!({"text": "thanks"})), "ok").await;
    // Ten messages now: enough for /compact to summarise the older ones.
    send(&fake, command(4, "/compact"), "Compacted").await;
    assert_eq!(fake.llm_requests().len(), 6);

    send(&fake, command(5, "/usage"), "This session").await;
    let report = fake.sent_texts().last().unwrap().clone();
    let totals = "6 calls · in 360 · cache read 240 · cache write 0 · out 60";
    assert!(report.contains(&format!("This session: {totals}")), "{report}");
    assert!(report.contains(&format!("Today, all chats: {totals}")), "{report}");

    // A new conversation starts from zero; today's totals keep counting.
    send(&fake, command(6, "/new"), "new conversation").await;
    send(&fake, message(7, json!({"text": "hi"})), "ok").await;
    send(&fake, command(8, "/usage"), "This session").await;
    let report = fake.sent_texts().last().unwrap().clone();
    assert!(report.contains("This session: 1 calls · in 60 · cache read 40 · cache write 0 · out 10"), "{report}");
    assert!(report.contains("Today, all chats: 7 calls · in 420"), "{report}");
}
