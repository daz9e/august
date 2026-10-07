//! The Telegram messenger against a fake Bot API: replies as HTML, questions as inline
//! buttons whose presses come back, the command menu, and strangers turned away. Features
//! themselves are tested through the terminal messenger; this is the adapter.

use crate::support::*;
use serde_json::json;
use std::collections::HashMap;

#[tokio::test]
async fn telegram_carries_replies_buttons_and_commands() {
    let llm: Llm = Box::new(|req| {
        if req["messages"].as_array().unwrap().last().unwrap()["role"] == "tool" {
            reply_text("**Done**")
        } else {
            reply_tool("shell", json!({"command": "touch made.txt"}))
        }
    });
    let fake = Fake::start(vec![message(1, json!({"text": "make a file"}))], HashMap::new(), Some(llm)).await;
    let gw = august(&fake, Setup { telegram: true, ..Default::default() }).await;

    // The approval is a message with inline buttons; pressing one answers it.
    fake.wait_for(TIMEOUT, |f| f.calls("sendMessage").iter().any(|r| r.text().contains("inline_keyboard"))).await;
    let ask = fake.calls("sendMessage").into_iter().find(|r| r.text().contains("inline_keyboard")).unwrap().json();
    assert!(ask["text"].as_str().unwrap().contains("touch made.txt"));
    let row = &ask["reply_markup"]["inline_keyboard"][0];
    assert_eq!((row[0]["text"].as_str(), row[1]["text"].as_str()), (Some("✅ Allow"), Some("❌ Deny")));
    fake.push_updates(vec![button_press(2, row[0]["callback_data"].as_str().unwrap())]);

    // The press is confirmed, the question settled, and the reply rendered as HTML.
    fake.wait_for(TIMEOUT, |f| f.sent_texts().iter().any(|t| t.contains("<b>Done</b>"))).await;
    assert!(gw.workspace.join("made.txt").exists());
    assert!(!fake.calls("answerCallbackQuery").is_empty());
    assert!(fake.sent_texts().iter().any(|t| t.starts_with("✅ Allowed")));

    // The command menu lists built-in and extension commands.
    let menu = fake.calls("setMyCommands");
    assert!(menu.iter().any(|r| r.text().contains("\"new\"") && r.text().contains("\"goal\"")), "{:?}", menu.iter().map(|r| r.text()).collect::<Vec<_>>());
}

#[tokio::test]
async fn strangers_are_turned_away() {
    let stranger = json!({"update_id": 1, "message": {
        "message_id": 1, "date": 0, "text": "hi",
        "chat": {"id": 77, "type": "private"},
        "from": {"id": 77, "is_bot": false, "first_name": "Stranger"},
    }});
    let llm: Llm = Box::new(|_| reply_text("should not happen"));
    let fake = Fake::start(vec![stranger], HashMap::new(), Some(llm)).await;
    let _gw = august(&fake, Setup { telegram: true, ..Default::default() }).await;
    fake.wait_for(TIMEOUT, |f| f.sent_texts().iter().any(|t| t.contains("not authorized"))).await;
    assert!(fake.llm_requests().is_empty());
}

const LATE_COMMAND: &str = r#"export default function (august) {
  setTimeout(() => august.registerCommand("later", { description: "Added later", handler: () => "here" }), 300);
}"#;

#[tokio::test]
async fn commands_added_later_reach_the_menu() {
    if !std::env::var_os("PATH").is_some_and(|p| std::env::split_paths(&p).any(|d| d.join("bun").is_file())) {
        return eprintln!("skipping: bun is not installed");
    }
    let fake = Fake::llm(Box::new(|_| reply_text("ok"))).await;
    let _gw = august(&fake, Setup { telegram: true, home: &[("extensions/late/index.ts", LATE_COMMAND)], ..Default::default() }).await;
    fake.wait_for(TIMEOUT, |f| f.calls("setMyCommands").iter().any(|r| r.text().contains("\"later\""))).await;
}

const REACT: &str = r#"export default function (august) {
  august.on("message_in", async ({ id }, ctx) => { await august.react(ctx.thread, id, "👀"); });
  august.on("reaction", async ({ message, emoji }, ctx) => { await ctx.send(`you reacted ${emoji} to ${message}`); });
}"#;

#[tokio::test]
async fn reactions_go_both_ways() {
    if !std::env::var_os("PATH").is_some_and(|p| std::env::split_paths(&p).any(|d| d.join("bun").is_file())) {
        return eprintln!("skipping: bun is not installed");
    }
    let reaction = json!({"update_id": 2, "message_reaction": {
        "chat": {"id": CHAT, "type": "private"}, "message_id": 40, "date": 0,
        "user": {"id": OWNER, "is_bot": false, "first_name": "Owner"},
        "old_reaction": [], "new_reaction": [{"type": "emoji", "emoji": "👍"}],
    }});
    let fake = Fake::start(vec![message(1, json!({"text": "hi"})), reaction], HashMap::new(), Some(Box::new(|_| reply_text("ok")))).await;
    let _gw = august(&fake, Setup { telegram: true, home: &[("extensions/react/index.ts", REACT)], ..Default::default() }).await;

    // August reacts to the user's message…
    fake.wait_for(TIMEOUT, |f| !f.calls("setMessageReaction").is_empty()).await;
    let set = fake.calls("setMessageReaction")[0].json();
    assert_eq!((set["message_id"].as_i64(), set["reaction"][0]["emoji"].as_str()), (Some(1), Some("👀")));
    // …and hears the user's reaction.
    fake.wait_for(TIMEOUT, |f| f.sent_texts().iter().any(|t| t.contains("you reacted 👍 to 40"))).await;
}
