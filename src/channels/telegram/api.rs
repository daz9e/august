//! Minimal Telegram Bot API client (only what August needs).

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde_json::{Value, json};
use std::time::Duration;

#[derive(Clone)]
pub struct Api {
    http: reqwest::Client,
    base: String,
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

impl Api {
    /// `TELEGRAM_API_BASE` points at a self-hosted Bot API server (or a test double).
    pub fn new(token: &str) -> Self {
        let host = std::env::var("TELEGRAM_API_BASE")
            .ok()
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| "https://api.telegram.org".into());
        Self::with_base(format!("{}/bot{token}", host.trim_end_matches('/')))
    }

    pub fn with_base(base: String) -> Self {
        Self {
            http: reqwest::Client::builder()
                .user_agent(crate::util::USER_AGENT)
                .timeout(Duration::from_secs(60))
                .build()
                .expect("http client"),
            base,
        }
    }

    /// Calls a Bot API method; waits and retries on flood control (429).
    pub async fn call(&self, method: &str, params: Value) -> Result<Value> {
        for attempt in 0..4 {
            let resp = self
                .http
                .post(format!("{}/{method}", self.base))
                .json(&params)
                .send()
                .await
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
                    "allowed_updates": ["message", "callback_query"],
                }),
            )
            .await?;
        Ok(v.as_array().cloned().unwrap_or_default())
    }
}

/// Telegram refused the HTML (unbalanced tags, ...): the caller retries as plain text.
pub fn is_parse_error(e: &anyhow::Error) -> bool {
    e.to_string().contains("can't parse entities")
}

pub fn is_not_modified(e: &anyhow::Error) -> bool {
    e.to_string().contains("message is not modified")
}
