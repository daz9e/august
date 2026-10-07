//! Sub-agents: the agent hands parts of the work to background sub-agents with a fresh
//! context; their reports come back to the chat as new messages.

use crate::support::*;
use serde_json::{Value, json};

fn msgs(req: &Value) -> &Vec<Value> {
    req["messages"].as_array().unwrap()
}

fn is_child(req: &Value) -> bool {
    msgs(req)[0]["content"].as_str().is_some_and(|s| s.contains("You are a sub-agent"))
}

fn last_user(req: &Value) -> String {
    let m = msgs(req).iter().rev().find(|m| m["role"] == "user").unwrap();
    m["content"].as_str().unwrap_or("").to_string()
}

fn two_delegations() -> Value {
    let call = |id: &str, goal: &str| json!({"id": id, "type": "function", "function": {
        "name": "delegate_task", "arguments": json!({"goal": goal, "context": "be quick"}).to_string()}});
    json!({"choices": [{"message": {"role": "assistant", "content": null, "tool_calls": [
        call("c1", "Research apples"), call("c2", "Research pears")
    ]}, "finish_reason": "tool_calls"}]})
}

#[tokio::test]
async fn subtasks_run_in_the_background_and_report_back() {
    let llm: Llm = Box::new(|req| {
        if is_child(req) {
            let fruit = if last_user(req).contains("apples") { "apples" } else { "pears" };
            return reply_text(&format!("{fruit} are tasty"));
        }
        let last = msgs(req).last().unwrap();
        if last["role"] == "tool" {
            return reply_text("I asked two helpers.");
        }
        let text = last_user(req);
        if text.contains("[Subtask") {
            // Reports that arrive together come in one message.
            let found: Vec<&str> = text.lines().filter(|l| l.contains("tasty")).collect();
            return reply_text(&format!("helpers say: {}", found.join("; ")));
        }
        two_delegations()
    });
    let fake = Fake::llm(llm).await;
    let gw = august(&fake, Setup::default()).await;
    let mut chat = gw.chat().await;
    chat.say("compare fruit").await;
    chat.wait_until("both reports", |c| {
        let t = c.texts();
        ["apples are tasty", "pears are tasty"].iter().all(|r| t.iter().any(|x| x.contains("helpers say") && x.contains(r)))
    })
    .await;
    assert!(chat.texts().iter().any(|t| t.contains("I asked two helpers")));

    let children: Vec<Value> = fake.llm_requests().into_iter().filter(is_child).collect();
    assert_eq!(children.len(), 2);
    for c in &children {
        let all = c.to_string();
        assert!(!all.contains("compare fruit"), "a sub-agent doesn't see the chat");
        assert!(all.contains("be quick"));
        let tools: Vec<&str> = c["tools"].as_array().unwrap().iter().filter_map(|t| t["function"]["name"].as_str()).collect();
        assert!(tools.contains(&"shell") && !tools.contains(&"delegate_task") && !tools.contains(&"remember"), "{tools:?}");
    }
}

#[tokio::test]
async fn failed_subtask_is_reported_to_the_chat() {
    let llm: Llm = Box::new(|req| {
        if is_child(req) {
            return json!({"error": {"message": "child model exploded"}}); // not a completion
        }
        let last = msgs(req).last().unwrap();
        if last["role"] == "tool" {
            return reply_text("delegated");
        }
        let text = last_user(req);
        if text.contains("[Subtask") {
            return reply_text(&format!("relay: {}", text.replace('\n', " ")));
        }
        reply_tool("delegate_task", json!({"goal": "Count the stars"}))
    });
    let fake = Fake::llm(llm).await;
    let gw = august(&fake, Setup::default()).await;
    let mut chat = gw.chat().await;
    chat.say("count stars").await;
    let relay = chat.wait_for("relay:").await.text;
    assert!(relay.contains("Subtask #1 failed: Count the stars"), "{relay}");
}
