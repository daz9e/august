//! `steps` against a fake core: below the limit a model call goes as it is; the last one
//! gets no tools and a note to report, and a turn's own limit wins over the setting.

use super::serve;
use august_ext::FakeAugust;
use august_ext::fake::turn_ctx;
use serde_json::{Value, json};

async fn steps() -> FakeAugust {
    let (fake, august) = FakeAugust::new(&[]);
    tokio::spawn(serve(august));
    fake.started().await;
    fake
}

async fn call(fake: &FakeAugust, step: u64, meta: Value) -> Value {
    let data = json!({"step": step, "system": "You are August.", "model": "m", "tools": ["read"]});
    let turn = json!({"id": 1, "conversation": "thread", "show": true, "meta": meta});
    fake.event("llm_call", data, turn_ctx("1", turn)).await.unwrap()
}

#[tokio::test]
async fn the_last_step_gets_no_tools_and_asks_for_a_report() {
    let fake = steps().await;
    fake.set_settings(json!({"max_steps": 3}));
    assert_eq!(call(&fake, 1, json!({})).await["tools"], json!(["read"]));
    let last = call(&fake, 2, json!({})).await;
    assert_eq!(last["tools"], json!([]));
    let system = last["system"].as_str().unwrap();
    assert!(system.starts_with("You are August.") && system.contains("Step limit reached"), "{system}");
}

#[tokio::test]
async fn a_turn_may_set_its_own_limit() {
    let fake = steps().await;
    assert_eq!(call(&fake, 10, json!({})).await["tools"], json!(["read"]));
    assert_eq!(call(&fake, 10, json!({"max_steps": 5})).await["tools"], json!([]));
}
