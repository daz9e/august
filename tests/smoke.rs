//! Smoke tests of the assembled program: the real `august gateway` with its default
//! extensions, against a fake OpenAI-compatible endpoint behind the `openai` extension. It
//! starts and a message gets a streamed reply. Features are tested at their own layer (tests/core, extensions/*/tests.rs).

use axum::body::Bytes;
use axum::response::{IntoResponse, Response};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

const TIMEOUT: Duration = Duration::from_secs(30);

/// A chat-completions stream: `text` in three pieces, or one tool call.
fn stream(text: Option<&str>, call: Option<(&str, Value)>) -> Response {
    let mut chunks: Vec<Value> = Vec::new();
    if let Some(text) = text {
        let words: Vec<&str> = text.split_inclusive(' ').collect();
        for part in words.chunks(words.len().div_ceil(3)) {
            chunks.push(json!({"choices": [{"delta": {"content": part.concat()}}]}));
        }
        chunks.push(json!({"choices": [{"delta": {}, "finish_reason": "stop"}]}));
    }
    if let Some((name, args)) = call {
        let call = json!({"index": 0, "id": "call_1", "type": "function", "function": {"name": name, "arguments": args.to_string()}});
        chunks.push(json!({"choices": [{"delta": {"tool_calls": [call]}}]}));
        chunks.push(json!({"choices": [{"delta": {}, "finish_reason": "tool_calls"}]}));
    }
    let body: String = chunks.iter().map(|c| format!("data: {c}\n\n")).chain(["data: [DONE]\n\n".to_string()]).collect();
    ([("content-type", "text/event-stream")], body).into_response()
}

/// The fake model: runs `bash` when asked to make a file, reports a tool's result, else greets.
async fn model(uri: axum::http::Uri, body: Bytes) -> Response {
    if uri.path().ends_with("/models") {
        return axum::Json(json!({"data": [{"id": "fake-model"}]})).into_response();
    }
    let req: Value = serde_json::from_slice(&body).unwrap_or_default();
    let last = req["messages"].as_array().and_then(|m| m.last()).cloned().unwrap_or_default();
    if last["role"] == "tool" {
        return stream(Some(&format!("Done: {}", last["content"].as_str().unwrap_or(""))), None);
    }
    if last["content"].to_string().contains("make a file") {
        return stream(None, Some(("bash", json!({"command": "touch made.txt"}))));
    }
    stream(Some("Hello there, my friend."), None)
}

/// `august gateway` on a fresh home and workspace, killed on drop.
struct Gateway {
    child: Child,
    home: PathBuf,
    _dir: tempfile::TempDir,
}

impl Drop for Gateway {
    fn drop(&mut self) {
        self.child.kill().ok();
        self.child.wait().ok();
    }
}

fn env(dir: &Path, url: &str) -> Vec<(String, String)> {
    [
        ("PATH", std::env::var("PATH").unwrap_or_default()),
        ("HOME", dir.display().to_string()),
        ("AUGUST_HOME", dir.join("home").display().to_string()),
        ("AUGUST_WORKSPACE", dir.join("workspace").display().to_string()),
        ("AUGUST_PROVIDER", "openai".into()),
        ("AUGUST_MODEL", "fake-model".into()),
        ("OPENAI_API_KEY", "test".into()),
        ("OPENAI_BASE_URL", format!("{url}/v1")),
        ("AUGUST_OPEN_BROWSER", "0".into()),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v))
    .collect()
}

async fn august() -> Gateway {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, axum::Router::new().fallback(model)).await.ok() });
    let dir = tempfile::tempdir().unwrap();
    let (home, workspace) = (dir.path().join("home"), dir.path().join("workspace"));
    std::fs::create_dir_all(&workspace).unwrap();
    let child = Command::new(env!("CARGO_BIN_EXE_august"))
        .arg("gateway")
        .env_clear()
        .current_dir(dir.path())
        .envs(env(dir.path(), &url))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("start august gateway");
    let socket = home.join("august.sock");
    let start = Instant::now();
    while tokio::net::UnixStream::connect(&socket).await.is_err() {
        assert!(start.elapsed() < TIMEOUT, "August did not listen on {}", socket.display());
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    Gateway { child, home, _dir: dir }
}

#[tokio::test]
async fn it_starts_and_a_message_gets_a_streamed_reply() {
    let gw = august().await;
    let stream = tokio::net::UnixStream::connect(gw.home.join("august.sock")).await.unwrap();
    let (read, mut write) = stream.into_split();
    write.write_all(b"{\"type\":\"hello\"}\n{\"type\":\"text\",\"text\":\"hello\"}\n").await.unwrap();
    let mut lines = tokio::io::BufReader::new(read).lines();
    let mut shown: Vec<String> = Vec::new();
    let read = async {
        while let Ok(Some(line)) = lines.next_line().await {
            let msg: Value = serde_json::from_str(&line).unwrap();
            if let Some(text) = msg["text"].as_str() {
                shown.push(text.to_string());
            }
            if msg["type"] == "idle" && shown.iter().any(|t| t == "Hello there, my friend.") {
                return;
            }
        }
    };
    tokio::time::timeout(TIMEOUT, read).await.unwrap_or_else(|_| panic!("no reply; shown: {shown:?}"));
    // The reply grew in place as it streamed, rather than arriving only at the end.
    let partial = shown.iter().filter(|t| !t.is_empty() && "Hello there, my friend.".starts_with(t.as_str()) && t.len() < 23).count();
    assert!(partial > 0, "{shown:?}");
}
