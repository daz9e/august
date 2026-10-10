//! `clarify` against a fake core: the question goes to the thread with the options as
//! buttons, and the user's press, words or silence come back as the tool's output.

use super::serve;
use august_ext::FakeAugust;
use august_ext::fake::{ctx, turn_ctx};
use serde_json::json;

async fn clarify() -> FakeAugust {
    let (fake, august) = FakeAugust::new(&[]);
    tokio::spawn(serve(august));
    fake.started().await;
    fake
}

/// Asks "Which colour?" in thread 1; the tool's output once answered.
fn ask(fake: &FakeAugust) -> tokio::task::JoinHandle<anyhow::Result<String>> {
    let fake = fake.clone();
    tokio::spawn(async move { fake.tool("clarify", json!({"question": "Which colour?", "options": ["Red", "Blue"]}), ctx("1")).await })
}

#[tokio::test]
async fn the_user_taps_an_answer() {
    let fake = clarify().await;
    let out = ask(&fake);
    let question = fake.wait_sent("Which colour?").await;
    assert_eq!(question.thread, "test:1");
    let labels: Vec<&str> = question.buttons.iter().map(|b| b.label.as_str()).collect();
    assert_eq!(labels, ["Red", "Blue"]);
    fake.press("1", &question.buttons[1].id);
    assert_eq!(out.await.unwrap().unwrap(), "The user chose: Blue");
    // The question is edited to show the answer.
    fake.wait_sent("→ Blue").await;
}

#[tokio::test]
async fn the_user_can_answer_in_their_own_words() {
    let fake = clarify().await;
    let out = ask(&fake);
    fake.wait_sent("Which colour?").await;
    fake.reply("1", "green, actually");
    assert_eq!(out.await.unwrap().unwrap(), "The user answered in their own words: green, actually");
    // A number or a label picks that option.
    let out = ask(&fake);
    fake.wait_until("a second question", |f| f.sent().len() == 2).await;
    fake.reply("1", "1");
    assert_eq!(out.await.unwrap().unwrap(), "The user chose: Red");
}

#[tokio::test(start_paused = true)]
async fn no_answer_in_ten_minutes_says_so() {
    let fake = clarify().await;
    let out = ask(&fake).await.unwrap().unwrap();
    assert_eq!(out, "The user didn't answer within 10 minutes.");
    fake.wait_sent("→ ⌛ no answer").await;
}

#[tokio::test]
async fn a_background_turn_cannot_ask() {
    let fake = clarify().await;
    let task = turn_ctx("1", json!({"id": 7, "conversation": "thread", "show": false, "source": "scheduler", "parent": null, "meta": {}}));
    let err = fake.tool("clarify", json!({"question": "Which?", "options": ["A", "B"]}), task).await.unwrap_err();
    assert!(err.to_string().contains("nobody can answer here"), "{err}");
    assert!(fake.sent().is_empty());
}
