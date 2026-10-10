//! `/goal` against a fake core: after each of the user's turns it asks the model whether the
//! goal is reached, then nudges the chat on, reports, or pauses.

use super::serve;
use august_ext::FakeAugust;
use august_ext::fake::ctx;
use serde_json::{Value, json};

async fn goal(env: &[(&str, &str)]) -> FakeAugust {
    let (fake, august) = FakeAugust::new(env);
    tokio::spawn(serve(august));
    fake.started().await;
    fake
}

/// Done once the reply judged says `done_at`.
fn judge(fake: &FakeAugust, done_at: &'static str) {
    fake.on_llm(move |prompt, system| {
        assert!(system.contains("You check whether an AI assistant has reached a goal"));
        Ok(if prompt.contains(done_at) { "DONE" } else { "CONTINUE: more steps needed" }.into())
    });
}

/// The user's turn in thread 1 ended with `reply`.
fn ended(reply: &str) -> Value {
    json!({"status": "ok", "reply": reply, "unattended": false})
}

async fn set(fake: &FakeAugust, text: &str) {
    let reply = fake.command("goal", text, ctx("1")).await.unwrap().unwrap();
    assert_eq!(reply, format!("🎯 Goal set: {text}"));
}

fn prompts(fake: &FakeAugust) -> Vec<String> {
    fake.calls("prompt").iter().map(|p| p["text"].as_str().unwrap().to_string()).collect()
}

#[tokio::test]
async fn goal_keeps_the_agent_working_until_reached() {
    let fake = goal(&[]).await;
    judge(&fake, "step 3");
    set(&fake, "count to three").await;
    assert!(prompts(&fake)[0].starts_with("[New goal] count to three"));

    for n in 1..=3 {
        fake.event("turn_end", ended(&format!("step {n}")), ctx("1")).await.unwrap();
    }
    fake.wait_sent("🎯 Goal reached: count to three").await;
    assert_eq!(fake.calls("llm").len(), 3);
    // Each nudge says what is missing and how much of the budget is used.
    let nudges = prompts(&fake);
    assert_eq!(nudges.len(), 3, "{nudges:?}");
    assert_eq!(nudges[2], "[Goal not reached yet (turn 2 of 20): more steps needed] Keep working towards the goal: count to three");

    // Reached, it is gone: the next turn is not judged.
    fake.event("turn_end", ended("step 4"), ctx("1")).await.unwrap();
    assert_eq!(fake.calls("llm").len(), 3);
}

#[tokio::test]
async fn only_the_users_finished_turns_are_judged() {
    let fake = goal(&[]).await;
    judge(&fake, "never");
    set(&fake, "count to three").await;
    fake.event("turn_end", json!({"status": "ok", "reply": "x", "unattended": true}), ctx("1")).await.unwrap();
    fake.event("turn_end", json!({"status": "cancelled", "unattended": false}), ctx("1")).await.unwrap();
    fake.event("turn_end", ended("step 1"), ctx("2")).await.unwrap();
    assert!(fake.calls("llm").is_empty());
}

#[tokio::test]
async fn goal_pauses_when_it_cannot_be_checked() {
    let fake = goal(&[]).await;
    fake.on("llm", |_| Err(anyhow::anyhow!("judge unavailable")));
    set(&fake, "count to three").await;
    fake.event("turn_end", ended("working"), ctx("1")).await.unwrap();
    let paused = fake.wait_sent("Goal paused").await.text;
    assert!(paused.contains("judge unavailable") && paused.contains("/goal"), "{paused}");
}

#[tokio::test]
async fn goal_pauses_when_its_turns_run_out() {
    let fake = goal(&[("AUGUST_GOAL_TURNS", "2")]).await;
    judge(&fake, "unreachable");
    set(&fake, "never ends").await;
    for n in 1..=2 {
        fake.event("turn_end", ended(&format!("step {n}")), ctx("1")).await.unwrap();
    }
    fake.wait_sent("Goal paused after 2 turns. Still missing: more steps needed").await;
    assert_eq!(prompts(&fake).len(), 2, "the new goal and one nudge");
    fake.event("turn_end", ended("step 3"), ctx("1")).await.unwrap();
    assert_eq!(fake.calls("llm").len(), 2);
}

/// After `drop` the goal of thread 1 is gone: its next turn starts nothing.
async fn assert_dropped(fake: &FakeAugust) {
    fake.event("turn_end", ended("step 2"), ctx("1")).await.unwrap();
    assert!(fake.calls("llm").is_empty());
    assert_eq!(prompts(fake).len(), 1, "only the new goal");
    assert_eq!(fake.command("goal", "", ctx("1")).await.unwrap().unwrap(), "No goal. Set one with /goal <what should be achieved>.");
}

#[tokio::test]
async fn goal_clear_and_new_drop_the_goal() {
    let fake = goal(&[]).await;
    judge(&fake, "unreachable");
    set(&fake, "never ends").await;
    assert_eq!(fake.command("goal", "clear", ctx("1")).await.unwrap().unwrap(), "Goal dropped.");
    assert_dropped(&fake).await;

    let fake = goal(&[]).await;
    judge(&fake, "unreachable");
    set(&fake, "never ends").await;
    fake.event("session_changed", json!({"reason": "new"}), ctx("1")).await.unwrap();
    assert_dropped(&fake).await;
}

#[tokio::test]
async fn stop_drops_the_goal() {
    let fake = goal(&[]).await;
    judge(&fake, "unreachable");
    set(&fake, "never ends").await;
    fake.event("stop", json!({"turns": [1]}), ctx("1")).await.unwrap();
    fake.wait_sent("Goal dropped.").await;
    assert_dropped(&fake).await;
}

#[tokio::test]
async fn goal_pauses_when_a_turn_fails() {
    let fake = goal(&[]).await;
    judge(&fake, "unreachable");
    set(&fake, "count to three").await;
    fake.event("turn_end", json!({"status": "error", "error": "the model is down", "unattended": false}), ctx("1")).await.unwrap();
    let paused = fake.wait_sent("Goal paused: the last turn failed").await.text;
    assert!(paused.contains("the model is down"), "{paused}");
    assert!(fake.calls("llm").is_empty());
}
