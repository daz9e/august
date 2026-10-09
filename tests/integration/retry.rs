//! A model call that fails for a passing reason is tried again; one that keeps failing
//! moves the turn to the fallback model; a lasting failure (a bad key) is not retried.

use crate::support::*;
use serde_json::{Value, json};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

fn rate_limited() -> Value {
    json!({"error": {"message": "Rate limit reached, try again later", "type": "rate_limit"}})
}

const SETTINGS: &str = r#"{"settings": {"delay_ms": 10, "fallback": "openai:backup-model"}}"#;

fn setup() -> Setup<'static> {
    Setup { home: &[("config/extensions/retry.json", SETTINGS)], ..Default::default() }
}

#[tokio::test]
async fn a_passing_failure_is_tried_again() {
    let calls = Arc::new(AtomicUsize::new(0));
    let n = calls.clone();
    let llm: Llm = Box::new(move |_| if n.fetch_add(1, Ordering::SeqCst) < 2 { rate_limited() } else { reply_text("made it") });
    let fake = Fake::llm(llm).await;
    let gw = august(&fake, setup()).await;
    let mut chat = gw.chat().await;
    chat.ask("hi", "made it").await;
    assert_eq!(calls.load(Ordering::SeqCst), 3);
    assert!(fake.llm_requests().iter().all(|r| r["model"] == "fake-model"));
}

#[tokio::test]
async fn a_model_that_keeps_failing_hands_the_turn_to_the_fallback() {
    let llm: Llm = Box::new(|req| if req["model"] == "backup-model" { reply_text("from backup") } else { rate_limited() });
    let fake = Fake::llm(llm).await;
    let gw = august(&fake, setup()).await;
    let mut chat = gw.chat().await;
    chat.ask("hi", "from backup").await;
    let models: Vec<Value> = fake.llm_requests().iter().map(|r| r["model"].clone()).collect();
    assert_eq!(models, ["fake-model", "fake-model", "fake-model", "backup-model"]);
}

#[tokio::test]
async fn a_bad_key_is_not_retried() {
    let llm: Llm = Box::new(|_| json!({"error": {"message": "Incorrect API key provided", "code": "invalid_api_key"}}));
    let fake = Fake::llm(llm).await;
    let gw = august(&fake, setup()).await;
    let mut chat = gw.chat().await;
    chat.ask("hi", "invalid_api_key").await;
    assert_eq!(fake.llm_requests().len(), 1);
}
