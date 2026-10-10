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
async fn anthropic_answers_with_its_account_key() {
    let fake = Fake::llm(Box::new(|_| reply_text("hello from claude"))).await;
    let settings = json!({"settings": {"base_url": format!("{}/v1", fake.url)}}).to_string();
    let secrets = json!({"anthropic": "sk-old"}).to_string();
    let env = [("AUGUST_PROVIDER", "anthropic"), ("AUGUST_MODEL", "")];
    let home = [("config/extensions/anthropic.json", settings.as_str()), ("secrets/anthropic.json", secrets.as_str())];
    let gw = august(&fake, Setup { home: &home, env: &env, ..Default::default() }).await;
    let mut chat = gw.chat().await;
    chat.ask("hi", "hello from claude").await;
    let call = fake.requests().into_iter().find(|r| r.path == "/v1/messages").unwrap();
    assert_eq!(call.key, "sk-old");
    assert_eq!(call.json()["model"], "claude-opus-5-5");
}

#[tokio::test]
async fn opencode_go_answers_with_its_account_key() {
    let fake = Fake::llm(Box::new(|_| reply_text("hello from opencode"))).await;
    let secrets = json!({"opencode": "oc-old"}).to_string();
    let settings = json!({"settings": {"base_url": format!("{}/v1", fake.url), "format": "chat"}}).to_string();
    let env = [("AUGUST_PROVIDER", "opencode-go"), ("AUGUST_MODEL", "kimi-k2")];
    let home = [("secrets/opencode.json", secrets.as_str()), ("config/extensions/opencode.json", settings.as_str())];
    let gw = august(&fake, Setup { home: &home, env: &env, ..Default::default() }).await;
    let mut chat = gw.chat().await;
    chat.ask("hi", "hello from opencode").await;
    let call = fake.requests().into_iter().find(|r| r.path == "/v1/chat/completions").unwrap();
    assert_eq!(call.key, "oc-old");
    assert_eq!(call.json()["model"], "kimi-k2");
}
