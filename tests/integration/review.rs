//! Learning after a turn: a background review replays the conversation with the same
//! system prompt and tools and saves what is worth keeping, after the reply was sent.

use crate::support::*;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::time::Duration;

const TIMEOUT: Duration = Duration::from_secs(30);

fn msgs(req: &Value) -> &Vec<Value> {
    req["messages"].as_array().unwrap()
}

fn is_review(req: &Value) -> bool {
    msgs(req).iter().any(|m| m["role"] == "user" && m["content"].as_str().is_some_and(|c| c.starts_with("[Background review]")))
}

fn command(id: i64, text: &str) -> Value {
    message(id, json!({"text": text, "entities": [{"type": "bot_command", "offset": 0, "length": text.len()}]}))
}

#[tokio::test]
async fn review_saves_a_correction_after_the_reply() {
    let llm: Llm = Box::new(|req| {
        let last = msgs(req).last().unwrap();
        if is_review(req) {
            return if last["role"] == "tool" {
                reply_text("Saved.")
            } else {
                reply_tool("remember", json!({"fact": "User wants answers without bullet lists"}))
            };
        }
        reply_text("Sure, no lists from now on.")
    });
    let fake = Fake::start(vec![message(1, json!({"text": "stop using bullet lists please"}))], HashMap::new(), Some(llm)).await;
    let _gw = spawn_gateway_env(&fake, LlmSetup::Fake, &[], &[], &[("AUGUST_REVIEW_MEMORY_EVERY", "1")]);

    fake.wait_for(TIMEOUT, |f| f.llm_requests().iter().filter(|r| is_review(r)).count() >= 2).await;
    fake.push_updates(vec![command(2, "/memory")]);
    fake.wait_for(TIMEOUT, |f| f.sent_texts().iter().any(|t| t.contains("User wants answers without bullet lists"))).await;

    let reqs = fake.llm_requests();
    let (chat, review) = (&reqs[0], reqs.iter().find(|r| is_review(r)).unwrap());
    // The reply went out first; the review reuses the conversation's prompt prefix.
    assert!(!is_review(chat));
    assert_eq!(chat["tools"], review["tools"]);
    assert_eq!(msgs(chat)[0], msgs(review)[0]);
    assert_eq!(msgs(review)[1], msgs(chat)[1]);
    assert_eq!(msgs(review)[2]["content"], "Sure, no lists from now on.");
}

#[tokio::test]
async fn review_can_only_save_and_read() {
    let llm: Llm = Box::new(|req| {
        let last = msgs(req).last().unwrap();
        if is_review(req) {
            return if last["role"] == "tool" {
                reply_text(&format!("review saw: {}", last["content"].as_str().unwrap_or("")))
            } else {
                reply_tool("shell", json!({"command": "echo hi"}))
            };
        }
        reply_text("hello")
    });
    let fake = Fake::start(vec![message(1, json!({"text": "hi"}))], HashMap::new(), Some(llm)).await;
    let _gw = spawn_gateway_env(&fake, LlmSetup::Fake, &[], &[], &[("AUGUST_REVIEW_MEMORY_EVERY", "1")]);
    fake.wait_for(TIMEOUT, |f| {
        f.llm_requests().iter().any(|r| is_review(r) && msgs(r).last().unwrap()["role"] == "tool")
    })
    .await;
    let review = fake.llm_requests().into_iter().filter(|r| is_review(r)).last().unwrap();
    let out = msgs(&review).last().unwrap()["content"].as_str().unwrap().to_string();
    assert!(out.contains("not available during the review"), "{out}");
}
