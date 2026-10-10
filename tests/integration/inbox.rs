//! Messages sent while the agent is working: they reach the running turn before its
//! next model call, or run right after it; `/queue` keeps one as a separate turn.

use crate::support::*;
use serde_json::{Value, json};
use std::time::Duration;

const MARK: &str = "[Sent while you were working]";

fn msgs(req: &Value) -> &Vec<Value> {
    req["messages"].as_array().unwrap()
}

fn texts(req: &Value) -> Vec<String> {
    msgs(req).iter().filter(|m| m["role"] == "user").filter_map(|m| m["content"].as_str().map(String::from)).collect()
}

/// The model is slow on its first call, so the test can talk to it mid-turn.
fn slow_first(answer: fn(&Value) -> Value) -> Llm {
    let calls = std::sync::atomic::AtomicUsize::new(0);
    Box::new(move |req| {
        if calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
            std::thread::sleep(Duration::from_millis(1_500));
        }
        answer(req)
    })
}

#[tokio::test(flavor = "multi_thread")]
async fn message_during_tool_use_joins_the_running_turn() {
    let llm = slow_first(|req| {
        if msgs(req).last().unwrap()["role"] == "tool" {
            return reply_tool("bash", json!({"command": "echo again"}));
        }
        let last = texts(req).pop().unwrap();
        if last.contains(MARK) {
            reply_text(&format!("done, noted: {}", last.rsplit(MARK).next().unwrap().trim()))
        } else {
            reply_tool("bash", json!({"command": "echo working"}))
        }
    });
    let fake = Fake::llm(llm).await;
    let gw = august(&fake, Setup::default()).await;
    let mut chat = gw.chat().await;
    chat.say("start the job").await;
    fake.wait_for(TIMEOUT, |f| !f.llm_requests().is_empty()).await;
    chat.say("use the blue theme").await;

    chat.wait_for("done, noted: use the blue theme").await;
    assert!(chat.texts().iter().any(|t| t.contains("Got it")));
    // One turn: the message never started its own.
    let reqs = fake.llm_requests();
    assert!(reqs.iter().all(|r| texts(r).iter().filter(|t| t.contains("start the job")).count() == 1));
    assert!(!reqs.iter().any(|r| texts(r).iter().any(|t| t.ends_with("] use the blue theme") && !t.contains(MARK))));
}

#[tokio::test(flavor = "multi_thread")]
async fn message_during_the_final_answer_runs_next_and_queue_keeps_its_own_turn() {
    let llm = slow_first(|req| reply_text(&format!("re: {}", texts(req).pop().unwrap().lines().last().unwrap())));
    let fake = Fake::llm(llm).await;
    let gw = august(&fake, Setup::default()).await;
    let mut chat = gw.chat().await;
    chat.say("first").await;
    fake.wait_for(TIMEOUT, |f| !f.llm_requests().is_empty()).await;
    chat.say("second").await;
    chat.say("/queue third").await;
    chat.wait_until("all three replies", |c| {
        let t = c.texts();
        ["] first", "] second", "] third"].iter().all(|r| t.iter().any(|x| x.contains(r)))
    })
    .await;
    assert!(chat.texts().iter().any(|t| t.contains("Queued")));
    assert_eq!(fake.llm_requests().len(), 3);
}
