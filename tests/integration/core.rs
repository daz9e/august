//! The agent calls the core's operations itself through the `august` tool: reads pass at
//! once, changes are asked about first.

use crate::support::*;
use serde_json::json;

#[tokio::test]
async fn the_agent_calls_core_operations_reads_freely_and_changes_with_approval() {
    let fake = Fake::llm(Box::new(|req| {
        let last = req["messages"].as_array().unwrap().last().unwrap();
        if last["role"] == "tool" {
            return reply_text(&format!("Result: {}", last["content"].as_str().unwrap_or("")));
        }
        let text = last["content"].to_string();
        let input = if text.contains("what can you") {
            json!({"op": "ops"})
        } else if text.contains("new chat") {
            json!({"op": "session_new", "params": {"name": "side project"}})
        } else {
            json!({"op": "sessions"})
        };
        reply_tool("august", input)
    }))
    .await;
    let gw = august(&fake, Setup::default()).await;
    let mut chat = gw.chat().await;

    // Reading needs no approval.
    let ops = chat.ask("what can you call", "Result:").await;
    assert!(ops.contains("session_new") && ops.contains("model_set"), "{ops}");

    // A change is asked about, then runs in the current chat.
    chat.say("start a new chat").await;
    let ask = chat.question().await;
    assert!(ask.text.contains("session_new"), "{}", ask.text);
    chat.press(&ask.button("Allow")).await;
    chat.wait_until("the new session's id", |c| c.texts().iter().filter(|t| t.contains("Result:")).count() == 2).await;

    let sessions = chat.ask("list sessions", "Result:").await;
    assert!(sessions.contains("side project"), "{sessions}");
}
