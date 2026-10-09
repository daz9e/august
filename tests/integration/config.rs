//! Settings: one file per unit under `AUGUST_HOME/config`, and older homes moved into it.

use crate::support::*;
use serde_json::json;

#[tokio::test]
async fn an_older_home_is_moved_into_one_file_per_unit() {
    let fake = Fake::llm(Box::new(|_| reply_text("hello from the old setup"))).await;
    let providers = json!({"myllm": {"format": "openai", "base_url": format!("{}/v1", fake.url), "model": "my-model"}}).to_string();
    let home = [
        ("config.json", r#"{"provider": "myllm", "model": "my-model"}"#),
        ("providers.json", providers.as_str()),
        ("credentials.json", r#"{"myllm": {"key": "old-key"}}"#),
        ("extensions/.runtime/defaults/web/disabled", ""),
    ];
    // No provider from the environment: the moved settings choose it.
    let env = [("AUGUST_PROVIDER", ""), ("AUGUST_MODEL", "")];
    let gw = august(&fake, Setup { home: &home, env: &env, ..Default::default() }).await;
    let mut chat = gw.chat().await;
    chat.ask("hi", "hello from the old setup").await;
    assert_eq!(fake.llm_requests()[0]["model"], "my-model");

    let read = |p: &str| std::fs::read_to_string(gw.home.join(p)).unwrap_or_default();
    assert!(read("config/august.json").contains("myllm"), "{}", read("config/august.json"));
    let provider = read("config/extensions/openai.json");
    assert!(provider.contains("old-key") && provider.contains("myllm"), "{provider}");
    assert!(!gw.home.join("credentials.json").exists() && gw.home.join("config/.migrated/credentials.json").exists());

    // A paused extension stays paused; the key is shown masked.
    chat.ask("/extensions", "⏸ web").await;
    let key = chat.ask("/config extensions.openai.settings.endpoints.myllm.key", "endpoints.myllm.key").await;
    assert!(key.contains("••••") && !key.contains("old-key"), "{key}");
}

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
