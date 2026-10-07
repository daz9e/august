//! `/goal`: the chat keeps working until a judging call says the goal is reached, or
//! the turn budget for the goal runs out.

use crate::support::*;
use serde_json::Value;
use std::time::Duration;

fn is_judge(req: &Value) -> bool {
    req["messages"][0]["content"].as_str().is_some_and(|s| s.contains("You check whether an AI assistant has reached a goal"))
}

/// Main model: "step N" for its N-th reply. Judge: done once a reply says `done_at`.
fn llm(done_at: &'static str) -> Llm {
    Box::new(move |req| {
        let msgs = req["messages"].as_array().unwrap();
        if is_judge(req) {
            let asked = msgs.last().unwrap()["content"].as_str().unwrap_or("");
            return reply_text(if asked.contains(done_at) { "DONE" } else { "CONTINUE: more steps needed" });
        }
        let n = msgs.iter().filter(|m| m["role"] == "assistant").count() + 1;
        reply_text(&format!("step {n}"))
    })
}

#[tokio::test]
async fn goal_keeps_the_agent_working_until_reached() {
    let fake = Fake::llm(llm("step 3")).await;
    let gw = august(&fake, Setup::default()).await;
    let mut chat = gw.chat().await;
    chat.say("/goal count to three").await;
    chat.wait_for("Goal reached").await;

    let texts = chat.texts();
    assert!(texts.iter().any(|t| t.contains("step 3")) && !texts.iter().any(|t| t.contains("step 4")));
    let reqs = fake.llm_requests();
    assert_eq!(reqs.iter().filter(|r| is_judge(r)).count(), 3);
    // Each nudge says what is missing and how much of the budget is used.
    assert!(reqs.iter().any(|r| r.to_string().contains("[Goal not reached yet (turn 2 of 20): more steps needed]")));
}

#[tokio::test]
async fn goal_pauses_when_it_cannot_be_checked() {
    let llm: Llm = Box::new(|req| {
        if is_judge(req) {
            return serde_json::json!({"error": {"message": "judge unavailable"}}); // not a completion
        }
        reply_text("working")
    });
    let fake = Fake::llm(llm).await;
    let gw = august(&fake, Setup::default()).await;
    let mut chat = gw.chat().await;
    chat.say("/goal count to three").await;
    let paused = chat.wait_for("Goal paused").await.text;
    assert!(paused.contains("judge unavailable") && paused.contains("/goal"), "{paused}");
}

#[tokio::test]
async fn goal_pauses_when_its_turns_run_out() {
    let fake = Fake::llm(llm("unreachable")).await;
    let gw = august(&fake, Setup { env: &[("AUGUST_GOAL_TURNS", "2")], ..Default::default() }).await;
    let mut chat = gw.chat().await;
    chat.say("/goal never ends").await;
    chat.wait_for("Goal paused after 2 turns").await;
    assert!(!chat.texts().iter().any(|t| t.contains("step 3")));
}

/// A goal that never completes, on a model slow enough to interrupt.
async fn endless_goal() -> (Fake, Gateway, Chat) {
    let slow: Llm = Box::new(|req| {
        if !is_judge(req) {
            std::thread::sleep(Duration::from_millis(400));
        }
        llm("unreachable")(req)
    });
    let fake = Fake::llm(slow).await;
    let gw = august(&fake, Setup { env: &[("AUGUST_GOAL_TURNS", "100")], ..Default::default() }).await;
    let mut chat = gw.chat().await;
    chat.say("/goal never ends").await;
    chat.wait_for("step 2").await;
    (fake, gw, chat)
}

async fn assert_work_stops(fake: &Fake) {
    tokio::time::sleep(Duration::from_millis(1_200)).await; // a turn already running may finish
    let n = fake.llm_requests().len();
    tokio::time::sleep(Duration::from_millis(1_500)).await;
    assert_eq!(fake.llm_requests().len(), n, "still working on the goal");
}

#[tokio::test(flavor = "multi_thread")]
async fn goal_clear_and_new_drop_the_goal() {
    for (cmd, reply) in [("/goal clear", "Goal dropped."), ("/new", "Started a new conversation.")] {
        let (fake, _gw, mut chat) = endless_goal().await;
        chat.ask(cmd, reply).await;
        assert_work_stops(&fake).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn stop_drops_the_goal() {
    let (fake, _gw, mut chat) = endless_goal().await;
    chat.ask("/stop", "Goal dropped.").await;
    assert_work_stops(&fake).await;
}
