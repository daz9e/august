//! Learning after a turn: a background review replays the conversation with the same
//! system prompt and tools and saves what is worth keeping, after the reply was sent.

use crate::support::*;
use serde_json::{Value, json};
use std::time::Duration;

fn msgs(req: &Value) -> &Vec<Value> {
    req["messages"].as_array().unwrap()
}

fn is_review(req: &Value) -> bool {
    msgs(req).iter().any(|m| m["role"] == "user" && m["content"].as_str().is_some_and(|c| c.starts_with("[Background review]")))
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
    let fake = Fake::llm(llm).await;
    let gw = august(&fake, Setup { env: &[("AUGUST_REVIEW_MEMORY_EVERY", "1")], ..Default::default() }).await;
    let mut chat = gw.chat().await;
    chat.say("stop using bullet lists please").await;

    fake.wait_for(TIMEOUT, |f| f.llm_requests().iter().filter(|r| is_review(r)).count() >= 2).await;
    chat.ask("/memory", "User wants answers without bullet lists").await;

    // The chat is told that something was saved.
    chat.wait_until("a note", |c| c.texts().iter().any(|t| t == "💾 Memory updated")).await;

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
                reply_tool("bash", json!({"command": "echo hi"}))
            };
        }
        reply_text("hello")
    });
    let fake = Fake::llm(llm).await;
    let gw = august(&fake, Setup { env: &[("AUGUST_REVIEW_MEMORY_EVERY", "1")], ..Default::default() }).await;
    let mut chat = gw.chat().await;
    chat.say("hi").await;
    fake.wait_for(TIMEOUT, |f| {
        f.llm_requests().iter().any(|r| is_review(r) && msgs(r).last().unwrap()["role"] == "tool")
    })
    .await;
    let review = fake.llm_requests().into_iter().filter(|r| is_review(r)).last().unwrap();
    let out = msgs(&review).last().unwrap()["content"].as_str().unwrap().to_string();
    assert!(out.contains("not available here"), "{out}");
}

#[tokio::test]
async fn verbose_notes_show_each_change() {
    let llm: Llm = Box::new(|req| {
        if !is_review(req) {
            return reply_text("ok");
        }
        if msgs(req).last().unwrap()["role"] == "tool" {
            reply_text("Saved.")
        } else {
            reply_tool("remember", json!({"fact": "User is a night owl"}))
        }
    });
    let fake = Fake::llm(llm).await;
    let env = [("AUGUST_REVIEW_MEMORY_EVERY", "1"), ("AUGUST_REVIEW_NOTIFY", "verbose")];
    let gw = august(&fake, Setup { env: &env, ..Default::default() }).await;
    let mut chat = gw.chat().await;
    chat.say("I work best after midnight").await;
    chat.wait_until("a note", |c| c.texts().iter().any(|t| t == "💾 remembered: User is a night owl")).await;
}

#[tokio::test]
async fn enough_tool_calls_trigger_a_skill_review_that_writes_a_skill() {
    let llm: Llm = Box::new(|req| {
        let last = msgs(req).last().unwrap();
        if is_review(req) {
            let ask = msgs(req).iter().rev().find(|m| m["role"] == "user" && m["content"].as_str().is_some_and(|c| c.starts_with("[Background review]"))).unwrap();
            let ask = ask["content"].as_str().unwrap();
            assert!(ask.contains("Skills:") && !ask.contains("Memory:"), "{ask}");
            return if last["role"] == "tool" {
                reply_text("Saved.")
            } else {
                reply_tool("save_skill", json!({"name": "greeting", "description": "How to greet", "body": "Say hi twice."}))
            };
        }
        if last["role"] == "tool" { reply_text("done") } else { reply_tool("bash", json!({"command": "echo hi"})) }
    });
    let fake = Fake::llm(llm).await;
    let env = [("AUGUST_REVIEW_MEMORY_EVERY", "0"), ("AUGUST_REVIEW_SKILLS_AFTER", "1")];
    let gw = august(&fake, Setup { env: &env, ..Default::default() }).await;
    let mut chat = gw.chat().await;
    chat.say("say hi").await;
    // No approval button: nobody is asked in the background; the chat is told instead.
    chat.wait_until("a note", |c| c.texts().iter().any(|t| t.contains("💾 Skill") && t.contains("greeting"))).await;
    assert!(std::fs::read_to_string(gw.home.join("skills/greeting/SKILL.md")).unwrap().contains("Say hi twice."));
}

#[tokio::test]
async fn scheduled_tasks_do_not_trigger_reviews() {
    let llm: Llm = Box::new(|req| {
        let last = msgs(req).last().unwrap();
        if is_review(req) {
            return reply_text("Nothing to save.");
        }
        let text = msgs(req).iter().rev().find(|m| m["role"] == "user").unwrap()["content"].as_str().unwrap_or("").to_string();
        if text.contains("Scheduled task") {
            return reply_text("task ran");
        }
        if last["role"] == "tool" { reply_text("scheduled") } else { reply_tool("schedule_task", json!({"schedule": "in 1s", "prompt": "ping"})) }
    });
    let fake = Fake::llm(llm).await;
    let env = [("AUGUST_REVIEW_MEMORY_EVERY", "1"), ("AUGUST_SCHEDULER_TICK", "1")];
    let gw = august(&fake, Setup { env: &env, ..Default::default() }).await;
    let mut chat = gw.chat().await;
    chat.say("ping me").await;
    chat.wait_until("a note", |c| c.texts().iter().any(|t| t.contains("task ran"))).await;
    tokio::time::sleep(Duration::from_millis(800)).await;
    // Only the user's own turn was reviewed.
    assert_eq!(fake.llm_requests().iter().filter(|r| is_review(r)).count(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_slow_review_still_reports() {
    // A real model may take longer than a hook's usual 10 s to review.
    let llm: Llm = Box::new(|req| {
        if !is_review(req) {
            return reply_text("ok");
        }
        if msgs(req).last().unwrap()["role"] == "tool" {
            std::thread::sleep(Duration::from_secs(11));
            reply_text("Saved.")
        } else {
            reply_tool("remember", json!({"fact": "User is patient"}))
        }
    });
    let fake = Fake::llm(llm).await;
    let gw = august(&fake, Setup { env: &[("AUGUST_REVIEW_MEMORY_EVERY", "1")], ..Default::default() }).await;
    let mut chat = gw.chat().await;
    chat.say("hi").await;
    chat.wait_until("the review's note", |c| c.texts().iter().any(|t| t == "💾 Memory updated")).await;
}
