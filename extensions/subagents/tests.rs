//! `delegate_task` against a fake core: it starts a fresh sub-agent turn, answers at once,
//! and hands the sub-agent's report (or failure) back to the chat as a prompt; /stop and
//! /new drop the reports still to come.

use super::serve;
use august_ext::FakeAugust;
use august_ext::fake::turn_ctx;
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Notify;

async fn subagents() -> FakeAugust {
    let (fake, august) = FakeAugust::new(&[]);
    tokio::spawn(serve(august));
    fake.started().await;
    fake
}

/// The user's turn 3 in thread 1.
fn users_turn() -> Value {
    turn_ctx("1", json!({"id": 3, "mode": "visible", "source": null, "parent": null, "meta": {}}))
}

/// Sub-agent turns are numbered from 100 and end as `out(task)` says.
fn run_agents(fake: &FakeAugust, out: impl Fn(&str) -> Value + Send + Sync + 'static) {
    let tasks = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let t = tasks.clone();
    fake.on("turn_start", move |p| {
        let mut tasks = t.lock().unwrap();
        tasks.push(p["turn"]["text"].as_str().unwrap().to_string());
        Ok(json!(99 + tasks.len()))
    });
    fake.on("turn_wait", move |p| {
        let i = p["id"].as_u64().unwrap() as usize - 100;
        Ok(out(&tasks.lock().unwrap()[i]))
    });
}

fn prompts(fake: &FakeAugust) -> Vec<String> {
    fake.calls("prompt").iter().map(|p| p["text"].as_str().unwrap().to_string()).collect()
}

#[tokio::test]
async fn subtasks_run_as_fresh_turns_and_report_back() {
    let fake = subagents().await;
    run_agents(&fake, |task| {
        let fruit = if task.contains("apples") { "apples" } else { "pears" };
        json!({"status": "ok", "reply": format!("{fruit} are tasty")})
    });
    for (n, goal) in [(1, "Research apples"), (2, "Research pears")] {
        let out = fake.tool("delegate_task", json!({"goal": goal, "context": "be quick"}), users_turn()).await.unwrap();
        assert!(out.starts_with(&format!("Started subtask #{n}.")), "{out}");
    }
    fake.wait_call("prompt", 2).await;
    let mut reports = prompts(&fake);
    reports.sort();
    assert_eq!(reports, ["[Subtask #1 finished: Research apples]\napples are tasty", "[Subtask #2 finished: Research pears]\npears are tasty"]);

    for start in fake.calls("turn_start") {
        let turn = &start["turn"];
        assert_eq!(start["thread"], json!({"messenger": "test", "id": "1"}));
        // A fresh conversation that sees only the task, child of the user's turn.
        assert_eq!(turn["mode"], "fresh");
        assert_eq!(turn["parent"], 3);
        assert!(turn["text"].as_str().unwrap().ends_with("\n\nContext:\nbe quick"));
        assert!(turn["system"].as_str().unwrap().starts_with("You are a sub-agent"));
        // Nobody answers a sub-agent's questions, and it can't delegate or change memory.
        let excluded: Vec<&str> = turn["exclude"].as_array().unwrap().iter().filter_map(Value::as_str).collect();
        for tool in ["clarify", "delegate_task", "remember", "schedule_task", "save_skill"] {
            assert!(excluded.contains(&tool), "{tool} not excluded: {excluded:?}");
        }
    }
}

#[tokio::test]
async fn a_failed_subtask_is_reported() {
    let fake = subagents().await;
    run_agents(&fake, |_| json!({"status": "error", "error": "child model exploded"}));
    fake.tool("delegate_task", json!({"goal": "Count the stars"}), users_turn()).await.unwrap();
    fake.wait_call("prompt", 1).await;
    assert_eq!(prompts(&fake), ["[Subtask #1 failed: Count the stars]\nchild model exploded"]);
}

#[tokio::test]
async fn a_task_needs_a_goal() {
    let fake = subagents().await;
    let err = fake.tool("delegate_task", json!({"goal": "  "}), users_turn()).await.unwrap_err();
    assert!(err.to_string().contains("`goal` is empty"), "{err}");
    assert!(fake.calls("turn_start").is_empty());
}

#[tokio::test(start_paused = true)]
async fn stop_and_new_drop_running_subtasks() {
    for event in ["stop", "session_changed"] {
        let fake = subagents().await;
        // The sub-agent works until the user has stopped, then reports cancelled.
        let stopped = Arc::new(Notify::new());
        let s = stopped.clone();
        fake.on("turn_start", |_| Ok(json!(100)));
        fake.on_async("turn_wait", move |_| {
            let s = s.clone();
            async move {
                s.notified().await;
                Ok(json!({"status": "cancelled"}))
            }
        });
        fake.tool("delegate_task", json!({"goal": "Count the stars"}), users_turn()).await.unwrap();
        fake.wait_call("turn_wait", 1).await;
        fake.event(event, json!({}), users_turn()).await.unwrap();
        fake.wait_sent("Dropped 1 running subtask(s); their reports won't arrive.").await;
        stopped.notify_waiters();
        tokio::time::sleep(Duration::from_secs(1)).await;
        assert!(prompts(&fake).is_empty(), "{event}: {:?}", prompts(&fake));

        // Nothing left running: the next stop says nothing.
        fake.event(event, json!({}), users_turn()).await.unwrap();
        assert_eq!(fake.sent().len(), 1);
    }
}
