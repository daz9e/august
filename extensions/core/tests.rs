//! `core` against a fake core: the `august` tool passes the agent's `{op, params}` on as a
//! call of the core's table, in the thread and turn it runs in.

use super::serve;
use august_ext::FakeAugust;
use august_ext::fake::{ctx, thread, turn_ctx};
use serde_json::{Value, json};

async fn core() -> FakeAugust {
    let (fake, august) = FakeAugust::new(&[]);
    tokio::spawn(serve(august));
    fake.started().await;
    fake
}

async fn run(fake: &FakeAugust, input: Value, ctx: Value) -> Result<String, String> {
    fake.tool("august", input, ctx).await.map_err(|e| e.to_string())
}

#[tokio::test]
async fn an_operation_runs_in_the_current_thread_and_turn() {
    let fake = core().await;
    fake.on("sessions", |_| Ok(json!([{"name": "side project"}])));
    fake.on("guide", |_| Ok(json!("How the operations work")));

    // JSON results come back pretty, strings as they are.
    let out = run(&fake, json!({"op": "sessions"}), ctx("1")).await.unwrap();
    assert_eq!(serde_json::from_str::<Value>(&out).unwrap(), json!([{"name": "side project"}]));
    assert_eq!(fake.calls("sessions"), vec![json!({"thread": thread("1")})]);
    assert_eq!(run(&fake, json!({"op": "guide"}), ctx("1")).await.unwrap(), "How the operations work");

    // In a turn the call says which, so a new session waits for the turn to end; a thread
    // given in the params wins.
    let turn = json!({"id": 7, "mode": "visible", "source": null, "parent": null});
    run(&fake, json!({"op": "session_new", "params": {"name": "side"}}), turn_ctx("1", turn)).await.unwrap();
    assert_eq!(fake.calls("session_new"), vec![json!({"name": "side", "thread": thread("1"), "from_turn": 7})]);
    run(&fake, json!({"op": "history", "params": {"thread": thread("2")}}), ctx("1")).await.unwrap();
    assert_eq!(fake.calls("history"), vec![json!({"thread": thread("2")})]);
}

#[tokio::test]
async fn a_missing_op_or_a_refusal_is_the_agents_error() {
    let fake = core().await;
    fake.on("model_set", |_| Err(anyhow::anyhow!("blocked: the user denied it")));
    assert!(run(&fake, json!({}), ctx("1")).await.unwrap_err().contains("missing string argument `op`"));
    let err = run(&fake, json!({"op": "model_set", "params": {"model": "x"}}), ctx("1")).await.unwrap_err();
    assert!(err.contains("the user denied it"), "{err}");
}
