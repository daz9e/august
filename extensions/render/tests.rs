//! How a visible turn is drawn, against a fake core: the reply streams into a message edited
//! in place, tool calls get a line, a break starts a new message, long replies go on in more
//! messages, and the turn's ending is shown.

use super::serve;
use august_ext::FakeAugust;
use august_ext::fake::ctx;
use serde_json::{Value, json};
use std::time::Duration;

async fn render() -> FakeAugust {
    let (fake, august) = FakeAugust::new(&[]);
    tokio::spawn(serve(august));
    fake.started().await;
    fake
}

/// Draws `events` in thread `thread`, the first being `start` with `caps`.
async fn draw(fake: &FakeAugust, thread: &str, caps: Value, events: &[Value]) {
    fake.event("render", json!({"kind": "start", "capabilities": caps}), ctx(thread)).await.unwrap();
    for e in events {
        fake.event("render", e.clone(), ctx(thread)).await.unwrap();
    }
}

fn live() -> Value {
    json!({"edit": true, "edit_interval_ms": 0, "max_len": 4000})
}

fn text(t: &str) -> Value {
    json!({"kind": "text", "text": t})
}

fn end(status: &str) -> Value {
    json!({"kind": "end", "status": status, "reply": "", "error": "boom"})
}

#[tokio::test]
async fn the_reply_streams_into_one_message_with_a_line_per_tool_call() {
    let fake = render().await;
    let events = [
        text("Let me "),
        text("look."),
        json!({"kind": "tool", "tool": "read", "input": {"path": "src/main.rs", "limit": 5}}),
        json!({"kind": "step"}),
        text("Found it."),
        json!({"kind": "end", "status": "ok", "reply": "Let me look.Found it."}),
    ];
    draw(&fake, "1", live(), &events).await;
    assert_eq!(fake.texts(), ["Let me look.\n\n🔧 `read` `src/main.rs`\n\nFound it."]);
    assert!(!fake.calls("edit").is_empty(), "it was edited in place");
    assert_eq!(fake.sent()[0].thread, "test:1");
}

#[tokio::test]
async fn a_message_sent_mid_reply_lands_in_its_place_and_the_reply_goes_on_below() {
    let fake = render().await;
    let events = [text("Checking."), json!({"kind": "break"}), text("Going with it."), json!({"kind": "end", "status": "ok"})];
    draw(&fake, "1", live(), &events).await;
    assert_eq!(fake.texts(), ["Checking.", "Going with it."]);
}

#[tokio::test(start_paused = true)]
async fn edits_wait_out_the_messengers_interval() {
    let fake = render().await;
    let caps = json!({"edit": true, "edit_interval_ms": 1500});
    draw(&fake, "1", caps, &[text("Hel")]).await;
    assert_eq!(fake.texts(), ["Hel"], "the first text shows at once");
    let t = tokio::time::Instant::now();
    fake.event("render", text("lo"), ctx("1")).await.unwrap();
    assert!(t.elapsed() >= Duration::from_millis(1500), "{:?}", t.elapsed());
    assert_eq!(fake.texts(), ["Hello"]);
}

#[tokio::test]
async fn without_edits_only_the_whole_reply_is_sent() {
    let fake = render().await;
    let caps = json!({"edit": false});
    draw(&fake, "1", caps.clone(), &[text("One, "), text("two.")]).await;
    assert!(fake.sent().is_empty());
    fake.event("render", json!({"kind": "end", "status": "ok", "reply": "One, two."}), ctx("1")).await.unwrap();
    assert_eq!(fake.texts(), ["One, two."]);
    assert!(fake.calls("edit").is_empty());

    // Nothing streamed: the outcome's reply is shown.
    draw(&fake, "2", caps.clone(), &[json!({"kind": "end", "status": "ok", "reply": "done"})]).await;
    draw(&fake, "3", caps, &[json!({"kind": "end", "status": "ok", "reply": ""})]).await;
    assert_eq!(fake.texts()[1..], ["done", "(empty reply)"]);
}

#[tokio::test]
async fn a_long_reply_goes_on_in_more_messages() {
    let fake = render().await;
    let caps = json!({"edit": true, "edit_interval_ms": 0, "max_len": 30});
    let para = "A paragraph of some length.";
    let events = [text(para), text("\n\n"), text(para), text("\n\n"), text(para), end("ok")];
    draw(&fake, "1", caps, &events).await;
    assert_eq!(fake.texts(), [para, para, para]);
}

#[tokio::test]
async fn a_stopped_or_failed_turn_says_so() {
    let fake = render().await;
    draw(&fake, "1", live(), &[text("Half a"), end("cancelled")]).await;
    draw(&fake, "2", live(), &[end("error")]).await;
    assert_eq!(fake.texts(), ["Half a\n\n⏹ Stopped.", "⚠️ Error: boom"]);
}
