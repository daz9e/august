//! Long-term memory: what the agent saves reaches the system prompt of the next session,
//! while the prompt of the running session stays byte-identical (so provider caching works).

use crate::support::*;
use serde_json::{Value, json};

fn system(req: &Value) -> String {
    req["messages"][0]["content"].as_str().unwrap_or("").to_string()
}

fn last_user_text(req: &Value) -> String {
    let msgs = req["messages"].as_array().unwrap();
    let m = msgs.iter().rev().find(|m| m["role"] == "user").unwrap();
    m["content"].as_str().map(String::from).unwrap_or_else(|| m["content"].to_string())
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
    let fake = Fake::llm(llm).await;
    let gw = august(&fake, Setup::default()).await;
    let mut chat = gw.chat().await;
    chat.ask("remember that I drink green tea", "noted").await;
    chat.ask("how are you", "hello").await;

    // Same session: every model call saw the same system prompt, without the new fact.
    let before: Vec<Value> = fake.llm_requests();
    assert_eq!(before.len(), 3);
    assert!(before.iter().all(|r| system(r) == system(&before[0])));
    assert!(!system(&before[0]).contains("green tea"));

    chat.ask("/new", "new conversation").await;
    chat.ask("hi again", "hello").await;

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
    let fake = Fake::llm(llm).await;
    let gw = august(&fake, Setup { env: &[("AUGUST_MEMORY_CHARS", "50")], ..Default::default() }).await;
    let mut chat = gw.chat().await;
    chat.ask("I have a cat", "done: remembered as #1").await;
    chat.ask("I also have a dog", "done: remembered as #2").await;
    let memory = chat.ask("/memory", "#2 User has a cat Murzik and a dog Sharik").await;
    assert!(!memory.contains("#1 User has a cat named"), "{memory}");
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
    let fake = Fake::llm(llm).await;
    let gw = august(&fake, Setup::default()).await;
    let mut chat = gw.chat().await;
    chat.ask("remember that I play the cello", "noted").await;
    // Enough history for /compact to summarise something.
    for i in 2..=6 {
        chat.ask(&format!("message {i}"), "ok").await;
    }
    let before = fake.llm_requests().into_iter().find(|r| last_user_text(r).contains("message 6")).unwrap();
    assert!(!system(&before).contains("cello"));

    chat.ask("/compact", "Compacted").await;
    chat.ask("after compaction", "ok").await;
    let after = fake.llm_requests().into_iter().find(|r| last_user_text(r).contains("after compaction")).unwrap();
    assert!(system(&after).contains("User plays the cello"), "{}", system(&after));
}

#[tokio::test]
async fn facts_saved_by_an_older_version_are_kept() {
    let old = "CREATE TABLE facts (id INTEGER PRIMARY KEY AUTOINCREMENT, text TEXT NOT NULL, created_at INTEGER NOT NULL);
               INSERT INTO facts VALUES (3, 'User drinks green tea', 0), (7, 'User lives in Berlin', 0);";
    let llm: Llm = Box::new(|req| {
        let last = req["messages"].as_array().unwrap().last().unwrap();
        if last["role"] == "tool" { reply_text("noted") } else { reply_tool("remember", json!({"fact": "User has a cat"})) }
    });
    let fake = Fake::llm(llm).await;
    let gw = august(&fake, Setup { db: old, ..Default::default() }).await;
    let mut chat = gw.chat().await;
    chat.ask("/memory", "#7 User lives in Berlin").await;
    // They reach the prompt, and new facts never reuse their ids.
    chat.ask("I have a cat", "noted").await;
    assert!(system(&fake.llm_requests()[0]).contains("#3 User drinks green tea"));
    chat.ask("/memory", "#8 User has a cat").await;
}
