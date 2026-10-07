//! Providers of the user's own, from `providers.json`: a known wire format at another address.

use crate::support::*;
use serde_json::json;

#[tokio::test]
async fn a_provider_from_providers_json_answers() {
    let fake = Fake::llm(Box::new(|_| reply_text("hello from my provider"))).await;
    let providers = json!({"myllm": {
        "label": "My LLM", "format": "openai", "base_url": format!("{}/v1", fake.url),
        "key_env": "MYLLM_KEY", "model": "my-model", "context_window": 32000,
    }})
    .to_string();
    let env = [("AUGUST_PROVIDER", "myllm"), ("AUGUST_MODEL", ""), ("MYLLM_KEY", "k")];
    let gw = august(&fake, Setup { home: &[("providers.json", &providers)], env: &env, ..Default::default() }).await;
    let mut chat = gw.chat().await;
    chat.ask("hi", "hello from my provider").await;
    assert_eq!(fake.llm_requests()[0]["model"], "my-model");
    chat.ask("/status", "myllm").await;
}
