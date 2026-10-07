//! Token accounting: every model call (turns, tool steps, compaction summaries) is
//! recorded per session, and `/usage` reports the sums.

use crate::support::*;
use serde_json::json;

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
    let fake = Fake::llm(llm).await;
    let gw = august(&fake, Setup::default()).await;
    let mut chat = gw.chat().await;

    chat.ask("look around", "done").await;
    chat.ask("look again", "done").await;
    chat.ask("thanks", "ok").await;
    // Ten messages now: enough for /compact to summarise the older ones.
    chat.ask("/compact", "Compacted").await;
    assert_eq!(fake.llm_requests().len(), 6);

    let report = chat.ask("/usage", "This session").await;
    let totals = "6 calls · in 360 · cache read 240 · cache write 0 · out 60";
    assert!(report.contains(&format!("This session: {totals}")), "{report}");
    assert!(report.contains(&format!("Today, all chats: {totals}")), "{report}");

    // A new conversation starts from zero; today's totals keep counting.
    chat.ask("/new", "new conversation").await;
    chat.ask("hi", "ok").await;
    let report = chat.ask("/usage", "This session").await;
    assert!(report.contains("This session: 1 calls · in 60 · cache read 40 · cache write 0 · out 10"), "{report}");
    assert!(report.contains("Today, all chats: 7 calls · in 420"), "{report}");
}
