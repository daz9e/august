//! The agent's messenger tools against a fake core: it sees the messengers and their threads,
//! writes to another thread, opens one, uses a messenger's own actions, sends files.

use super::serve;
use august_ext::FakeAugust;
use august_ext::fake::ctx;
use serde_json::json;
use std::path::Path;

async fn messaging(workspace: &Path) -> FakeAugust {
    let (fake, august) = FakeAugust::new(&[("AUGUST_WORKSPACE", workspace.to_str().unwrap())]);
    tokio::spawn(serve(august));
    fake.started().await;
    fake
}

#[tokio::test]
async fn the_agent_sees_the_messengers_their_threads_and_actions() {
    let dir = tempfile::tempdir().unwrap();
    let fake = messaging(dir.path()).await;
    fake.on("messengers", |_| {
        Ok(json!([
            {"id": "cli", "name": "a terminal", "capabilities": {"files_out": true},
             "threads": [{"id": "1", "place": {"kind": "dm"}, "active": true}, {"id": "2", "place": {"kind": "dm"}}]},
            {"id": "telegram", "name": "Telegram", "notes": "Topics are threads.",
             "capabilities": {"buttons": 8, "images": true, "open_thread": true},
             "actions": [{"name": "pin", "description": "Pin a message", "input_schema": {"type": "object"}}],
             "threads": [{"id": "5", "place": {"kind": "group", "title": "Family"}}]}
        ]))
    });
    let out = fake.tool("messengers", json!({}), ctx("1")).await.unwrap();
    assert!(out.contains("cli (a terminal; files) threads: 1*, 2"), "{out}");
    assert!(out.contains("telegram (Telegram; buttons, images, open_thread) threads: 5 (group \"Family\")"), "{out}");
    assert!(out.contains("\n  Topics are threads.\n  action pin: Pin a message"), "{out}");
    assert!(out.ends_with("(* = where the user wrote last)"), "{out}");
}

#[tokio::test]
async fn the_agent_writes_to_another_thread() {
    let dir = tempfile::tempdir().unwrap();
    let fake = messaging(dir.path()).await;
    let out = fake.tool("send_message", json!({"messenger": "test", "thread": "2", "text": "ping from window 1"}), ctx("1")).await.unwrap();
    assert_eq!(out, "sent to test:2");
    let sent = fake.sent();
    assert_eq!((sent[0].thread.as_str(), sent[0].text.as_str()), ("test:2", "ping from window 1"));

    // Its own conversation gets the reply; nothing to send there, nor an empty text.
    let own = fake.tool("send_message", json!({"messenger": "test", "thread": "1", "text": "hi"}), ctx("1")).await.unwrap_err();
    assert!(own.to_string().contains("just reply"), "{own}");
    let empty = fake.tool("send_message", json!({"messenger": "test", "thread": "2", "text": "  "}), ctx("1")).await.unwrap_err();
    assert!(empty.to_string().contains("empty"), "{empty}");
    assert_eq!(fake.sent().len(), 1);
}

#[tokio::test]
async fn the_agent_opens_threads_and_uses_a_messengers_actions() {
    let dir = tempfile::tempdir().unwrap();
    let fake = messaging(dir.path()).await;
    fake.on("open_thread", |_| Ok(json!({"messenger": "telegram", "id": "5/77"})));
    let out = fake.tool("open_thread", json!({"messenger": "telegram", "thread": "5", "title": "Trip"}), ctx("1")).await.unwrap();
    assert_eq!(out, "opened thread 5/77");
    assert_eq!(fake.calls("open_thread")[0], json!({"thread": {"messenger": "telegram", "id": "5"}, "title": "Trip"}));

    fake.on("action", |p| match p["args"]["message"].as_str() {
        Some(_) => Ok(json!(null)),
        None => Err(anyhow::anyhow!("missing `message`")),
    });
    let input = |args| json!({"messenger": "telegram", "thread": "5", "action": "pin", "args": args});
    let bad = fake.tool("messenger_action", input(json!({})), ctx("1")).await.unwrap_err();
    assert!(bad.to_string().contains("missing `message`"), "{bad}");
    assert_eq!(fake.tool("messenger_action", input(json!({"message": "1"})), ctx("1")).await.unwrap(), "done");
    assert_eq!(fake.calls("action")[1], json!({"thread": {"messenger": "telegram", "id": "5"}, "action": "pin", "args": {"message": "1"}}));

    fake.on("action", |_| Ok(json!({"pinned": 1})));
    assert_eq!(fake.tool("messenger_action", input(json!({"message": "1"})), ctx("1")).await.unwrap(), r#"{"pinned":1}"#);
}

#[tokio::test]
async fn the_agent_sends_a_file_from_the_workspace() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("chart.png"), b"\x89PNG chart").unwrap();
    let fake = messaging(dir.path()).await;
    let out = fake.tool("send_file", json!({"path": "chart.png", "caption": "Here it is"}), ctx("1")).await.unwrap();
    assert_eq!(out, "sent chart.png to the chat");
    let sent = fake.sent();
    assert_eq!((sent[0].thread.as_str(), sent[0].text.as_str()), ("test:1", "Here it is"));
    assert_eq!(sent[0].files, [dir.path().join("chart.png").canonicalize().unwrap().to_str().unwrap()]);

    // Only files of the workspace, and only into a chat.
    let outside = tempfile::NamedTempFile::new().unwrap();
    for path in ["missing.png", ".", outside.path().to_str().unwrap(), "../"] {
        assert!(fake.tool("send_file", json!({"path": path}), ctx("1")).await.is_err(), "{path}");
    }
    let nowhere = json!({"thread": null, "turn": null, "depth": 0});
    let err = fake.tool("send_file", json!({"path": "chart.png"}), nowhere).await.unwrap_err();
    assert!(err.to_string().contains("tell the user the path instead: chart.png"), "{err}");
    assert_eq!(fake.sent().len(), 1);
}
