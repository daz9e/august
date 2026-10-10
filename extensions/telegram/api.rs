//! Minimal Telegram Bot API client (only what August needs).

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde_json::{Value, json};
use std::time::Duration;

#[derive(Clone)]
pub struct Api {
    http: reqwest::Client,
    /// `{host}/bot{token}`: method calls.
    base: String,
    /// `{host}/file/bot{token}`: file downloads.
    file_base: String,
}

#[derive(Deserialize)]
struct Reply {
    ok: bool,
    #[serde(default)]
    result: Value,
    #[serde(default)]
    description: String,
    #[serde(default)]
    parameters: Option<Value>,
}

#[derive(Debug)]
pub struct ApiError(pub String);

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "telegram: {}", self.0)
    }
}
impl std::error::Error for ApiError {}

/// Uploads and downloads may be large; method calls use the client's shorter timeout.
const FILE_TIMEOUT: Duration = Duration::from_secs(300);

impl Api {
    /// `TELEGRAM_API_BASE` points at a self-hosted Bot API server (or a test double).
    pub fn new(token: &str) -> Self {
        let host = std::env::var("TELEGRAM_API_BASE")
            .ok()
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| "https://api.telegram.org".into());
        Self::with_host(&host, token)
    }

    pub fn with_host(host: &str, token: &str) -> Self {
        let host = host.trim_end_matches('/');
        Self {
            http: reqwest::Client::builder()
                .user_agent(august_llm::USER_AGENT)
                .timeout(Duration::from_secs(60))
                .build()
                .expect("http client"),
            base: format!("{host}/bot{token}"),
            file_base: format!("{host}/file/bot{token}"),
        }
    }

    /// Calls a Bot API method with JSON parameters.
    pub async fn call(&self, method: &str, params: Value) -> Result<Value> {
        let url = format!("{}/{method}", self.base);
        self.request(method, || self.http.post(&url).json(&params)).await
    }

    /// Calls a Bot API method that uploads a file in `field`.
    pub async fn upload(&self, method: &str, params: Value, field: &str, name: &str, bytes: &[u8]) -> Result<Value> {
        let url = format!("{}/{method}", self.base);
        self.request(method, || {
            let mut form = reqwest::multipart::Form::new();
            for (k, v) in params.as_object().into_iter().flatten() {
                form = form.text(k.clone(), v.as_str().map_or_else(|| v.to_string(), str::to_string));
            }
            let part = reqwest::multipart::Part::bytes(bytes.to_vec()).file_name(name.to_string());
            self.http.post(&url).timeout(FILE_TIMEOUT).multipart(form.part(field.to_string(), part))
        })
        .await
    }

    /// Downloads a file by its `file_id`.
    pub async fn download(&self, file_id: &str) -> Result<Vec<u8>> {
        let file = self.call("getFile", json!({"file_id": file_id})).await?;
        let path = file["file_path"].as_str().context("getFile: no file_path")?;
        let resp = self
            .http
            .get(format!("{}/{path}", self.file_base))
            .timeout(FILE_TIMEOUT)
            .send()
            .await
            .and_then(|r| r.error_for_status())
            .map_err(no_token)
            .context("telegram: file download failed")?;
        Ok(resp.bytes().await.map_err(no_token)?.to_vec())
    }

    /// Sends a request built by `build`; waits and retries on flood control (429).
    async fn request(&self, method: &str, build: impl Fn() -> reqwest::RequestBuilder) -> Result<Value> {
        for attempt in 0..4 {
            let resp = build()
                .send()
                .await
                .map_err(no_token)
                .with_context(|| format!("telegram {method}: request failed"))?;
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            let reply: Reply = serde_json::from_str(&text)
                .with_context(|| format!("telegram {method}: HTTP {status}: {text}"))?;
            if reply.ok {
                return Ok(reply.result);
            }
            let retry_after = reply
                .parameters
                .as_ref()
                .and_then(|p| p["retry_after"].as_u64());
            if let (Some(secs), true) = (retry_after, attempt < 3) {
                tokio::time::sleep(Duration::from_secs(secs.min(30) + 1)).await;
                continue;
            }
            return Err(ApiError(format!("{method}: {}", reply.description)).into());
        }
        bail!("telegram {method}: gave up after flood-control retries")
    }

    pub async fn get_me(&self) -> Result<Value> {
        self.call("getMe", json!({})).await
    }

    /// Long-polls for updates; `timeout` is the server-side wait in seconds.
    pub async fn get_updates(&self, offset: i64, timeout: u64) -> Result<Vec<Value>> {
        let v = self
            .call(
                "getUpdates",
                json!({
                    "offset": offset,
                    "timeout": timeout,
                    "allowed_updates": ["message", "edited_message", "callback_query", "message_reaction"],
                }),
            )
            .await?;
        Ok(v.as_array().cloned().unwrap_or_default())
    }
}

/// The error without its URL, which holds the bot token and must not reach logs.
fn no_token(e: reqwest::Error) -> reqwest::Error {
    e.without_url()
}

/// Telegram refused the HTML (unbalanced tags, ...): the caller retries as plain text.
pub fn is_parse_error(e: &anyhow::Error) -> bool {
    e.to_string().contains("can't parse entities")
}

pub fn is_not_modified(e: &anyhow::Error) -> bool {
    e.to_string().contains("message is not modified")
}
