//! Skills: the agent fixes an existing skill in place, adds a supporting file and
//! retires the skill, each with the owner's approval.

use crate::support::*;
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::sync::Mutex;
use std::time::Duration;

const TIMEOUT: Duration = Duration::from_secs(30);
const SKILL: &str = "---\nname: deploy\ndescription: Deploy the blog\n---\n1. Run `make deploy`.\n";

/// Tool results since the last user message.
fn steps(req: &Value) -> Vec<String> {
    let msgs = req["messages"].as_array().unwrap();
    let start = msgs.iter().rposition(|m| m["role"] == "user").unwrap();
    msgs[start..].iter().filter(|m| m["role"] == "tool").map(|m| m["content"].as_str().unwrap_or("").to_string()).collect()
}

fn user_text(req: &Value) -> String {
    let m = req["messages"].as_array().unwrap().iter().rev().find(|m| m["role"] == "user").unwrap();
    m["content"].as_str().unwrap_or("").to_string()
}

#[tokio::test]
async fn agent_patches_extends_and_archives_a_skill() {
    let llm: Llm = Box::new(|req| {
        let done = steps(req);
        if user_text(req).contains("retire") {
            return match done.len() {
                0 => reply_tool("edit_skill", json!({"action": "archive", "name": "deploy"})),
                _ => reply_text(&format!("retired: {}", done[0])),
            };
        }
        match done.len() {
            0 => reply_tool("edit_skill", json!({"action": "patch", "name": "deploy", "old": "make deploy", "new": "make release"})),
            1 => reply_tool("edit_skill", json!({"action": "write_file", "name": "deploy", "file": "references/hosts.md", "content": "web1, web2"})),
            2 => reply_tool("load_skill", json!({"name": "deploy"})),
            _ => reply_text(&format!("loaded: {}", done[2])),
        }
    });
    let fake = Fake::start(vec![message(1, json!({"text": "improve the deploy skill"}))], HashMap::new(), Some(llm)).await;
    let gw = spawn_gateway_with_home(&fake, LlmSetup::Fake, &[], &[("skills/deploy/SKILL.md", SKILL)]);

    let pressed = Mutex::new(HashSet::new());
    let approve_all = |f: &Fake| {
        for req in f.calls("sendMessage") {
            let data = req.json()["reply_markup"]["inline_keyboard"][0][0]["callback_data"].as_str().map(String::from);
            if let Some(data) = data.filter(|d| !d.is_empty()) {
                let mut seen = pressed.lock().unwrap();
                if seen.insert(data.clone()) {
                    f.push_updates(vec![button_press(100 + seen.len() as i64, &data)]);
                }
            }
        }
    };
    fake.wait_for(TIMEOUT, |f| {
        approve_all(f);
        f.sent_texts().iter().any(|t| t.contains("loaded:"))
    })
    .await;

    // The approval showed the change; the skill was fixed in place and lists its new file.
    assert!(fake.sent_texts().iter().any(|t| t.contains("- make deploy") && t.contains("+ make release")));
    let loaded = fake.sent_texts().into_iter().find(|t| t.contains("loaded:")).unwrap();
    assert!(loaded.contains("make release") && !loaded.contains("make deploy"), "{loaded}");
    assert!(loaded.contains("references/hosts.md"), "{loaded}");
    assert_eq!(std::fs::read_to_string(gw.home.join("skills/deploy/references/hosts.md")).unwrap(), "web1, web2");

    fake.push_updates(vec![message(2, json!({"text": "retire it"}))]);
    fake.wait_for(TIMEOUT, |f| {
        approve_all(f);
        f.sent_texts().iter().any(|t| t.contains("retired: archived"))
    })
    .await;
    assert!(!gw.home.join("skills/deploy").exists());
    let archived = std::fs::read_dir(gw.home.join("skills/.archive")).unwrap().count();
    assert_eq!(archived, 1);
}
