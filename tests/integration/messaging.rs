//! The agent uses the messenger primitives too: it sees the threads and writes to another one.

use crate::support::*;
use serde_json::json;

#[tokio::test]
async fn the_agent_writes_to_another_thread() {
    let llm: Llm = Box::new(|req| {
        let msgs = req["messages"].as_array().unwrap();
        let results: Vec<&str> = msgs.iter().filter(|m| m["role"] == "tool").filter_map(|m| m["content"].as_str()).collect();
        match results.len() {
            0 => reply_tool("messengers", json!({})),
            1 => {
                assert!(results[0].contains("cli (a terminal") && results[0].contains("threads: 1*, 2"), "{}", results[0]);
                reply_tool("send_message", json!({"messenger": "cli", "thread": "2", "text": "ping from window 1"}))
            }
            _ => reply_text(&format!("done: {}", results[1])),
        }
    });
    let fake = Fake::llm(llm).await;
    let gw = august(&fake, Setup::default()).await;
    let mut first = gw.chat().await;
    let second = gw.chat().await;
    first.ask("tell the other window", "done: sent to cli:2").await;
    second.wait_for("ping from window 1").await;
}
