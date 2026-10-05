//! Provider-neutral conversation types and the `LlmProvider` trait.

pub mod anthropic;
pub mod chatgpt;
pub mod openai;
pub mod opencode;
pub mod providers;
pub mod resilient;
pub mod responses;
pub mod sse;

use anyhow::Result;
use async_trait::async_trait;
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    User,
    Assistant,
}

#[derive(Debug, Clone)]
pub enum Block {
    Text(String),
    ToolUse {
        id: String,
        name: String,
        input: Value,
    },
    ToolResult {
        tool_use_id: String,
        content: String,
        is_error: bool,
    },
    /// An image the user sent, kept on disk (absolute path); providers read and
    /// base64-encode it when building a request.
    Image { media_type: String, path: String },
    /// Provider-specific block (e.g. Anthropic thinking) that must be sent back
    /// unchanged. Providers that don't understand it skip it.
    Opaque(Value),
}

/// Image formats every supported provider accepts.
pub const IMAGE_TYPES: &[&str] = &["image/jpeg", "image/png", "image/gif", "image/webp"];
/// Larger images are not sent to the model (Anthropic's per-image limit).
pub const MAX_IMAGE_BYTES: usize = 5 * 1024 * 1024;

/// The image as a `data:` URL, or `None` if the file is gone.
pub fn image_data_url(media_type: &str, path: &str) -> Option<String> {
    image_base64(path).map(|b64| format!("data:{media_type};base64,{b64}"))
}

pub fn image_base64(path: &str) -> Option<String> {
    use base64::Engine;
    let bytes = std::fs::read(path).ok()?;
    Some(base64::engine::general_purpose::STANDARD.encode(bytes))
}

/// What a provider sends instead of an image whose file has disappeared.
pub fn missing_image(path: &str) -> String {
    format!("[image no longer available: {path}]")
}

#[derive(Debug, Clone)]
pub struct Message {
    pub role: Role,
    pub content: Vec<Block>,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Role::User => "user",
            Role::Assistant => "assistant",
        }
    }
}

impl Block {
    /// Lossless JSON form, used to persist conversations.
    pub fn to_json(&self) -> Value {
        match self {
            Block::Text(t) => serde_json::json!({"type": "text", "text": t}),
            Block::ToolUse { id, name, input } => {
                serde_json::json!({"type": "tool_use", "id": id, "name": name, "input": input})
            }
            Block::ToolResult { tool_use_id, content, is_error } => serde_json::json!({
                "type": "tool_result", "tool_use_id": tool_use_id, "content": content, "is_error": is_error
            }),
            Block::Image { media_type, path } => {
                serde_json::json!({"type": "image", "media_type": media_type, "path": path})
            }
            Block::Opaque(v) => serde_json::json!({"type": "opaque", "value": v}),
        }
    }

    pub fn from_json(v: &Value) -> Option<Block> {
        let s = |k: &str| v[k].as_str().map(str::to_string);
        Some(match v["type"].as_str()? {
            "text" => Block::Text(s("text")?),
            "tool_use" => Block::ToolUse { id: s("id")?, name: s("name")?, input: v["input"].clone() },
            "tool_result" => Block::ToolResult {
                tool_use_id: s("tool_use_id")?,
                content: s("content")?,
                is_error: v["is_error"].as_bool().unwrap_or(false),
            },
            "image" => Block::Image { media_type: s("media_type")?, path: s("path")? },
            "opaque" => Block::Opaque(v["value"].clone()),
            _ => return None,
        })
    }
}

impl Message {
    pub fn from_parts(role: &str, content: &str) -> Option<Message> {
        let role = match role {
            "user" => Role::User,
            "assistant" => Role::Assistant,
            _ => return None,
        };
        let blocks: Vec<Value> = serde_json::from_str(content).ok()?;
        Some(Message { role, content: blocks.iter().filter_map(Block::from_json).collect() })
    }

    pub fn content_json(&self) -> String {
        Value::Array(self.content.iter().map(Block::to_json).collect()).to_string()
    }
}

impl Message {
    pub fn user_text(text: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            content: vec![Block::Text(text.into())],
        }
    }

    pub fn text(&self) -> String {
        self.content
            .iter()
            .filter_map(|b| match b {
                Block::Text(t) => Some(t.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    pub fn tool_uses(&self) -> impl Iterator<Item = (&str, &str, &Value)> {
        self.content.iter().filter_map(|b| match b {
            Block::ToolUse { id, name, input } => Some((id.as_str(), name.as_str(), input)),
            _ => None,
        })
    }
}

#[derive(Debug, Clone)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StopReason {
    EndTurn,
    ToolUse,
    MaxTokens,
    Refusal,
    Other(String),
}

#[derive(Debug, Clone, Default)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
}

#[derive(Debug, Clone)]
pub struct Completion {
    pub message: Message,
    pub stop_reason: StopReason,
    pub usage: Usage,
}

#[async_trait]
pub trait LlmProvider: Send + Sync {
    fn name(&self) -> &str;

    /// `session` is a stable id for the conversation (gateways use it for routing
    /// and prompt caching); providers that don't need it ignore it.

    async fn complete(
        &self,
        session: &str,
        system: &str,
        messages: &[Message],
        tools: &[ToolSpec],
    ) -> Result<Completion>;

    /// Like `complete`, but calls `on_text` with each text fragment as it arrives.
    /// Providers without real streaming keep this default: one fragment at the end.
    async fn complete_stream(
        &self,
        session: &str,
        system: &str,
        messages: &[Message],
        tools: &[ToolSpec],
        on_text: &mut (dyn for<'a> FnMut(&'a str) + Send),
    ) -> Result<Completion> {
        let c = self.complete(session, system, messages, tools).await?;
        let text = c.message.text();
        if !text.is_empty() {
            on_text(&text);
        }
        Ok(c)
    }
}
