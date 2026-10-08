//! Providers of the user's own: endpoints in the `openai` extension's settings.

use crate::support::*;
use serde_json::json;

#[tokio::test]
async fn an_endpoint_of_the_openai_extension_answers() {
    let fake = Fake::llm(Box::new(|_| reply_text("hello from my provider"))).await;
    let providers = json!({"settings": {"endpoints": {"myllm": {
        "label": "My LLM", "base_url": format!("{}/v1", fake.url),
        "key_env": "MYLLM_KEY", "model": "my-model", "context_window": 32000,
    }}}})
    .to_string();
    let env = [("AUGUST_PROVIDER", "myllm"), ("AUGUST_MODEL", ""), ("MYLLM_KEY", "k")];
    let gw = august(&fake, Setup { home: &[("config/extensions/openai.json", &providers)], env: &env, ..Default::default() }).await;
    let mut chat = gw.chat().await;
    chat.ask("hi", "hello from my provider").await;
    assert_eq!(fake.llm_requests()[0]["model"], "my-model");
    chat.ask("/status", "myllm").await;
}

#[tokio::test]
async fn anthropic_answers_with_the_key_an_older_home_saved() {
    let fake = Fake::llm(Box::new(|_| reply_text("hello from claude"))).await;
    let old = json!({"key": "sk-old", "base_url": format!("{}/v1", fake.url)}).to_string();
    let env = [("AUGUST_PROVIDER", "anthropic"), ("AUGUST_MODEL", "")];
    let gw = august(&fake, Setup { home: &[("config/providers/anthropic.json", &old)], env: &env, ..Default::default() }).await;
    let mut chat = gw.chat().await;
    chat.ask("hi", "hello from claude").await;
    let call = fake.requests().into_iter().find(|r| r.path == "/v1/messages").unwrap();
    assert_eq!(call.key, "sk-old");
    assert_eq!(call.json()["model"], "claude-opus-5-5");
    // The key now lives in the extension's settings.
    let moved: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(gw.home.join("config/extensions/anthropic.json")).unwrap()).unwrap();
    assert_eq!(moved["settings"]["key"], "sk-old");
}

#[tokio::test]
async fn opencode_go_answers_with_the_key_an_older_home_saved() {
    let fake = Fake::llm(Box::new(|_| reply_text("hello from opencode"))).await;
    let old = json!({"key": "oc-old"}).to_string();
    let settings = json!({"settings": {"base_url": format!("{}/v1", fake.url), "format": "chat"}}).to_string();
    let env = [("AUGUST_PROVIDER", "opencode-go"), ("AUGUST_MODEL", "kimi-k2")];
    let home = [("config/providers/opencode-go.json", old.as_str()), ("config/extensions/opencode.json", settings.as_str())];
    let gw = august(&fake, Setup { home: &home, env: &env, ..Default::default() }).await;
    let mut chat = gw.chat().await;
    chat.ask("hi", "hello from opencode").await;
    let call = fake.requests().into_iter().find(|r| r.path == "/v1/chat/completions").unwrap();
    assert_eq!(call.key, "oc-old");
    assert_eq!(call.json()["model"], "kimi-k2");
}
