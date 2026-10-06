//! `/goal`: the chat keeps working until a judging call says the goal is reached, or
//! the turn budget for the goal runs out.

use crate::support::*;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::time::Duration;

fn is_judge(req: &Value) -> bool {
    req["messages"][0]["content"].as_str().is_some_and(|s| s.contains("You check whether an AI assistant has reached a goal"))
}

fn goal(id: i64, text: &str) -> Value {
    message(id, json!({"text": format!("/goal {text}"), "entities": [{"type": "bot_command", "offset": 0, "length": 5}]}))
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
    let fake = Fake::start(vec![goal(1, "count to three")], HashMap::new(), Some(llm("step 3"))).await;
    let _gw = spawn_gateway(&fake, LlmSetup::Fake, &[]);
    fake.wait_for(Duration::from_secs(30), |f| f.sent_texts().iter().any(|t| t.contains("Goal reached"))).await;

    let texts = fake.sent_texts();
    assert!(texts.iter().any(|t| t.contains("step 3")) && !texts.iter().any(|t| t.contains("step 4")));
    let reqs = fake.llm_requests();
    assert_eq!(reqs.iter().filter(|r| is_judge(r)).count(), 3);
    assert!(reqs.iter().any(|r| r.to_string().contains("[Goal not reached yet: more steps needed]")));
}

#[tokio::test]
async fn goal_pauses_when_its_turns_run_out() {
    let fake = Fake::start(vec![goal(1, "never ends")], HashMap::new(), Some(llm("unreachable"))).await;
    let _gw = spawn_gateway_env(&fake, LlmSetup::Fake, &[], &[], &[("AUGUST_GOAL_TURNS", "2")]);
    fake.wait_for(Duration::from_secs(30), |f| f.sent_texts().iter().any(|t| t.contains("Goal paused after 2 turns"))).await;
    assert!(!fake.sent_texts().iter().any(|t| t.contains("step 3")));
}

/// A goal that never completes, on a model slow enough to interrupt.
async fn endless_goal() -> (Fake, Gateway) {
    let slow: Llm = Box::new(|req| {
        if !is_judge(req) {
            std::thread::sleep(Duration::from_millis(400));
        }
        llm("unreachable")(req)
    });
    let fake = Fake::start(vec![goal(1, "never ends")], HashMap::new(), Some(slow)).await;
    let gw = spawn_gateway_env(&fake, LlmSetup::Fake, &[], &[], &[("AUGUST_GOAL_TURNS", "100")]);
    fake.wait_for(Duration::from_secs(30), |f| f.sent_texts().iter().any(|t| t.contains("step 2"))).await;
    (fake, gw)
}

fn command(id: i64, text: &str) -> Value {
    message(id, json!({"text": text, "entities": [{"type": "bot_command", "offset": 0, "length": text.split(' ').next().unwrap().len()}]}))
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
        let (fake, _gw) = endless_goal().await;
        fake.push_updates(vec![command(2, cmd)]);
        fake.wait_for(Duration::from_secs(30), |f| f.sent_texts().iter().any(|t| t.contains(reply))).await;
        assert_work_stops(&fake).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn stop_drops_the_goal() {
    let (fake, _gw) = endless_goal().await;
    fake.push_updates(vec![command(2, "/stop")]);
    fake.wait_for(Duration::from_secs(30), |f| f.sent_texts().iter().any(|t| t.contains("Goal dropped."))).await;
    assert_work_stops(&fake).await;
}
