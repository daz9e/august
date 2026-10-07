//! The default `clarify` extension: the agent asks with buttons, the user taps one and the
//! choice comes back to the model.

use crate::support::*;
use serde_json::json;

#[tokio::test]
async fn agent_asks_with_buttons_and_gets_the_choice() {
    let llm: Llm = Box::new(|req| {
        let last = req["messages"].as_array().unwrap().last().unwrap().clone();
        if last["role"] == "tool" {
            return reply_text(&format!("Noted: {}", last["content"].as_str().unwrap()));
        }
        reply_tool("clarify", json!({"question": "Which colour?", "options": ["Red", "Blue"]}))
    });
    let fake = Fake::llm(llm).await;
    let gw = august(&fake, Setup::default()).await;
    let mut chat = gw.chat().await;
    chat.say("paint it").await;

    let ask = chat.question().await;
    assert!(ask.text.contains("Which colour?"));
    let labels: Vec<&str> = ask.buttons.iter().map(|(_, l)| l.as_str()).collect();
    assert_eq!(labels, ["Red", "Blue"]);
    chat.press(&ask.button("Blue")).await;

    chat.wait_for("Noted: The user chose: Blue").await;
    // The question is edited to show the answer.
    chat.wait_for("→ Blue").await;
}

#[tokio::test]
async fn the_user_can_answer_in_their_own_words() {
    let llm: Llm = Box::new(|req| {
        let last = req["messages"].as_array().unwrap().last().unwrap().clone();
        if last["role"] == "tool" {
            return reply_text(&format!("Noted: {}", last["content"].as_str().unwrap()));
        }
        reply_tool("clarify", json!({"question": "Which colour?", "options": ["Red", "Blue"]}))
    });
    let fake = Fake::llm(llm).await;
    let gw = august(&fake, Setup::default()).await;
    let mut chat = gw.chat().await;
    chat.say("paint it").await;
    chat.question().await;
    // Typing instead of pressing answers the open question; it doesn't start a new turn.
    chat.say("green, actually").await;
    chat.wait_for("Noted: The user answered in their own words: green, actually").await;
    assert_eq!(fake.llm_requests().len(), 2);
}

#[tokio::test]
async fn a_background_turn_cannot_ask() {
    let llm: Llm = Box::new(|req| {
        let msgs = req["messages"].as_array().unwrap();
        let last = msgs.last().unwrap().clone();
        let task = msgs.iter().any(|m| m["content"].to_string().contains("Scheduled task"));
        match (task, last["role"] == "tool") {
            (true, true) => reply_text(&format!("task got: {}", last["content"].as_str().unwrap())),
            (true, false) => reply_tool("clarify", json!({"question": "Which?", "options": ["A", "B"]})),
            (false, true) => reply_text("scheduled"),
            (false, false) => reply_tool("schedule_task", json!({"schedule": "in 1s", "prompt": "decide something"})),
        }
    });
    let fake = Fake::llm(llm).await;
    let gw = august(&fake, Setup { env: &[("AUGUST_SCHEDULER_TICK", "1")], ..Default::default() }).await;
    let mut chat = gw.chat().await;
    chat.ask("later, decide something", "scheduled").await;
    chat.wait_for("task got: error: nobody can answer here").await;
    assert!(chat.messages().iter().all(|m| m.buttons.is_empty()), "a question was asked");
}
