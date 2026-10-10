//! Settings: one file per unit under `AUGUST_HOME/config`.

use crate::support::*;
use serde_json::json;

#[tokio::test]
async fn the_agent_lists_reads_and_changes_settings_with_approval() {
    let fake = Fake::llm(Box::new(|req| {
        let messages = req["messages"].as_array().unwrap();
        let last = messages.last().unwrap();
        if last["role"] == "tool" {
            return reply_text(&format!("Result: {}", last["content"].as_str().unwrap_or("")));
        }
        let text = last["content"].to_string();
        let input = if text.contains("list") {
            json!({"action": "list", "path": ""})
        } else if text.contains("stop judging") {
            json!({"action": "set", "path": "extensions.approvals.settings.judge", "value": false})
        } else {
            json!({"action": "set", "path": "august.effort", "value": "high"})
        };
        reply_tool("config", input)
    }))
    .await;
    let gw = august(&fake, Setup::default()).await;
    let mut chat = gw.chat().await;

    // August's own keys and every extension's, with descriptions, defaults and values.
    let list = chat.ask("list the settings", "Result:").await;
    assert!(list.contains("august.model = ") && list.contains("— The active model"), "{list}");
    assert!(list.contains("extensions.approvals.settings.judge = null (default true) — Let a separate model call"), "{list}");

    // Changing one is the user's call: approvals asks first.
    chat.say("stop judging commands").await;
    let ask = chat.question().await;
    assert!(ask.text.contains("extensions.approvals.settings.judge"), "{}", ask.text);
    chat.press(&ask.button("Allow")).await;
    chat.wait_for("Result: extensions.approvals.settings.judge = false").await;
    let unit = std::fs::read_to_string(gw.home.join("config/extensions/approvals.json")).unwrap();
    assert!(unit.contains(r#""judge": false"#), "{unit}");

    // Its own settings the same way.
    chat.say("think harder").await;
    chat.press(&chat.question().await.button("Allow")).await;
    chat.wait_for("Result: august.effort = \"high\"").await;
    let own = std::fs::read_to_string(gw.home.join("config/august.json")).unwrap();
    assert!(own.contains(r#""effort": "high""#), "{own}");
}
