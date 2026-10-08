//! Skills: the agent fixes an existing skill in place, adds a supporting file and
//! retires the skill, each with the owner's approval.

use crate::support::*;
use serde_json::{Value, json};

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
    let fake = Fake::llm(llm).await;
    let gw = august(&fake, Setup { home: &[("skills/deploy/SKILL.md", SKILL)], ..Default::default() }).await;
    let mut chat = gw.chat().await;
    chat.say("improve the deploy skill").await;
    let loaded = chat.allow_until("loaded:").await.text;

    // The approval showed the change; the skill was fixed in place and lists its new file.
    assert!(chat.texts().iter().any(|t| t.contains("old: make deploy") && t.contains("new: make release")));
    assert!(loaded.contains("make release") && !loaded.contains("make deploy"), "{loaded}");
    assert!(loaded.contains("references/hosts.md"), "{loaded}");
    assert_eq!(std::fs::read_to_string(gw.home.join("skills/deploy/references/hosts.md")).unwrap(), "web1, web2");

    chat.say("retire it").await;
    chat.allow_until("retired: archived").await;
    assert!(!gw.home.join("skills/deploy").exists());
    let archived = std::fs::read_dir(gw.home.join("skills/.archive")).unwrap().count();
    assert_eq!(archived, 1);
}
