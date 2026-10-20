//! Model-neutral messages and the two providers: Anthropic (API key) and ChatGPT (Codex login).

pub mod anthropic;
pub mod codex;
mod sse;

use crate::config::Config;
use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    User,
    Assistant,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Block {
    Text { text: String },
    ToolUse { id: String, name: String, input: Value },
    ToolResult { tool_use_id: String, content: String, is_error: bool },
    /// An image file attached by the user, read when the request is built.
    Image { media_type: String, path: String },
    /// Provider-specific items (thinking, reasoning) replayed as-is to the provider that made them.
    Opaque { provider: String, value: Value },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: Vec<Block>,
}

impl Message {
    pub fn user(text: impl Into<String>) -> Self {
        Self { role: Role::User, content: vec![Block::Text { text: text.into() }] }
    }

    pub fn text(&self) -> String {
        let parts: Vec<&str> = self
            .content
            .iter()
            .filter_map(|b| match b {
                Block::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        parts.join("\n")
    }
}

/// The media type of an image file the models accept, by extension.
pub fn image_type(path: &std::path::Path) -> Option<&'static str> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    Some(match ext.as_str() {
        "jpg" | "jpeg" => "image/jpeg",
        "png" => "image/png",
        "gif" => "image/gif",
        "webp" => "image/webp",
        _ => return None,
    })
}

fn image_base64(path: &str) -> Option<String> {
    use base64::Engine;
    std::fs::read(path).ok().map(|b| base64::engine::general_purpose::STANDARD.encode(b))
}

fn missing_image(path: &str) -> String {
    format!("[image {path} is no longer available]")
}

/// A one-line description of server-side tool activity (web search), for progress output.
pub fn activity(block: &Block) -> Option<String> {
    let Block::Opaque { value: v, .. } = block else { return None };
    let query = match v["type"].as_str()? {
        "server_tool_use" => v["input"]["query"].as_str().or(v["input"]["url"].as_str()),
        "web_search_call" => v["action"]["query"].as_str().or(v["action"]["url"].as_str()),
        _ => return None,
    };
    Some(format!("web_search: {}", query.unwrap_or_default()))
}

pub struct ToolSpec {
    pub name: &'static str,
    pub description: &'static str,
    pub schema: Value,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct Usage {
    /// Uncached input.
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
}

impl std::ops::AddAssign for Usage {
    fn add_assign(&mut self, o: Self) {
        self.input += o.input;
        self.output += o.output;
        self.cache_read += o.cache_read;
        self.cache_write += o.cache_write;
    }
}

impl Usage {
    /// Everything the model read for this request.
    pub fn context(&self) -> u64 {
        self.input + self.cache_read + self.cache_write
    }
}

/// Dollars for one request, for models billed per token (not the ChatGPT plan).
pub fn cost(provider: &str, model: &str, u: &Usage) -> Option<f64> {
    if provider != "anthropic" {
        return None;
    }
    // $/MTok: input, output, cache read; cache writes cost 1.25x input.
    let (input, output, read) = match model {
        m if m.starts_with("claude-opus-5-5") => (4.0, 20.0, 0.20),
        m if m.starts_with("claude-opus") => (5.0, 25.0, 0.50),
        m if m.starts_with("claude-sonnet-5-5") => (2.0, 10.0, 0.20),
        m if m.starts_with("claude-sonnet-5") => (2.0, 10.0, 0.20),
        m if m.starts_with("claude-sonnet") => (3.0, 15.0, 0.30),
        m if m.starts_with("claude-haiku-5-5") => (0.10, 0.50, 0.01),
        m if m.starts_with("claude-haiku") => (1.0, 5.0, 0.10),
        m if m.starts_with("claude-fable") => (10.0, 50.0, 0.25),
        _ => return None,
    };
    let m = 1_000_000.0;
    Some((u.input as f64 * input + u.output as f64 * output + u.cache_read as f64 * read + u.cache_write as f64 * input * 1.25) / m)
}

#[derive(Debug, Clone, PartialEq)]
pub enum Stop {
    EndTurn,
    ToolUse,
    MaxTokens,
    Other(String),
}

pub struct Completion {
    pub message: Message,
    pub stop: Stop,
    pub usage: Usage,
}

pub type OnText<'a> = &'a mut (dyn FnMut(&str) + Send);

/// One per process: ChatGPT refresh tokens rotate, so the session must not be duplicated.
pub struct Llm {
    http: reqwest::Client,
    codex: tokio::sync::Mutex<Option<codex::Profile>>,
}

impl Llm {
    pub fn new() -> Self {
        Self { http: http_client(), codex: Default::default() }
    }

    pub async fn complete(
        &self,
        cfg: &Config,
        system: &str,
        messages: &[Message],
        tools: &[ToolSpec],
        on_text: OnText<'_>,
    ) -> Result<Completion> {
        match cfg.model.provider.as_str() {
            "anthropic" => {
                let Some(key) = cfg.secrets.anthropic_key.as_deref() else {
                    bail!("no Anthropic API key, run `august login`");
                };
                anthropic::complete(&self.http, key, cfg, system, messages, tools, on_text).await
            }
            "chatgpt" => codex::complete(&self.http, &self.codex, cfg, system, messages, tools, on_text).await,
            "" => bail!("no provider configured, run `august login`"),
            p => bail!("unknown provider {p:?}"),
        }
    }
}

impl Llm {
    /// Models the configured provider offers.
    pub async fn models(&self, cfg: &Config) -> Result<Vec<String>> {
        match cfg.model.provider.as_str() {
            "anthropic" => anthropic::models(&self.http, cfg.secrets.anthropic_key.as_deref().unwrap_or_default()).await,
            "chatgpt" => codex::models(&self.http, &self.codex).await,
            _ => bail!("no provider configured, run `august login`"),
        }
    }
}

/// Reasoning effort levels the provider accepts.
pub fn efforts(provider: &str) -> &'static [&'static str] {
    match provider {
        "anthropic" => &["low", "medium", "high", "xhigh", "max"],
        _ => &["low", "medium", "high"],
    }
}

pub fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .user_agent(concat!("august/", env!("CARGO_PKG_VERSION")))
        .timeout(std::time::Duration::from_secs(600))
        .build()
        .expect("http client")
}
