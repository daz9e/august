//! How August talks, against a fake core: what the model is told before a turn, and what
//! becomes of a message on its way in (who it is from, what it answers, sent mid-turn).

use super::serve;
use august_ext::FakeAugust;
use august_ext::fake::{ctx, turn_ctx};
use serde_json::{Value, json};

async fn conversation() -> FakeAugust {
    let (fake, august) = FakeAugust::new(&[("AUGUST_WORKSPACE", "/home/me/work")]);
    tokio::spawn(serve(august));
    fake.started().await;
    fake.on("messengers", |_| Ok(json!([{"id": "test", "name": "Telegram", "capabilities": {"markdown": true}}])));
    fake
}

/// `[Mon 2026-10-05 09:30] ` stripped, or a panic if the text doesn't start with a stamp.
fn unstamped(text: &str) -> &str {
    let (stamp, rest) = text.strip_prefix('[').and_then(|t| t.split_once("] ")).unwrap_or_else(|| panic!("no stamp: {text}"));
    assert_eq!(stamp.len(), "Mon 2026-10-05 09:30".len(), "{text}");
    rest
}

fn text(data: &Value) -> &str {
    data["text"].as_str().unwrap()
}

#[tokio::test]
async fn a_turn_starts_with_who_august_is_and_how_its_replies_are_shown() {
    let fake = conversation().await;
    let data = fake.event("before_turn", json!({"system": "Extra rules.", "text": "hello"}), ctx("1")).await.unwrap();
    let system = data["system"].as_str().unwrap();
    assert!(system.starts_with("You are August"), "{system}");
    assert!(system.contains("Your workspace directory is /home/me/work"), "{system}");
    assert!(system.contains("The user reads your replies in Telegram, which renders Markdown"), "{system}");
    assert!(system.ends_with("\nExtra rules."), "the system prompt of others goes on after: {system}");
    assert_eq!(unstamped(text(&data)), "hello");

    fake.on("messengers", |_| Ok(json!([{"id": "test", "name": "a terminal", "capabilities": {"markdown": false}}])));
    let data = fake.event("before_turn", json!({"text": "hello"}), ctx("1")).await.unwrap();
    assert!(data["system"].as_str().unwrap().contains("in a terminal: plain text"), "{data}");
}

#[tokio::test]
async fn the_model_sees_who_a_message_is_from_when_not_the_user() {
    let fake = conversation().await;
    let turn = |show: bool, source: &str| turn_ctx("1", json!({"id": 1, "conversation": "thread", "show": show, "source": source}));
    let data = fake.event("before_turn", json!({"text": "all done"}), turn(true, "subagent")).await.unwrap();
    assert_eq!(unstamped(text(&data)), "[from subagent] all done");
    let data = fake.event("before_turn", json!({"text": "hi"}), turn(true, "user")).await.unwrap();
    assert_eq!(unstamped(text(&data)), "hi");
    // A quiet turn is August talking to itself.
    let data = fake.event("before_turn", json!({"text": "check"}), turn(false, "subagent")).await.unwrap();
    assert_eq!(unstamped(text(&data)), "check");

    let data = fake.event("message_in", json!({"text": "all done", "source": "subagent"}), ctx("1")).await.unwrap();
    assert_eq!(text(&data), "[from subagent] all done");
}

#[tokio::test]
async fn a_reply_shows_the_agent_what_it_answers() {
    let fake = conversation().await;
    let reply = json!({"text": "why?", "source": "user", "reply_to": {"text": "Paris is the capital.\nOf France.", "mine": true}});
    let data = fake.event("message_in", reply, ctx("1")).await.unwrap();
    assert_eq!(text(&data), "(replying to your message)\n> Paris is the capital.\n> Of France.\n\nwhy?");

    let reply = json!({"text": "true?", "source": "user", "reply_to": {"text": "a rumour", "mine": false}});
    let data = fake.event("message_in", reply, ctx("1")).await.unwrap();
    assert_eq!(text(&data), "(replying to a message)\n> a rumour\n\ntrue?");

    // A plain message from the user goes on as it came.
    let data = fake.event("message_in", json!({"text": "hi", "source": "user"}), ctx("1")).await.unwrap();
    assert_eq!(text(&data), "hi");
    assert!(fake.sent().is_empty());
}

#[tokio::test]
async fn a_message_sent_while_august_works_is_acknowledged_and_marked() {
    let fake = conversation().await;
    let data = fake.event("message_in", json!({"text": "use the blue theme", "source": "user", "steer": true}), ctx("1")).await.unwrap();
    assert_eq!(unstamped(text(&data)), "[Sent while you were working] use the blue theme");
    assert_eq!(fake.texts(), ["↪️ Got it, I'll take this into account."]);
    assert_eq!(fake.sent()[0].thread, "test:1");

    // Not from the user: marked with its source, and nobody to acknowledge.
    let data = fake.event("message_in", json!({"text": "all done", "source": "subagent", "steer": true}), ctx("1")).await.unwrap();
    assert_eq!(unstamped(text(&data)), "[Sent while you were working] [from subagent] all done");
    assert_eq!(fake.texts().len(), 1);
}
