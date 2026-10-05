//! Claude Messages API over raw HTTP (there is no official Rust SDK).
//! Also used for Anthropic-compatible endpoints (OpenCode Zen/Go `/messages`).

use super::*;
use serde_json::json;

pub const API_BASE: &str = "https://api.anthropic.com/v1";

/// Models that accept `fallbacks: "default"` (server-side refusal fallback).
const FALLBACK_MODELS: &[&str] = &[
    "claude-opus-5-5",
    "claude-opus-5",
    "claude-fable-5-1",
    "claude-sonnet-5-5",
];

pub struct Anthropic {
    http: reqwest::Client,
    url: String,
    official: bool,
    api_key: String,
    model: String,
    effort: String,
    session_header: Option<&'static str>,
}

impl Anthropic {
    pub fn new(base_url: &str, api_key: String, model: String, effort: String) -> Self {
        let base_url = base_url.trim_end_matches('/');
        Self {
            http: crate::util::http_client(),
            url: format!("{base_url}/messages"),
            official: base_url == API_BASE,
            api_key,
            model,
            effort,
            session_header: None,
        }
    }

    /// Sends the conversation id in this header (e.g. `x-opencode-session`).
    pub fn with_session_header(mut self, name: &'static str) -> Self {
        self.session_header = Some(name);
        self
    }

    fn encode_message(m: &Message) -> Value {
        let role = match m.role {
            Role::User => "user",
            Role::Assistant => "assistant",
        };
        let content: Vec<Value> = m
            .content
            .iter()
            .map(|b| match b {
                Block::Text(t) => json!({"type": "text", "text": t}),
                Block::ToolUse { id, name, input } => {
                    json!({"type": "tool_use", "id": id, "name": name, "input": input})
                }
                Block::ToolResult {
                    tool_use_id,
                    content,
                    is_error,
                } => json!({
                    "type": "tool_result",
                    "tool_use_id": tool_use_id,
                    "content": content,
                    "is_error": is_error,
                }),
                Block::Opaque(v) => v.clone(),
            })
            .collect();
        json!({"role": role, "content": content})
    }

    fn decode_block(v: &Value) -> Block {
        match v["type"].as_str() {
            Some("text") => Block::Text(v["text"].as_str().unwrap_or_default().to_string()),
            Some("tool_use") => Block::ToolUse {
                id: v["id"].as_str().unwrap_or_default().to_string(),
                name: v["name"].as_str().unwrap_or_default().to_string(),
                input: v["input"].clone(),
            },
            // thinking, redacted_thinking, server tool blocks, ...
            _ => Block::Opaque(v.clone()),
        }
    }
}

/// Accumulates Anthropic SSE events into a `Completion`.
#[derive(Default)]
struct StreamState {
    /// Content blocks by index, with tool input JSON still as partial text.
    blocks: Vec<(Value, String)>,
    stop_reason: Option<String>,
    usage: Usage,
}

impl StreamState {
    fn event(&mut self, v: &Value, on_text: &mut (dyn for<'a> FnMut(&'a str) + Send)) -> Result<()> {
        match v["type"].as_str() {
            Some("message_start") => {
                self.usage_from(&v["message"]["usage"]);
            }
            Some("content_block_start") => {
                let i = v["index"].as_u64().unwrap_or(0) as usize;
                while self.blocks.len() <= i {
                    self.blocks.push((Value::Null, String::new()));
                }
                self.blocks[i] = (v["content_block"].clone(), String::new());
            }
            Some("content_block_delta") => {
                let i = v["index"].as_u64().unwrap_or(0) as usize;
                let Some((block, partial)) = self.blocks.get_mut(i) else {
                    return Ok(());
                };
                let d = &v["delta"];
                let append = |block: &mut Value, key: &str, add: &str| {
                    let cur = block[key].as_str().unwrap_or_default().to_string();
                    block[key] = Value::String(cur + add);
                };
                match d["type"].as_str() {
                    Some("text_delta") => {
                        let t = d["text"].as_str().unwrap_or_default();
                        append(block, "text", t);
                        if !t.is_empty() {
                            on_text(t);
                        }
                    }
                    Some("input_json_delta") => {
                        partial.push_str(d["partial_json"].as_str().unwrap_or_default());
                    }
                    Some("thinking_delta") => {
                        append(block, "thinking", d["thinking"].as_str().unwrap_or_default());
                    }
                    Some("signature_delta") => {
                        append(block, "signature", d["signature"].as_str().unwrap_or_default());
                    }
                    _ => {}
                }
            }
            Some("message_delta") => {
                if let Some(r) = v["delta"]["stop_reason"].as_str() {
                    self.stop_reason = Some(r.to_string());
                }
                self.usage_from(&v["usage"]);
            }
            Some("error") => anyhow::bail!("stream error: {}", v["error"]),
            _ => {}
        }
        Ok(())
    }

    fn usage_from(&mut self, u: &Value) {
        if let Some(n) = u["input_tokens"].as_u64() {
            self.usage.input_tokens = n;
        }
        if let Some(n) = u["output_tokens"].as_u64() {
            self.usage.output_tokens = n;
        }
        if let Some(n) = u["cache_read_input_tokens"].as_u64() {
            self.usage.cache_read_tokens = n;
        }
    }

    fn finish(self) -> Completion {
        let content = self
            .blocks
            .into_iter()
            .filter(|(b, _)| !b.is_null())
            .map(|(mut b, partial)| {
                if !partial.is_empty() {
                    b["input"] = serde_json::from_str(&partial)
                        .unwrap_or_else(|_| json!({ "_raw": partial }));
                }
                Anthropic::decode_block(&b)
            })
            .collect();
        Completion {
            message: Message {
                role: Role::Assistant,
                content,
            },
            stop_reason: match self.stop_reason.as_deref() {
                Some("end_turn") | Some("stop_sequence") => StopReason::EndTurn,
                Some("tool_use") => StopReason::ToolUse,
                Some("max_tokens") => StopReason::MaxTokens,
                Some("refusal") => StopReason::Refusal,
                other => StopReason::Other(other.unwrap_or("unknown").to_string()),
            },
            usage: self.usage,
        }
    }
}

impl Anthropic {
    async fn run(
        &self,
        session: &str,
        system: &str,
        messages: &[Message],
        tools: &[ToolSpec],
        on_text: &mut (dyn for<'a> FnMut(&'a str) + Send),
    ) -> Result<Completion> {
        let tools: Vec<Value> = tools
            .iter()
            .map(|t| {
                json!({
                    "name": t.name,
                    "description": t.description,
                    "input_schema": t.input_schema,
                })
            })
            .collect();

        let mut body = json!({
            "model": self.model,
            "max_tokens": 16000,
            "stream": true,
            "system": system,
            "tools": tools,
            "messages": messages.iter().map(Self::encode_message).collect::<Vec<_>>(),
            // Automatic prompt caching of the growing prefix.
            "cache_control": {"type": "ephemeral"},
        });
        // Haiku 4.5 has no adaptive thinking / effort; non-Claude models behind
        // Anthropic-compatible endpoints may not accept these fields at all.
        if self.model.starts_with("claude-") && !self.model.contains("haiku") {
            body["thinking"] = json!({"type": "adaptive"});
            body["output_config"] = json!({"effort": self.effort});
        }

        let mut req = self
            .http
            .post(&self.url)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", "2023-06-01");
        if let Some(h) = self.session_header {
            req = req.header(h, session);
        }
        if self.official && FALLBACK_MODELS.contains(&self.model.as_str()) {
            body["fallbacks"] = json!("default");
            req = req.header("anthropic-beta", "server-side-fallback-2026-07-01");
        }

        let resp = sse::post_stream(req, &body).await?;
        let mut state = StreamState::default();
        let mut done = false;
        sse::for_each_event(resp, |ev| {
            if let Ok(v) = serde_json::from_str::<Value>(&ev.data) {
                state.event(&v, on_text)?;
                done = v["type"] == "message_stop";
            }
            Ok(done)
        })
        .await?;
        if !done {
            anyhow::bail!("stream ended before message_stop");
        }
        Ok(state.finish())
    }
}

#[async_trait]
impl LlmProvider for Anthropic {
    fn name(&self) -> &str {
        &self.model
    }

    async fn complete(
        &self,
        session: &str,
        system: &str,
        messages: &[Message],
        tools: &[ToolSpec],
    ) -> Result<Completion> {
        self.run(session, system, messages, tools, &mut |_| {}).await
    }

    async fn complete_stream(
        &self,
        session: &str,
        system: &str,
        messages: &[Message],
        tools: &[ToolSpec],
        on_text: &mut (dyn for<'a> FnMut(&'a str) + Send),
    ) -> Result<Completion> {
        self.run(session, system, messages, tools, on_text).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accumulates_stream() {
        let events = [
            json!({"type":"message_start","message":{"usage":{"input_tokens":10,"cache_read_input_tokens":4,"output_tokens":1}}}),
            json!({"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"","signature":""}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"hm"}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"sig"}}),
            json!({"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}),
            json!({"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"Hel"}}),
            json!({"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"lo"}}),
            json!({"type":"content_block_start","index":2,"content_block":{"type":"tool_use","id":"t1","name":"sh","input":{}}}),
            json!({"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"{\"a\":"}}),
            json!({"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"1}"}}),
            json!({"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":42}}),
        ];
        let mut st = StreamState::default();
        let mut got = String::new();
        for e in &events {
            st.event(e, &mut |t| got.push_str(t)).unwrap();
        }
        let c = st.finish();
        assert_eq!(got, "Hello");
        assert_eq!(c.stop_reason, StopReason::ToolUse);
        assert_eq!((c.usage.input_tokens, c.usage.output_tokens, c.usage.cache_read_tokens), (10, 42, 4));
        match &c.message.content[0] {
            Block::Opaque(v) => {
                assert_eq!(v["thinking"], "hm");
                assert_eq!(v["signature"], "sig");
            }
            b => panic!("{b:?}"),
        }
        assert!(matches!(&c.message.content[1], Block::Text(t) if t == "Hello"));
        match &c.message.content[2] {
            Block::ToolUse { id, input, .. } => {
                assert_eq!(id, "t1");
                assert_eq!(input["a"], 1);
            }
            b => panic!("{b:?}"),
        }
    }
}
