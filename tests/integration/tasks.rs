//! Scheduled tasks: a task runs with its skills and script output, in a fresh
//! conversation if asked, can't schedule more tasks, and may stay silent.

use crate::support::*;
use serde_json::{Value, json};
use std::time::Duration;

const SKILL: &str = "---\nname: probe\ndescription: How to probe\n---\nPROBE-BODY\n";

fn msgs(req: &Value) -> &Vec<Value> {
    req["messages"].as_array().unwrap()
}

fn user_texts(req: &Value) -> Vec<String> {
    msgs(req).iter().filter(|m| m["role"] == "user").filter_map(|m| m["content"].as_str().map(String::from)).collect()
}

#[tokio::test]
async fn task_runs_with_skills_and_script_in_its_own_session() {
    let llm: Llm = Box::new(|req| {
        let last = msgs(req).last().unwrap();
        let task = user_texts(req).iter().any(|t| t.contains("Scheduled task"));
        if last["role"] == "tool" {
            let out = last["content"].as_str().unwrap_or("");
            return reply_text(&if task { format!("report: {out}") } else { "scheduled".into() });
        }
        if task {
            return reply_tool("schedule_task", json!({"schedule": "every 1h", "prompt": "more"}));
        }
        reply_tool("schedule_task", json!({
            "schedule": "in 1s", "prompt": "check the thing", "skills": ["probe"],
            "script": "echo SCRIPT-OUT", "isolated": true
        }))
    });
    let fake = Fake::llm(llm).await;
    let setup = Setup { home: &[("skills/probe/SKILL.md", SKILL)], env: &[("AUGUST_SCHEDULER_TICK", "1")], ..Default::default() };
    let gw = august(&fake, setup).await;
    let mut chat = gw.chat().await;
    chat.say("watch it").await;

    // The script needs approval when the task is created.
    let report = chat.allow_until("report:").await.text;
    assert!(chat.texts().iter().any(|t| t.contains("echo SCRIPT-OUT")), "approval shows the script");

    let run = fake.llm_requests().into_iter().find(|r| user_texts(r).iter().any(|t| t.contains("Scheduled task"))).unwrap();
    let prompt = user_texts(&run).join("\n");
    assert!(prompt.contains("check the thing") && prompt.contains("PROBE-BODY") && prompt.contains("SCRIPT-OUT"), "{prompt}");
    assert!(!prompt.contains("watch it"), "an isolated task doesn't see the chat");
    assert!(report.contains("tasks can't be managed from a scheduled task"), "{report}");
}

#[tokio::test]
async fn silent_task_sends_nothing() {
    let llm: Llm = Box::new(|req| {
        let last = msgs(req).last().unwrap();
        if user_texts(req).last().unwrap().contains("Scheduled task") {
            return reply_text("[SILENT] nothing changed");
        }
        if last["role"] == "tool" {
            return reply_text("ok, will watch");
        }
        reply_tool("schedule_task", json!({"schedule": "in 1s", "prompt": "quiet check"}))
    });
    let fake = Fake::llm(llm).await;
    let gw = august(&fake, Setup { env: &[("AUGUST_SCHEDULER_TICK", "1")], ..Default::default() }).await;
    let mut chat = gw.chat().await;
    chat.say("watch quietly").await;
    fake.wait_for(TIMEOUT, |f| f.llm_requests().iter().any(|r| user_texts(r).last().unwrap().contains("quiet check"))).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(chat.texts().iter().all(|t| !t.contains("SILENT") && !t.contains("nothing changed")), "{:?}", chat.texts());
}
