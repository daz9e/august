//! Token usage against a fake core: every model call (`llm_result`) adds up for its session
//! and the day, and `/usage` shows those of this conversation and today.

use super::serve;
use august_ext::FakeAugust;
use august_ext::fake::ctx;
use serde_json::{Value, json};

fn call(session: Option<&str>, input: u64, cache_read: u64, output: u64) -> Value {
    let usage = json!({"inputTokens": input, "outputTokens": output, "cacheReadTokens": cache_read, "cacheWriteTokens": 0});
    json!({"session": session, "usage": usage})
}

#[tokio::test]
async fn usage_sums_every_model_call_of_the_session() {
    let (fake, august) = FakeAugust::new(&[]);
    tokio::spawn(serve(august));
    fake.started().await;
    fake.on("sessions", |_| Ok(json!([{"id": "s2", "bound": ["test:2"]}, {"id": "s1", "bound": ["test:1"]}, {"id": "s0", "bound": []}])));

    // Calls that come at once all count.
    let calls = (0..5).map(|_| fake.event("llm_result", call(Some("s1"), 60, 40, 10), ctx("1")));
    for r in futures_util::future::join_all(calls).await {
        r.unwrap();
    }
    fake.event("llm_result", call(Some("s2"), 100, 0, 5), ctx("2")).await.unwrap();
    // A call outside any session (e.g. a side task) counts for the day only.
    fake.event("llm_result", call(None, 20, 0, 1), ctx("1")).await.unwrap();

    let report = fake.command("usage", "", ctx("1")).await.unwrap().unwrap();
    assert_eq!(
        report,
        "This session: 5 calls · in 300 · cache read 200 · cache write 0 · out 50\n\
         Today, all chats: 7 calls · in 420 · cache read 200 · cache write 0 · out 56"
    );
    let report = fake.command("usage", "", ctx("2")).await.unwrap().unwrap();
    assert!(report.starts_with("This session: 1 calls · in 100 · cache read 0 · cache write 0 · out 5\n"), "{report}");
    let report = fake.command("usage", "", ctx("3")).await.unwrap().unwrap();
    assert!(report.starts_with("This session: 0 calls · in 0"), "a chat without a session: {report}");
}
