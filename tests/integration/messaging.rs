//! The agent uses the messenger primitives too: it sees the threads and writes to another one.
//! What the user answers comes with the message it answers.

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

#[tokio::test]
async fn a_reply_shows_the_agent_what_it_answers() {
    let llm: Llm = Box::new(|req| {
        let last = req["messages"].as_array().unwrap().last().unwrap()["content"].to_string();
        if last.contains("why?") {
            assert!(last.contains("replying to your message") && last.contains("> Paris is the capital."), "{last}");
            reply_text("because of history")
        } else {
            reply_text("Paris is the capital.")
        }
    });
    let fake = Fake::llm(llm).await;
    let gw = august(&fake, Setup::default()).await;
    let mut chat = gw.chat().await;
    chat.ask("capital of France?", "Paris is the capital.").await;
    let answer = chat.wait_for("Paris is the capital.").await;
    chat.reply(&answer, "why?").await;
    chat.wait_for("because of history").await;
}
