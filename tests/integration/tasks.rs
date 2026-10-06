//! Scheduled tasks: a task runs with its skills and script output, in a fresh
//! conversation if asked, can't schedule more tasks, and may stay silent.

use crate::support::*;
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::sync::Mutex;
use std::time::Duration;

const TIMEOUT: Duration = Duration::from_secs(30);
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
    let fake = Fake::start(vec![message(1, json!({"text": "watch it"}))], HashMap::new(), Some(llm)).await;
    let _gw = spawn_gateway_env(&fake, LlmSetup::Fake, &[], &[("skills/probe/SKILL.md", SKILL)], &[("AUGUST_SCHEDULER_TICK", "1")]);

    // The script needs approval when the task is created.
    let pressed = Mutex::new(HashSet::new());
    fake.wait_for(TIMEOUT, |f| {
        for req in f.calls("sendMessage") {
            let data = req.json()["reply_markup"]["inline_keyboard"][0][0]["callback_data"].as_str().map(String::from);
            if let Some(d) = data.filter(|d| d.starts_with("ap:") && pressed.lock().unwrap().insert(d.clone())) {
                f.push_updates(vec![button_press(50, &d)]);
            }
        }
        f.sent_texts().iter().any(|t| t.contains("report:"))
    })
    .await;
    assert!(fake.sent_texts().iter().any(|t| t.contains("echo SCRIPT-OUT")), "approval shows the script");

    let run = fake.llm_requests().into_iter().find(|r| user_texts(r).iter().any(|t| t.contains("Scheduled task"))).unwrap();
    let prompt = user_texts(&run).join("\n");
    assert!(prompt.contains("check the thing") && prompt.contains("PROBE-BODY") && prompt.contains("SCRIPT-OUT"), "{prompt}");
    assert!(!prompt.contains("watch it"), "an isolated task doesn't see the chat");
    let report = fake.sent_texts().into_iter().find(|t| t.contains("report:")).unwrap();
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
    let fake = Fake::start(vec![message(1, json!({"text": "watch quietly"}))], HashMap::new(), Some(llm)).await;
    let _gw = spawn_gateway_env(&fake, LlmSetup::Fake, &[], &[], &[("AUGUST_SCHEDULER_TICK", "1")]);
    fake.wait_for(TIMEOUT, |f| f.llm_requests().iter().any(|r| user_texts(r).last().unwrap().contains("quiet check"))).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(fake.sent_texts().iter().all(|t| !t.contains("SILENT") && !t.contains("nothing changed")), "{:?}", fake.sent_texts());
}
