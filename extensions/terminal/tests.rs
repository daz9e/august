//! The terminal against a fake core: windows that connect to its socket are threads `cli:<n>`
//! handing in what is typed; what August sends reaches the window; `august` alone and the
//! shortcuts run its window. And the window itself: how it draws what streams in.

use super::client::{Screen, markdown_line};
use super::{ToAugust, ToWindow, serve};
use august_ext::{Button, FakeAugust};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

async fn terminal() -> (FakeAugust, tempfile::TempDir) {
    let home = tempfile::tempdir().unwrap();
    let (fake, august) = FakeAugust::new(&[("AUGUST_HOME", home.path().to_str().unwrap())]);
    tokio::spawn(serve(august));
    fake.started().await;
    fake.wait_until("the messenger", |f| f.manifest().is_some_and(|m| m["messengers"][0]["id"] == "cli")).await;
    (fake, home)
}

/// A window connected to the terminal: what it got, and a way to write to it.
struct Window {
    write: tokio::net::unix::OwnedWriteHalf,
    got: Arc<Mutex<Vec<Value>>>,
    thread: String,
}

impl Window {
    async fn open(home: &Path) -> Window {
        let socket = home.join("august.sock");
        let mut stream = None;
        for _ in 0..200 {
            if let Ok(s) = tokio::net::UnixStream::connect(&socket).await {
                stream = Some(s);
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let (read, mut write) = stream.expect("the terminal listens").into_split();
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

    async fn put(&mut self, msg: ToAugust) {
        self.write.write_all(format!("{}\n", serde_json::to_string(&msg).unwrap()).as_bytes()).await.unwrap();
    }

    async fn wait(&self, what: &str, f: impl Fn(&[Value]) -> bool) -> Vec<Value> {
        for _ in 0..500 {
            let got = self.got.lock().unwrap().clone();
            if f(&got) {
                return got;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("the window never got {what}: {:?}", self.got.lock().unwrap());
    }
}

fn text(t: &str) -> ToAugust {
    ToAugust::Text { text: t.into(), reply_to: None }
}

#[tokio::test]
async fn each_window_is_a_thread_handing_in_what_is_typed() {
    let (fake, home) = terminal().await;
    let mut first = Window::open(home.path()).await;
    let mut second = Window::open(home.path()).await;
    assert_eq!((first.thread.as_str(), second.thread.as_str()), ("1", "2"));

    first.put(text("hello")).await;
    let hi = fake.wait_call("inbound", 1).await;
    assert_eq!(hi["thread"], json!({"messenger": "cli", "id": "1"}));
    assert_eq!((hi["kind"].as_str(), hi["text"].as_str(), hi["addressed"].as_bool()), (Some("message"), Some("hello"), Some(true)));
    second.put(text("/model x")).await;
    let cmd = fake.wait_call("inbound", 2).await;
    assert_eq!((cmd["thread"]["id"].as_str(), cmd["kind"].as_str(), cmd["name"].as_str(), cmd["args"].as_str()), (Some("2"), Some("command"), Some("model"), Some("x")));
    second.put(ToAugust::Press { button: "b1".into() }).await;
    let press = fake.wait_call("inbound", 3).await;
    assert_eq!((press["kind"].as_str(), press["button"].as_str()), (Some("press"), Some("b1")));

    // A closed window frees its number for the next one.
    drop(first);
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert_eq!(Window::open(home.path()).await.thread, "1");
}

#[tokio::test]
async fn what_august_sends_reaches_the_window() {
    let (fake, home) = terminal().await;
    let w = Window::open(home.path()).await;
    let call = |method: &'static str, params: Value| {
        let mut params = params;
        params["messenger"] = json!("cli");
        params["thread"] = json!("1");
        fake.request(method, params)
    };
    let id = call("messenger_send", json!({"message": {"text": "Tea?", "buttons": [[{"id": "y", "label": "Yes"}]]}})).await.unwrap();
    call("messenger_edit", json!({"id": id, "message": {"text": "Tea?\n→ Yes"}})).await.unwrap();
    call("messenger_presence", json!({"busy": true})).await.unwrap();
    call("messenger_presence", json!({"busy": false})).await.unwrap();
    let got = w.wait("idle", |g| g.iter().any(|m| m["type"] == "idle")).await;
    let kinds: Vec<&str> = got.iter().map(|m| m["type"].as_str().unwrap()).collect();
    assert_eq!(kinds, ["send", "edit", "busy", "idle"]);
    assert_eq!((got[0]["id"].as_str(), got[0]["buttons"][0][0]["label"].as_str()), (id.as_str(), Some("Yes")));
    assert_eq!(got[1]["text"], "Tea?\n→ Yes");
    assert_eq!(fake.request("messenger_threads", json!({"messenger": "cli"})).await.unwrap(), json!(["1"]));
    // A window that isn't open can't be sent to.
    assert!(fake.request("messenger_send", json!({"messenger": "cli", "thread": "9", "message": {"text": "x"}})).await.is_err());
}

#[tokio::test]
async fn august_alone_and_the_shortcuts_open_a_window() {
    let (fake, home) = terminal().await;
    let manifest = fake.manifest().unwrap();
    let cli = manifest["cli"].as_array().unwrap();
    let exec = |name: &str| -> Vec<String> {
        let c = cli.iter().find(|c| c["name"] == name).unwrap_or_else(|| panic!("no `{name}` in {cli:?}"));
        serde_json::from_value(c["exec"].clone()).unwrap()
    };
    let socket = home.path().join("august.sock").display().to_string();
    assert_eq!(exec("")[1..], ["attach".to_string(), socket.clone()]);
    // `august connect telegram` opens a window that starts with `/login telegram`.
    assert_eq!(exec("connect")[1..], ["attach".to_string(), socket, "--".into(), "/login".into()]);
    assert!(PathBuf::from(&exec("")[0]).is_file());
}

/// What the window has drawn for good, as plain text.
fn drawn(screen: &mut Screen) -> Vec<String> {
    screen.take_pending().into_iter().map(|l| l.styled).collect()
}

fn send(id: &str, text: &str, buttons: Vec<Button>) -> ToWindow {
    ToWindow::Send { id: id.into(), text: text.into(), buttons: if buttons.is_empty() { vec![] } else { vec![buttons] }, files: vec![] }
}

fn edit(id: &str, text: &str) -> ToWindow {
    ToWindow::Edit { id: id.into(), text: text.into(), buttons: vec![] }
}

#[test]
fn a_streaming_reply_is_drawn_line_by_line_once() {
    let mut s = Screen::new(false);
    s.show(send("1", "Hel", vec![]));
    assert_eq!(drawn(&mut s), Vec::<String>::new(), "an unfinished line stays live");
    s.show(edit("1", "Hello\nwor"));
    assert_eq!(drawn(&mut s), ["  Hello"]);
    s.show(edit("1", "Hello\nworld\n\n🔧 bash"));
    assert_eq!(drawn(&mut s), ["  world", "  "]);
    s.show(ToWindow::Idle);
    assert_eq!(drawn(&mut s), ["  🔧 bash"]);
    // The next message starts after a blank line.
    s.show(send("2", "Next", vec![]));
    s.show(ToWindow::Idle);
    assert_eq!(drawn(&mut s), ["", "  Next"]);
}

#[test]
fn a_number_answers_the_open_question() {
    let mut s = Screen::new(false);
    let buttons = vec![Button { id: "y".into(), label: "Yes".into() }, Button { id: "n".into(), label: "No".into() }];
    s.show(send("1", "❓ Tea?", buttons));
    assert_eq!(s.submit("2"), ToAugust::Press { button: "n".into() });
    // No question open: a number is just text.
    assert_eq!(s.submit("2"), ToAugust::Text { text: "2".into(), reply_to: None });
    // Settling a question shows its outcome.
    let mut s = Screen::new(false);
    s.show(send("1", "❓ Tea?", vec![Button { id: "y".into(), label: "Yes".into() }]));
    s.show(ToWindow::Idle);
    drawn(&mut s);
    s.show(edit("1", "❓ Tea?\n→ Yes"));
    assert_eq!(drawn(&mut s), ["  → Yes"]);
}

#[test]
fn markdown_is_styled_and_measured_without_its_marks() {
    let plain = |l: &str, code: &mut bool| {
        let line = markdown_line(l, "", code);
        let stripped = unstyled(&line.styled);
        assert_eq!(stripped.chars().count(), line.width, "{l:?}");
        stripped
    };
    let mut code = false;
    assert_eq!(plain("**bold** and `code`", &mut code), "bold and code");
    assert_eq!(plain("## Title", &mut code), "Title");
    assert_eq!(plain("- item", &mut code), "• item");
    assert_eq!(plain("[docs](https://x.y)", &mut code), "docs (https://x.y)");
    assert_eq!(plain("```rust", &mut code), "── rust ");
    assert!(code);
    assert_eq!(plain("let a = **b**;", &mut code), "│ let a = **b**;", "no Markdown inside code");
    plain("```", &mut code);
    assert!(!code);
}

/// `s` without ANSI styling.
fn unstyled(s: &str) -> String {
    let mut out = String::new();
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            for c in chars.by_ref() {
                if c == 'm' {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}
