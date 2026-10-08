//! The terminal is a messenger of the same August as the others: commands, approvals,
//! sub-agents and their reports work there too.

use crate::support::*;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::time::Duration;

const TIMEOUT: Duration = Duration::from_secs(30);

fn msgs(req: &Value) -> &Vec<Value> {
    req["messages"].as_array().unwrap()
}

fn last_text(req: &Value) -> String {
    let m = msgs(req).last().unwrap();
    m["content"].as_str().map(String::from).unwrap_or_else(|| m["content"].to_string())
}

#[tokio::test]
async fn terminal_chat_runs_approved_tools_commands_and_subagents() {
    let llm: Llm = Box::new(|req| {
        let text = last_text(req);
        let sub_agent = msgs(req)[0]["content"].as_str().is_some_and(|s| s.contains("You are a sub-agent"));
        let said = |needle: &str| msgs(req).iter().rev().take(2).any(|m| m["content"].to_string().contains(needle));
        if said("call it other.txt") {
            reply_text("Understood: other.txt, not made.txt.")
        } else if sub_agent {
            reply_text("Report: counted 3 files.")
        } else if text.contains("Subtask #1 finished") {
            reply_text("The sub-agent counted 3 files.")
        } else if msgs(req).last().unwrap()["role"] == "tool" {
            reply_text(&format!("Done: {}", text.lines().next().unwrap_or("")))
        } else if text.contains("make a file") {
            reply_tool("bash", json!({"command": "touch made.txt"}))
        } else {
            reply_tool("delegate_task", json!({"goal": "count the files"}))
        }
    });
    let fake = Fake::start(vec![], HashMap::new(), Some(llm)).await;
    let mut term = spawn_terminal(&fake).await;
    term.wait_for(TIMEOUT, "> ").await;

    // A risky bash command asks first; `y` answers the approval.
    term.send("make a file");
    term.wait_for(TIMEOUT, "Approval needed").await;
    term.send("y");
    term.wait_for(TIMEOUT, "Done:").await;
    assert!(term.workspace.join("made.txt").exists(), "{}", term.output());
    assert!(term.output().contains("✅ Allowed"), "{}", term.output());

    // Answering an approval in words denies it, and the words reach the agent.
    term.send("make a file again");
    term.wait_for_count(TIMEOUT, "Approval needed", 2).await;
    term.send("no, call it other.txt");
    term.wait_for(TIMEOUT, "Understood: other.txt").await;
    assert!(term.output().contains("❌ Denied"), "{}", term.output());

    // Built-in commands go through the gateway.
    term.send("/status");
    term.wait_for(TIMEOUT, "Model: `openai · fake-model`").await;

    // A sub-agent runs in the background and its report starts a new turn.
    term.send("count them in the background");
    term.wait_for(TIMEOUT, "Started subtask #1").await;
    term.wait_for(TIMEOUT, "The sub-agent counted 3 files.").await;

    term.send("/exit");
    assert!(term.exited(TIMEOUT).await, "{}", term.output());
}

#[tokio::test]
async fn each_terminal_window_is_its_own_thread() {
    let llm: Llm = Box::new(|req| {
        let all = req["messages"].to_string();
        reply_text(if all.contains("I am Ann") && last_text(req).contains("who am I") { "You are Ann." } else { "Noted." })
    });
    let fake = Fake::start(vec![], HashMap::new(), Some(llm)).await;
    let mut first = spawn_terminal(&fake).await;
    first.wait_for(TIMEOUT, "terminal 1").await;
    let mut second = first.another();
    second.wait_for(TIMEOUT, "terminal 2").await;

    first.send("I am Ann");
    first.wait_for(TIMEOUT, "Noted.").await;
    // The second window doesn't share the first one's conversation.
    second.send("who am I?");
    second.wait_for(TIMEOUT, "Noted.").await;
    first.send("who am I?");
    first.wait_for(TIMEOUT, "You are Ann.").await;
    assert!(!second.output().contains("Ann"), "{}", second.output());
}
