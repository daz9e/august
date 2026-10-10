//! A fake HTTP service for the tests of extensions that talk to one (a model API, a search
//! API, the Telegram Bot API, ...): it listens on a free local port, records every request
//! and answers each with what the test's handler returns. Include it in an extension's
//! tests with `#[path = "../fake_http.rs"] mod fake_http;`.

#![allow(dead_code)]

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use serde_json::Value;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// One request the service got.
#[derive(Clone, Debug)]
pub struct Req {
    pub method: String,
    pub path: String,
    /// The raw query string (empty if none).
    pub query: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Req {
    pub fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or(Value::Null)
    }
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
    /// A header's value (`""` if missing); names are case-insensitive.
    pub fn header(&self, name: &str) -> String {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.clone()).unwrap_or_default()
    }
}

/// What the service answers.
pub struct Resp {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    /// Waits this long first (a slow service).
    pub delay: Duration,
}

impl Resp {
    pub fn json(v: Value) -> Resp {
        Resp { status: 200, headers: vec![("content-type".into(), "application/json".into())], body: v.to_string().into_bytes(), delay: Duration::ZERO }
    }
    pub fn text(s: &str) -> Resp {
        Resp { status: 200, headers: vec![("content-type".into(), "text/plain".into())], body: s.as_bytes().to_vec(), delay: Duration::ZERO }
    }
    pub fn bytes(content_type: &str, body: Vec<u8>) -> Resp {
        Resp { status: 200, headers: vec![("content-type".into(), content_type.into())], body, delay: Duration::ZERO }
    }
    /// Server-sent events: one `data:` line per event (with `event:` when it has a `type`).
    pub fn sse(events: &[Value]) -> Resp {
        let body: String = events
            .iter()
            .map(|e| match e["type"].as_str() {
                Some(t) => format!("event: {t}\ndata: {e}\n\n"),
                None => format!("data: {e}\n\n"),
            })
            .collect();
        Resp { status: 200, headers: vec![("content-type".into(), "text/event-stream".into())], body: body.into_bytes(), delay: Duration::ZERO }
    }
    /// A status with a body.
    pub fn status(status: u16, body: &str) -> Resp {
        Resp { status, headers: Vec::new(), body: body.as_bytes().to_vec(), delay: Duration::ZERO }
    }
    pub fn header(mut self, k: &str, v: &str) -> Resp {
        self.headers.push((k.into(), v.into()));
        self
    }
    pub fn after(mut self, delay: Duration) -> Resp {
        self.delay = delay;
        self
    }
}

type Handler = Arc<dyn Fn(&Req) -> Resp + Send + Sync>;

struct Inner {
    log: Mutex<Vec<Req>>,
    handler: Handler,
}

/// The running service; it stops with the test's runtime.
#[derive(Clone)]
pub struct FakeHttp {
    /// `http://127.0.0.1:<port>`.
    pub url: String,
    inner: Arc<Inner>,
}

impl FakeHttp {
    pub async fn start(handler: impl Fn(&Req) -> Resp + Send + Sync + 'static) -> FakeHttp {
        let inner = Arc::new(Inner { log: Mutex::default(), handler: Arc::new(handler) });
        let app = axum::Router::new().fallback(handle).with_state(inner.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.ok() });
        FakeHttp { url, inner }
    }

    /// Every request so far, in order.
    pub fn requests(&self) -> Vec<Req> {
        self.inner.log.lock().unwrap().clone()
    }

    /// Requests whose path ends with `suffix`.
    pub fn to(&self, suffix: &str) -> Vec<Req> {
        self.requests().into_iter().filter(|r| r.path.ends_with(suffix)).collect()
    }
}

async fn handle(State(s): State<Arc<Inner>>, method: Method, uri: Uri, headers: HeaderMap, body: Bytes) -> Response {
    let req = Req {
        method: method.to_string(),
        path: uri.path().to_string(),
        query: uri.query().unwrap_or_default().to_string(),
        headers: headers.iter().map(|(k, v)| (k.to_string(), v.to_str().unwrap_or_default().to_string())).collect(),
        body: body.to_vec(),
    };
    s.log.lock().unwrap().push(req.clone());
    let resp = (s.handler)(&req);
    tokio::time::sleep(resp.delay).await;
    let mut out = (StatusCode::from_u16(resp.status).unwrap_or(StatusCode::OK), resp.body).into_response();
    for (k, v) in resp.headers {
        if let (Ok(k), Ok(v)) = (axum::http::HeaderName::from_bytes(k.as_bytes()), axum::http::HeaderValue::from_str(&v)) {
            out.headers_mut().insert(k, v);
        }
    }
    out
}
