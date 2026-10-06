//! Long-term memory: what the agent saves reaches the system prompt of the next session,
//! while the prompt of the running session stays byte-identical (so provider caching works).

use crate::support::*;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::time::Duration;

const TIMEOUT: Duration = Duration::from_secs(30);

fn system(req: &Value) -> String {
    req["messages"][0]["content"].as_str().unwrap_or("").to_string()
}

fn last_user_text(req: &Value) -> String {
    let msgs = req["messages"].as_array().unwrap();
    let m = msgs.iter().rev().find(|m| m["role"] == "user").unwrap();
    m["content"].as_str().map(String::from).unwrap_or_else(|| m["content"].to_string())
}

fn command(id: i64, text: &str) -> Value {
    message(id, json!({"text": text, "entities": [{"type": "bot_command", "offset": 0, "length": text.len()}]}))
}

#[tokio::test]
async fn saved_fact_reaches_the_prompt_of_the_next_session() {
    let llm: Llm = Box::new(|req| {
        let last = req["messages"].as_array().unwrap().last().unwrap();
        if last["role"] == "tool" {
            reply_text("noted")
        } else if last_user_text(req).contains("remember") {
            reply_tool("remember", json!({"fact": "User drinks green tea"}))
        } else {
            reply_text("hello")
        }
    });
    let fake = Fake::start(vec![message(1, json!({"text": "remember that I drink green tea"}))], HashMap::new(), Some(llm)).await;
    let _gw = spawn_gateway(&fake, LlmSetup::Fake, &[]);
    let replies = |n: usize| move |f: &Fake| f.sent_texts().len() >= n;

    fake.wait_for(TIMEOUT, |f| f.sent_texts().iter().any(|t| t.contains("noted"))).await;
    let n = fake.sent_texts().len();
    fake.push_updates(vec![message(2, json!({"text": "how are you"}))]);
    fake.wait_for(TIMEOUT, replies(n + 1)).await;

    // Same session: every model call saw the same system prompt, without the new fact.
    let before: Vec<Value> = fake.llm_requests();
    assert_eq!(before.len(), 3);
    assert!(before.iter().all(|r| system(r) == system(&before[0])));
    assert!(!system(&before[0]).contains("green tea"));

    let n = fake.sent_texts().len();
    fake.push_updates(vec![command(3, "/new")]);
    fake.wait_for(TIMEOUT, replies(n + 1)).await;
    let n = fake.sent_texts().len();
    fake.push_updates(vec![message(4, json!({"text": "hi again"}))]);
    fake.wait_for(TIMEOUT, replies(n + 1)).await;

    let after = fake.llm_requests().into_iter().find(|r| last_user_text(r).contains("hi again")).unwrap();
    assert!(system(&after).contains("User drinks green tea"), "{}", system(&after));
}

#[tokio::test]
async fn full_memory_makes_the_agent_merge_facts() {
    // Remembers one fact, then a second that doesn't fit; on the error it merges both.
    let llm: Llm = Box::new(|req| {
        let msgs = req["messages"].as_array().unwrap();
        let last = msgs.last().unwrap();
        let out = last["content"].as_str().unwrap_or("");
        if last["role"] != "tool" {
            let fact = if last_user_text(req).contains("cat") { "User has a cat named Murzik" } else { "User has a dog named Sharik" };
            return reply_tool("remember", json!({"fact": fact}));
        }
        if out.contains("memory is full") {
            assert!(out.contains("#1 User has a cat named Murzik"), "{out}");
            return reply_tool("remember", json!({"fact": "User has a cat Murzik and a dog Sharik", "replaces": [1]}));
        }
        reply_text(&format!("done: {out}"))
    });
    let fake = Fake::start(vec![message(1, json!({"text": "I have a cat"}))], HashMap::new(), Some(llm)).await;
    let _gw = spawn_gateway_env(&fake, LlmSetup::Fake, &[], &[], &[("AUGUST_MEMORY_CHARS", "50")]);
    fake.wait_for(TIMEOUT, |f| f.sent_texts().iter().any(|t| t.contains("done: remembered as #1"))).await;

    fake.push_updates(vec![message(2, json!({"text": "I also have a dog"}))]);
    fake.wait_for(TIMEOUT, |f| f.sent_texts().iter().any(|t| t.contains("done: remembered as #2"))).await;

    fake.push_updates(vec![command(3, "/memory")]);
    fake.wait_for(TIMEOUT, |f| f.sent_texts().iter().any(|t| t.contains("#2 User has a cat Murzik and a dog Sharik"))).await;
    assert!(!fake.sent_texts().iter().any(|t| t.contains("#1 User has a cat named")));
}

#[tokio::test]
async fn compaction_refreshes_the_memory_snapshot() {
    let llm: Llm = Box::new(|req| {
        let last = req["messages"].as_array().unwrap().last().unwrap();
        if system(req).contains("You compress conversations") {
            return reply_text("## Goal\nchat");
        }
        if last["role"] == "tool" {
            reply_text("noted")
        } else if last_user_text(req).contains("remember") {
            reply_tool("remember", json!({"fact": "User plays the cello"}))
        } else {
            reply_text("ok")
        }
    });
    let fake = Fake::start(vec![message(1, json!({"text": "remember that I play the cello"}))], HashMap::new(), Some(llm)).await;
    let _gw = spawn_gateway(&fake, LlmSetup::Fake, &[]);
    fake.wait_for(TIMEOUT, |f| f.sent_texts().iter().any(|t| t.contains("noted"))).await;
    // Enough history for /compact to summarise something.
    for i in 2..=6 {
        let n = fake.sent_texts().len();
        fake.push_updates(vec![message(i, json!({"text": format!("message {i}")}))]);
        fake.wait_for(TIMEOUT, |f| f.sent_texts().len() > n).await;
    }
    let before = fake.llm_requests().into_iter().find(|r| last_user_text(r).contains("message 6")).unwrap();
    assert!(!system(&before).contains("cello"));

    fake.push_updates(vec![command(7, "/compact")]);
    fake.wait_for(TIMEOUT, |f| f.sent_texts().iter().any(|t| t.contains("Compacted"))).await;
    let n = fake.sent_texts().len();
    fake.push_updates(vec![message(8, json!({"text": "after compaction"}))]);
    fake.wait_for(TIMEOUT, |f| f.sent_texts().len() > n).await;
    let after = fake.llm_requests().into_iter().find(|r| last_user_text(r).contains("after compaction")).unwrap();
    assert!(system(&after).contains("User plays the cello"), "{}", system(&after));
}
