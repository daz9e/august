//! The terminal messenger: every client of its socket is a thread of its own, numbered from
//! 1, speaking one JSON object per line.

use crate::support::*;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

/// A terminal window: what it got from August, and a way to write to it.
struct Window {
    write: tokio::net::unix::OwnedWriteHalf,
    got: Arc<Mutex<Vec<Value>>>,
    thread: String,
}

impl Window {
    async fn open(core: &Core) -> Window {
        let stream = tokio::net::UnixStream::connect(core.path("august.sock")).await.unwrap();
        let (read, mut write) = stream.into_split();
        write.write_all(b"{\"type\":\"hello\"}\n").await.unwrap();
        let mut lines = tokio::io::BufReader::new(read).lines();
        let hello: Value = serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
        let got: Arc<Mutex<Vec<Value>>> = Arc::default();
        let sink = got.clone();
        tokio::spawn(async move {
            while let Ok(Some(line)) = lines.next_line().await {
                sink.lock().unwrap().push(serde_json::from_str(&line).unwrap());
            }
        });
        Window { write, got, thread: hello["thread"].as_str().unwrap().into() }
    }

    async fn put(&mut self, msg: Value) {
        self.write.write_all(format!("{msg}\n").as_bytes()).await.unwrap();
    }

    fn sent(&self) -> Vec<Value> {
        self.got.lock().unwrap().clone()
    }

    async fn wait(&self, core: &Core, what: &str, f: impl Fn(&[Value]) -> bool) {
        core.wait_until(what, |_| f(&self.sent())).await;
    }

    /// Says `text` and waits for a message containing `expect`.
    async fn ask(&mut self, core: &Core, text: &str, expect: &str) {
        self.put(json!({"type": "text", "text": text})).await;
        self.wait(core, expect, |s| s.iter().any(|m| m["text"].as_str().is_some_and(|t| t.contains(expect)))).await;
    }
}

#[tokio::test]
async fn each_terminal_window_is_its_own_thread() {
    let core = core()
        .model(|req| text(if all_text(req).contains("I am Ann") && last_user_text(req).contains("who am I") { "You are Ann." } else { "Noted." }))
        .terminal()
        .start()
        .await;
    let mut first = Window::open(&core).await;
    let mut second = Window::open(&core).await;
    assert_eq!((first.thread.as_str(), second.thread.as_str()), ("1", "2"));

    first.ask(&core, "I am Ann", "Noted.").await;
    // The second window doesn't share the first one's conversation.
    second.ask(&core, "who am I?", "Noted.").await;
    first.ask(&core, "who am I?", "You are Ann.").await;
    // A turn ends with `idle`.
    first.wait(&core, "idle", |s| s.iter().any(|m| m["type"] == "idle")).await;

    // A closed window frees its number for the next one.
    drop(first);
    settle(50).await;
    assert_eq!(Window::open(&core).await.thread, "1");
}

#[tokio::test]
async fn terminals_send_commands_and_press_buttons() {
    let core = core()
        .terminal()
        .ext("menu", |a| {
            a.needs(&["messaging"]);
            let me = a.clone();
            a.register_command("menu", "", move |_, ctx| {
                let me = me.clone();
                async move {
                    let answer = me.ask(ctx.thread.as_ref().unwrap(), "Tea?", &["Yes".into(), "No".into()], std::time::Duration::from_secs(10)).await?;
                    Ok(Some(format!("picked {}", answer.unwrap_or_default())))
                }
            });
        })
        .start()
        .await;
    let mut w = Window::open(&core).await;
    w.put(json!({"type": "text", "text": "/menu"})).await;
    w.wait(&core, "the question", |s| s.iter().any(|m| m["type"] == "send" && !m["buttons"].as_array().unwrap().is_empty())).await;
    let q = w.sent().into_iter().find(|m| m["type"] == "send" && m["text"].as_str().unwrap().contains("Tea?")).unwrap();
    assert_eq!(q["buttons"][0][1]["label"], "No");
    w.put(json!({"type": "press", "button": q["buttons"][0][1]["id"]})).await;
    w.wait(&core, "the answer", |s| s.iter().any(|m| m["text"] == "picked No")).await;
    // The question was settled with an edit.
    assert!(w.sent().iter().any(|m| m["type"] == "edit" && m["text"].as_str().unwrap().ends_with("→ No")));
}
