//! Shared test harness: a fake Telegram Bot API (and optionally a fake
//! OpenAI-compatible LLM) on one local HTTP server, and the real `august gateway`
//! binary running against it.

#![allow(dead_code)]

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{Method, Uri};
use axum::response::{IntoResponse, Response};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub const TOKEN: &str = "TEST";
pub const OWNER: i64 = 7;
pub const CHAT: i64 = 5;

/// One request the fake server received.
#[derive(Clone)]
pub struct Req {
    pub path: String,
    pub body: Vec<u8>,
}

impl Req {
    pub fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or(Value::Null)
    }
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
    /// The Bot API method, e.g. `sendMessage`.
    pub fn method(&self) -> &str {
        self.path.rsplit('/').next().unwrap_or("")
    }
}

/// Answers a chat-completions request body with a full (non-streamed) response.
pub type Llm = Box<dyn Fn(&Value) -> Value + Send + Sync>;

struct Inner {
    log: Mutex<Vec<Req>>,
    updates: Mutex<Vec<Value>>,
    files: HashMap<String, Vec<u8>>,
    llm: Option<Llm>,
    delivered: AtomicBool,
    next_id: AtomicI64,
}

pub struct Fake {
    pub url: String,
    inner: Arc<Inner>,
}

impl Fake {
    /// `updates` are delivered on the first `getUpdates`; `files` maps a file id to
    /// its contents for `getFile` + download.
    pub async fn start(updates: Vec<Value>, files: HashMap<String, Vec<u8>>, llm: Option<Llm>) -> Fake {
        let inner = Arc::new(Inner {
            log: Mutex::default(),
            updates: Mutex::new(updates),
            files,
            llm,
            delivered: AtomicBool::new(false),
            next_id: AtomicI64::new(100),
        });
        let app = axum::Router::new().fallback(handle).with_state(inner.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Fake { url, inner }
    }

    pub fn requests(&self) -> Vec<Req> {
        self.inner.log.lock().unwrap().clone()
    }

    /// Bot API calls of one method, in order.
    pub fn calls(&self, method: &str) -> Vec<Req> {
        self.requests().into_iter().filter(|r| r.path.starts_with("/bot") && r.method() == method).collect()
    }

    pub fn llm_requests(&self) -> Vec<Value> {
        self.requests().iter().filter(|r| r.path.ends_with("/chat/completions")).map(Req::json).collect()
    }

    /// Polls until `done` holds, panicking with the request log after `timeout`.
    pub async fn wait_for(&self, timeout: Duration, done: impl Fn(&Fake) -> bool) {
        let start = Instant::now();
        while !done(self) {
            if start.elapsed() > timeout {
                let log: Vec<String> = self.requests().iter().map(|r| format!("{} {}", r.path, r.text())).collect();
                panic!("timed out; requests so far:\n{}", log.join("\n"));
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
}

fn ok(result: Value) -> Response {
    axum::Json(json!({"ok": true, "result": result})).into_response()
}

async fn handle(State(s): State<Arc<Inner>>, method: Method, uri: Uri, body: Bytes) -> Response {
    let path = uri.path().to_string();
    s.log.lock().unwrap().push(Req { path: path.clone(), body: body.to_vec() });

    if path.ends_with("/chat/completions") {
        let llm = s.llm.as_ref().expect("no fake LLM configured");
        let req: Value = serde_json::from_slice(&body).unwrap();
        return axum::Json(llm(&req)).into_response();
    }
    if let Some(file) = path.strip_prefix(&format!("/file/bot{TOKEN}/")) {
        assert_eq!(method, Method::GET);
        return match s.files.get(file) {
            Some(bytes) => bytes.clone().into_response(),
            None => axum::http::StatusCode::NOT_FOUND.into_response(),
        };
    }
    let Some(api) = path.strip_prefix(&format!("/bot{TOKEN}/")) else {
        return axum::http::StatusCode::NOT_FOUND.into_response();
    };
    let params: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    match api {
        "getMe" => ok(json!({"id": 99, "is_bot": true, "username": "AugustBot"})),
        "getUpdates" => {
            if !s.delivered.swap(true, Ordering::SeqCst) {
                return ok(Value::Array(std::mem::take(&mut *s.updates.lock().unwrap())));
            }
            tokio::time::sleep(Duration::from_millis(300)).await;
            ok(json!([]))
        }
        "getFile" => {
            let id = params["file_id"].as_str().unwrap_or("");
            ok(json!({"file_id": id, "file_path": id}))
        }
        "sendMessage" | "sendPhoto" | "sendDocument" => {
            ok(json!({"message_id": s.next_id.fetch_add(1, Ordering::SeqCst)}))
        }
        _ => ok(json!(true)),
    }
}

/// A private message from the owner.
pub fn message(id: i64, fields: Value) -> Value {
    let mut m = json!({
        "message_id": id,
        "chat": {"id": CHAT, "type": "private"},
        "from": {"id": OWNER, "is_bot": false, "first_name": "Owner"},
        "date": 0,
    });
    for (k, v) in fields.as_object().unwrap() {
        m[k] = v.clone();
    }
    json!({"update_id": id, "message": m})
}

/// Chat-completions answers for the fake LLM.
pub fn reply_text(text: &str) -> Value {
    json!({"choices": [{"message": {"role": "assistant", "content": text}, "finish_reason": "stop"}]})
}

pub fn reply_tool(name: &str, args: Value) -> Value {
    json!({"choices": [{"message": {"role": "assistant", "content": null, "tool_calls": [{
        "id": "call_1", "type": "function", "function": {"name": name, "arguments": args.to_string()}
    }]}, "finish_reason": "tool_calls"}]})
}

/// The `august gateway` process, killed on drop.
pub struct Gateway {
    child: Child,
    pub workspace: PathBuf,
    _dir: tempfile::TempDir,
}

impl Drop for Gateway {
    fn drop(&mut self) {
        self.child.kill().ok();
        self.child.wait().ok();
    }
}

pub enum LlmSetup<'a> {
    /// The fake LLM on the test server.
    Fake,
    /// Whatever provider the developer configured in `home` (`~/.august`).
    Real { home: &'a Path },
}

/// Starts the gateway with a fresh workspace (seeded with `seed` files) talking to
/// the fake Bot API at `fake`.
pub fn spawn_gateway(fake: &Fake, llm: LlmSetup, seed: &[(&str, &[u8])]) -> Gateway {
    let dir = tempfile::tempdir().unwrap();
    let workspace = dir.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    for (name, bytes) in seed {
        std::fs::write(workspace.join(name), bytes).unwrap();
    }
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_august"));
    cmd.arg("gateway")
        .env_clear()
        .current_dir(dir.path()) // no project .env
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("HOME", dir.path())
        .env("AUGUST_WORKSPACE", &workspace)
        .env("TELEGRAM_API_BASE", &fake.url)
        .env("TELEGRAM_BOT_TOKEN", TOKEN)
        .env("TELEGRAM_ALLOWED_USERS", OWNER.to_string())
        .stdout(Stdio::null())
        .stderr(if std::env::var_os("TEST_GATEWAY_LOG").is_some() { Stdio::inherit() } else { Stdio::null() });
    match llm {
        LlmSetup::Fake => {
            let home = dir.path().join("home");
            cmd.env("AUGUST_HOME", &home)
                .env("AUGUST_PROVIDER", "openai")
                .env("AUGUST_MODEL", "fake-model")
                .env("OPENAI_API_KEY", "test")
                .env("OPENAI_BASE_URL", format!("{}/v1", fake.url));
        }
        LlmSetup::Real { home } => {
            cmd.env("AUGUST_HOME", home);
            for (k, v) in std::env::vars().filter(|(k, _)| k.starts_with("AUGUST_") || k.ends_with("_API_KEY")) {
                cmd.env(k, v);
            }
            cmd.env("AUGUST_WORKSPACE", &workspace);
        }
    }
    let child = cmd.spawn().expect("start august gateway");
    Gateway { child, workspace, _dir: dir }
}

pub fn fixture(name: &str) -> Vec<u8> {
    std::fs::read(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name)).unwrap()
}

/// Files in `workspace/inbox`.
pub fn inbox(gw: &Gateway) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(gw.workspace.join("inbox"))
        .map(|d| d.map(|e| e.unwrap().path()).collect())
        .unwrap_or_default();
    v.sort();
    v
}
