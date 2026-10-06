//! The default `clarify` extension: the agent asks with buttons, the user taps one and the
//! choice comes back to the model.

use crate::support::*;
use serde_json::json;
use std::collections::HashMap;
use std::time::Duration;

#[tokio::test]
async fn agent_asks_with_buttons_and_gets_the_choice() {
    let llm: Llm = Box::new(|req| {
        let last = req["messages"].as_array().unwrap().last().unwrap().clone();
        if last["role"] == "tool" {
            return reply_text(&format!("Noted: {}", last["content"].as_str().unwrap()));
        }
        reply_tool("clarify", json!({"question": "Which colour?", "options": ["Red", "Blue"]}))
    });
    let fake = Fake::start(vec![message(1, json!({"text": "paint it"}))], HashMap::new(), Some(llm)).await;
    let _gw = spawn_gateway(&fake, LlmSetup::Fake, &[]);

    let t = Duration::from_secs(30);
    fake.wait_for(t, |f| f.calls("sendMessage").iter().any(|r| r.text().contains("ap:"))).await;
    let ask = fake.calls("sendMessage").into_iter().find(|r| r.text().contains("ap:")).unwrap().json();
    assert!(ask["text"].as_str().unwrap().contains("Which colour?"));
    let row = &ask["reply_markup"]["inline_keyboard"][0];
    assert_eq!((row[0]["text"].as_str(), row[1]["text"].as_str()), (Some("Red"), Some("Blue")));
    fake.push_updates(vec![button_press(2, row[1]["callback_data"].as_str().unwrap())]);

    fake.wait_for(t, |f| f.sent_texts().iter().any(|t| t.contains("Noted: The user chose: Blue"))).await;
    // The question is edited to show the answer.
    assert!(fake.calls("editMessageText").iter().any(|r| r.text().contains("→ Blue")));
}
