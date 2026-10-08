//! Where a turn ends: when its step budget runs out, the model gets one call without tools
//! to report what it did; when the user stops it, nothing it started keeps running.

use crate::support::*;
use serde_json::json;

#[tokio::test]
async fn out_of_steps_the_agent_reports_progress() {
    let llm: Llm = Box::new(|req| {
        let has_tools = req["tools"].as_array().is_some_and(|t| !t.is_empty());
        if has_tools {
            reply_tool("shell", json!({"command": "echo step"}))
        } else {
            reply_text("Did 3 steps; the rest is left for next time.")
        }
    });
    let fake = Fake::llm(llm).await;
    let gw = august(&fake, Setup { env: &[("AUGUST_MAX_STEPS", "3")], ..Default::default() }).await;
    let mut chat = gw.chat().await;
    chat.say("loop forever").await;
    chat.wait_for("Did 3 steps").await;

    let reqs = fake.llm_requests();
    assert_eq!(reqs.len(), 4);
    let last = reqs.last().unwrap().to_string();
    assert!(last.contains("Step limit reached"), "{last}");
}

#[tokio::test]
async fn stop_ends_everything_a_command_started() {
    let llm: Llm = Box::new(|_| reply_tool("shell", json!({"command": "(sleep 2; touch late.txt) & sleep 30"})));
    let fake = Fake::llm(llm).await;
    let gw = august(&fake, Setup::default()).await;
    let mut chat = gw.chat().await;
    chat.say("start a long job").await;
    let ask = chat.question().await;
    chat.press(&ask.button("Allow")).await;
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    chat.ask("/stop", "Stopping").await;

    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    assert!(!gw.workspace.join("late.txt").exists(), "a process of the stopped command kept running");
}

#[tokio::test]
async fn stop_works_with_the_commands_extension_off() {
    let llm: Llm = Box::new(|_| reply_tool("shell", json!({"command": "sleep 30"})));
    let fake = Fake::llm(llm).await;
    let home = [("config/extensions/commands.json", r#"{"enabled": false}"#)];
    let gw = august(&fake, Setup { home: &home, ..Default::default() }).await;
    let mut chat = gw.chat().await;
    chat.ask("/help", "Unknown command /help").await;
    chat.say("start a long job").await;
    let ask = chat.question().await;
    chat.press(&ask.button("Allow")).await;
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    chat.ask("/stop", "Stopped 1 turn(s).").await;
}
