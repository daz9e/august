//! Claude Messages API over raw HTTP (there is no official Rust SDK), streamed.

use super::*;
use serde_json::json;

const API: &str = "https://api.anthropic.com/v1";
const PROVIDER: &str = "anthropic";
/// Models that accept `fallbacks: "default"` (server-side refusal fallback).
const FALLBACK_MODELS: &[&str] = &["claude-opus-5-5", "claude-opus-5", "claude-fable-5-1", "claude-sonnet-5-5"];

fn encode(m: &Message) -> Value {
    let role = match m.role {
        Role::User => "user",
        Role::Assistant => "assistant",
    };
    let content: Vec<Value> = m
        .content
        .iter()
        .filter_map(|b| match b {
            Block::Text { text } => Some(json!({"type": "text", "text": text})),
            Block::ToolUse { id, name, input } => Some(json!({"type": "tool_use", "id": id, "name": name, "input": input})),
            Block::ToolResult { tool_use_id, content, is_error } => Some(json!({
                "type": "tool_result", "tool_use_id": tool_use_id, "content": content, "is_error": is_error,
            })),
            Block::Opaque { provider, value } => (provider == PROVIDER).then(|| value.clone()),
            Block::Image { media_type, path } => Some(match image_base64(path) {
                Some(data) => json!({"type": "image", "source": {"type": "base64", "media_type": media_type, "data": data}}),
                None => json!({"type": "text", "text": missing_image(path)}),
            }),
        })
        .collect();
    json!({"role": role, "content": content})
}

/// Encodes the history, merging neighbours with the same role (e.g. tool results followed by
/// a new message after an interrupted turn) so roles strictly alternate.
fn encode_all(messages: &[Message]) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::new();
    for m in messages {
        let v = encode(m);
        match out.last_mut() {
            Some(prev) if prev["role"] == v["role"] => {
                if let (Some(a), Some(b)) = (prev["content"].as_array_mut(), v["content"].as_array()) {
                    a.extend(b.iter().cloned());
                }
            }
            _ => out.push(v),
        }
    }
    out
}

fn decode(v: Value) -> Block {
    match v["type"].as_str() {
        Some("text") => Block::Text { text: v["text"].as_str().unwrap_or_default().into() },
        Some("tool_use") => Block::ToolUse {
            id: v["id"].as_str().unwrap_or_default().into(),
            name: v["name"].as_str().unwrap_or_default().into(),
            input: v["input"].clone(),
        },
        // thinking, redacted_thinking, ...
        _ => Block::Opaque { provider: PROVIDER.into(), value: v },
    }
}

/// Accumulates SSE events into a `Completion`.
#[derive(Default)]
struct Stream {
    /// Content blocks by index, with tool input JSON still as partial text.
    blocks: Vec<(Value, String)>,
    stop: Option<String>,
    usage: Usage,
}

impl Stream {
    fn event(&mut self, v: &Value, on_text: OnText) -> Result<()> {
        match v["type"].as_str() {
            Some("message_start") => self.usage_from(&v["message"]["usage"]),
            Some("content_block_start") => {
                let i = v["index"].as_u64().unwrap_or(0) as usize;
                while self.blocks.len() <= i {
                    self.blocks.push((Value::Null, String::new()));
                }
                self.blocks[i] = (v["content_block"].clone(), String::new());
            }
            Some("content_block_delta") => {
                let i = v["index"].as_u64().unwrap_or(0) as usize;
                let Some((block, partial)) = self.blocks.get_mut(i) else { return Ok(()) };
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
                    Some("input_json_delta") => partial.push_str(d["partial_json"].as_str().unwrap_or_default()),
                    Some("thinking_delta") => append(block, "thinking", d["thinking"].as_str().unwrap_or_default()),
                    Some("signature_delta") => append(block, "signature", d["signature"].as_str().unwrap_or_default()),
                    _ => {}
                }
            }
            Some("message_delta") => {
                if let Some(r) = v["delta"]["stop_reason"].as_str() {
                    self.stop = Some(r.into());
                }
                self.usage_from(&v["usage"]);
            }
            Some("error") => bail!("stream error: {}", v["error"]),
            _ => {}
        }
        Ok(())
    }

    fn usage_from(&mut self, u: &Value) {
        let set = |dst: &mut u64, key: &str| {
            if let Some(n) = u[key].as_u64() {
                *dst = n;
            }
        };
        set(&mut self.usage.input, "input_tokens");
        set(&mut self.usage.output, "output_tokens");
        set(&mut self.usage.cache_read, "cache_read_input_tokens");
        set(&mut self.usage.cache_write, "cache_creation_input_tokens");
    }

    fn finish(self) -> Completion {
        let content = self
            .blocks
            .into_iter()
            .filter(|(b, _)| !b.is_null())
            .map(|(mut b, partial)| {
                if !partial.is_empty() {
                    b["input"] = serde_json::from_str(&partial).unwrap_or_else(|_| json!({ "_raw": partial }));
                }
                decode(b)
            })
            .collect();
        let stop = match self.stop.as_deref() {
            Some("end_turn") | Some("stop_sequence") => Stop::EndTurn,
            Some("tool_use") => Stop::ToolUse,
            Some("max_tokens") => Stop::MaxTokens,
            other => Stop::Other(other.unwrap_or("unknown").into()),
        };
        Completion { message: Message { role: Role::Assistant, content }, stop, usage: self.usage }
    }
}

pub async fn complete(
    http: &reqwest::Client,
    key: &str,
    cfg: &Config,
    system: &str,
    messages: &[Message],
    tools: &[ToolSpec],
    on_text: OnText<'_>,
) -> Result<Completion> {
    let mut tools: Vec<Value> = tools
        .iter()
        .map(|t| json!({"name": t.name, "description": t.description, "input_schema": t.schema}))
        .collect();
    if cfg.web_search {
        tools.push(json!({"type": "web_search_20260209", "name": "web_search"}));
    }
    let mut body = json!({
        "model": cfg.model,
        "max_tokens": 64000,
        "stream": true,
        "system": system,
        "tools": tools,
        "messages": encode_all(messages),
        // Automatic caching of the growing prefix.
        "cache_control": {"type": "ephemeral"},
    });
    // Haiku 4.5 and older models take neither adaptive thinking nor effort.
    if !cfg.model.contains("haiku-4") {
        body["thinking"] = json!({"type": "adaptive"});
        body["output_config"] = json!({"effort": cfg.effort});
    }
    let mut req = http.post(format!("{API}/messages")).header("x-api-key", key).header("anthropic-version", "2023-06-01");
    if FALLBACK_MODELS.contains(&cfg.model.as_str()) {
        body["fallbacks"] = json!("default");
        req = req.header("anthropic-beta", "server-side-fallback-2026-07-01");
    }

    let resp = sse::post(req, &body).await?;
    let mut state = Stream::default();
    sse::read(resp, |ev| {
        let Ok(v) = serde_json::from_str::<Value>(&ev.data) else { return Ok(false) };
        state.event(&v, on_text)?;
        Ok(v["type"] == "message_stop")
    })
    .await?;
    Ok(state.finish())
}

/// Model ids for the login picker.
pub async fn models(http: &reqwest::Client, key: &str) -> Result<Vec<String>> {
    let resp = http
        .get(format!("{API}/models?limit=100"))
        .header("x-api-key", key)
        .header("anthropic-version", "2023-06-01")
        .send()
        .await?;
    let status = resp.status();
    let v: Value = resp.json().await?;
    if !status.is_success() {
        bail!("HTTP {status}: {}", v["error"]["message"].as_str().unwrap_or_default());
    }
    Ok(v["data"].as_array().into_iter().flatten().filter_map(|m| m["id"].as_str().map(String::from)).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merges_same_role_neighbours() {
        let msgs = [
            Message { role: Role::User, content: vec![Block::ToolResult { tool_use_id: "t".into(), content: "x".into(), is_error: true }] },
            Message::user("next"),
            Message { role: Role::Assistant, content: vec![Block::Text { text: "ok".into() }] },
        ];
        let out = encode_all(&msgs);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0]["content"].as_array().unwrap().len(), 2);
        assert_eq!(out[0]["content"][0]["type"], "tool_result");
    }

    #[test]
    fn accumulates_stream() {
        let events = [
            json!({"type":"message_start","message":{"usage":{"input_tokens":10,"cache_read_input_tokens":4,"cache_creation_input_tokens":3,"output_tokens":1}}}),
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
        let mut st = Stream::default();
        let mut got = String::new();
        for e in &events {
            st.event(e, &mut |t: &str| got.push_str(t)).unwrap();
        }
        let c = st.finish();
        assert_eq!(got, "Hello");
        assert_eq!(c.stop, Stop::ToolUse);
        assert_eq!((c.usage.input, c.usage.output, c.usage.cache_read, c.usage.cache_write), (10, 42, 4, 3));
        assert!(matches!(&c.message.content[0], Block::Opaque { value, .. } if value["signature"] == "sig"));
        assert!(matches!(&c.message.content[1], Block::Text { text } if text == "Hello"));
        assert!(matches!(&c.message.content[2], Block::ToolUse { input, .. } if input["a"] == 1));
    }
}
