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
    let provider = read("config/providers/myllm.json");
    assert!(provider.contains("old-key") && provider.contains("openai"), "{provider}");
    assert!(!gw.home.join("credentials.json").exists() && gw.home.join("config/.migrated/credentials.json").exists());

    // A paused extension stays paused; the key is shown masked.
    chat.ask("/extensions", "⏸ web").await;
    let key = chat.ask("/config providers.myllm.key", "providers.myllm.key").await;
    assert!(key.contains("••••") && !key.contains("old-key"), "{key}");
}
