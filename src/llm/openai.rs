//! OpenAI-compatible Chat Completions (OpenAI, OpenRouter, Ollama, vLLM, ...).

use super::*;
use serde_json::json;

pub struct OpenAi {
    http: reqwest::Client,
    base_url: String,
    api_key: String,
    model: String,
    session_header: Option<&'static str>,
    /// The model's context window, if configured.
    window: Option<usize>,
}

impl OpenAi {
    pub fn new(base_url: String, api_key: String, model: String) -> Self {
        Self {
            http: crate::util::http_client(),
            base_url: base_url.trim_end_matches('/').to_string(),
            api_key,
            model,
            session_header: None,
            window: None,
        }
    }

    /// The model's context window, as configured.
    pub fn with_context_window(mut self, tokens: Option<usize>) -> Self {
        self.window = tokens;
        self
    }

    /// Sends the conversation id in this header (e.g. `x-opencode-session`).
    pub fn with_session_header(mut self, name: &'static str) -> Self {
        self.session_header = Some(name);
        self
    }

    fn encode(system: &str, messages: &[Message]) -> Vec<Value> {
        let mut out = vec![json!({"role": "system", "content": system})];
        for m in messages {
            match m.role {
                Role::User => {
                    // Tool results become separate `tool` messages.
                    for b in &m.content {
                        if let Block::ToolResult {
                            tool_use_id,
                            content,
                            ..
                        } = b
                        {
                            out.push(json!({
                                "role": "tool",
                                "tool_call_id": tool_use_id,
                                "content": content,
                            }));
                        }
                    }
                    let text = m.text();
                    let images: Vec<Value> = m
                        .content
                        .iter()
                        .filter_map(|b| match b {
                            Block::Image { media_type, path } => Some(match image_data_url(media_type, path) {
                                Some(url) => json!({"type": "image_url", "image_url": {"url": url}}),
                                None => json!({"type": "text", "text": missing_image(path)}),
                            }),
                            _ => None,
                        })
                        .collect();
                    if !images.is_empty() {
                        let mut parts = images;
                        if !text.is_empty() {
                            parts.insert(0, json!({"type": "text", "text": text}));
                        }
                        out.push(json!({"role": "user", "content": parts}));
                    } else if !text.is_empty() {
                        out.push(json!({"role": "user", "content": text}));
                    }
                }
                Role::Assistant => {
                    let calls: Vec<Value> = m
                        .tool_uses()
                        .map(|(id, name, input)| {
                            json!({
                                "id": id,
                                "type": "function",
                                "function": {"name": name, "arguments": input.to_string()},
                            })
                        })
                        .collect();
                    let text = m.text();
                    let mut msg = json!({
                        "role": "assistant",
                        "content": if text.is_empty() { Value::Null } else { json!(text) },
                    });
                    if !calls.is_empty() {
                        msg["tool_calls"] = json!(calls);
                    }
                    out.push(msg);
                }
            }
        }
        out
    }
}

fn finish_reason(r: Option<&str>) -> StopReason {
    match r {
        Some("stop") => StopReason::EndTurn,
        Some("tool_calls") | Some("function_call") => StopReason::ToolUse,
        Some("length") => StopReason::MaxTokens,
        Some("content_filter") => StopReason::Refusal,
        other => StopReason::Other(other.unwrap_or("unknown").to_string()),
    }
}

fn usage_of(u: &Value) -> Usage {
    // `prompt_tokens` includes the cached part.
    let cached = u["prompt_tokens_details"]["cached_tokens"].as_u64().unwrap_or(0);
    Usage {
        input_tokens: u["prompt_tokens"].as_u64().unwrap_or(0).saturating_sub(cached),
        output_tokens: u["completion_tokens"].as_u64().unwrap_or(0),
        cache_read_tokens: cached,
        cache_write_tokens: 0,
    }
}

fn tool_block(id: &str, name: &str, args: &str) -> Block {
    let args = if args.trim().is_empty() { "{}" } else { args };
    Block::ToolUse {
        id: id.to_string(),
        name: name.to_string(),
        input: serde_json::from_str(args).unwrap_or_else(|_| json!({ "_raw": args })),
    }
}

/// Accumulates Chat Completions stream chunks into a `Completion`.
#[derive(Default)]
struct StreamState {
    text: String,
    /// (id, name, arguments) by tool call index.
    calls: Vec<(String, String, String)>,
    finish: Option<String>,
    usage: Usage,
}

impl StreamState {
    fn chunk(&mut self, v: &Value, on_text: &mut (dyn for<'a> FnMut(&'a str) + Send)) -> Result<()> {
        if !v["error"].is_null() {
            anyhow::bail!("stream error: {}", v["error"]);
        }
        if v["usage"].is_object() {
            self.usage = usage_of(&v["usage"]);
        }
        let choice = &v["choices"][0];
        if let Some(t) = choice["delta"]["content"].as_str().filter(|t| !t.is_empty()) {
            self.text.push_str(t);
            on_text(t);
        }
        for tc in choice["delta"]["tool_calls"].as_array().into_iter().flatten() {
            let i = tc["index"].as_u64().unwrap_or(0) as usize;
            while self.calls.len() <= i {
                self.calls.push(Default::default());
            }
            let c = &mut self.calls[i];
            if let Some(id) = tc["id"].as_str().filter(|s| !s.is_empty()) {
                c.0 = id.to_string();
            }
            if let Some(n) = tc["function"]["name"].as_str() {
                c.1.push_str(n);
            }
            if let Some(a) = tc["function"]["arguments"].as_str() {
                c.2.push_str(a);
            }
        }
        if let Some(r) = choice["finish_reason"].as_str() {
            self.finish = Some(r.to_string());
        }
        Ok(())
    }

    fn finish(self) -> Completion {
        let mut content = Vec::new();
        if !self.text.is_empty() {
            content.push(Block::Text(self.text));
        }
        for (id, name, args) in &self.calls {
            content.push(tool_block(id, name, args));
        }
        Completion {
            message: Message {
                role: Role::Assistant,
                content,
            },
            stop_reason: finish_reason(self.finish.as_deref()),
            usage: self.usage,
        }
    }
}

impl OpenAi {
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
                    "type": "function",
                    "function": {
                        "name": t.name,
                        "description": t.description,
                        "parameters": t.input_schema,
                    },
                })
            })
            .collect();
        let body = json!({
            "model": self.model,
            "messages": Self::encode(system, messages),
            "tools": tools,
            "stream": true,
            "stream_options": {"include_usage": true},
        });
        let mut req = self
            .http
            .post(format!("{}/chat/completions", self.base_url))
            .bearer_auth(&self.api_key);
        if let Some(h) = self.session_header {
            req = req.header(h, session);
        }
        let resp = sse::post_stream(req, &body).await?;

        // Some compatible servers ignore `stream` and answer with plain JSON.
        let is_json = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.contains("application/json"));
        if is_json {
            let resp: Value = resp.json().await?;
            // Some proxies report failures as a 200 with an `error` object.
            if !resp["error"].is_null() && resp["choices"].is_null() {
                anyhow::bail!("provider error: {}", resp["error"]);
            }
            return Ok(Self::decode_full(&resp, on_text));
        }

        let mut state = StreamState::default();
        sse::for_each_event(resp, |ev| {
            if ev.data.trim() == "[DONE]" {
                return Ok(true);
            }
            if let Ok(v) = serde_json::from_str::<Value>(&ev.data) {
                state.chunk(&v, on_text)?;
            }
            Ok(false)
        })
        .await?;
        Ok(state.finish())
    }

    fn decode_full(resp: &Value, on_text: &mut (dyn for<'a> FnMut(&'a str) + Send)) -> Completion {
        let choice = &resp["choices"][0];
        let msg = &choice["message"];
        let mut content = Vec::new();
        if let Some(t) = msg["content"].as_str().filter(|t| !t.is_empty()) {
            on_text(t);
            content.push(Block::Text(t.to_string()));
        }
        for call in msg["tool_calls"].as_array().into_iter().flatten() {
            content.push(tool_block(
                call["id"].as_str().unwrap_or_default(),
                call["function"]["name"].as_str().unwrap_or_default(),
                call["function"]["arguments"].as_str().unwrap_or("{}"),
            ));
        }
        Completion {
            message: Message {
                role: Role::Assistant,
                content,
            },
            stop_reason: finish_reason(choice["finish_reason"].as_str()),
            usage: usage_of(&resp["usage"]),
        }
    }
}

#[async_trait]
impl LlmProvider for OpenAi {
    fn context_window(&self) -> Option<usize> {
        self.window
    }

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
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[tokio::test]
    async fn sends_session_header_and_user_agent() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 16384];
            let n = sock.read(&mut buf).await.unwrap();
            let body = r#"{"choices":[{"message":{"content":"ok"},"finish_reason":"stop"}]}"#;
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            );
            sock.write_all(resp.as_bytes()).await.unwrap();
            String::from_utf8_lossy(&buf[..n]).to_lowercase()
        });

        let p = OpenAi::new(base, "k".into(), "m".into()).with_session_header("x-opencode-session");
        let c = p.complete("sess-1", "sys", &[Message::user_text("hi")], &[]).await.unwrap();
        assert_eq!(c.message.text(), "ok");

        let req = server.await.unwrap();
        assert!(req.contains("x-opencode-session: sess-1"), "{req}");
        assert!(req.contains("user-agent: august/"), "{req}");
    }

    #[test]
    fn accumulates_stream() {
        let chunks = [
            json!({"choices":[{"delta":{"role":"assistant","content":"Hi"}}]}),
            json!({"choices":[{"delta":{"content":" there"}}]}),
            json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c1","function":{"name":"sh","arguments":""}}]}}]}),
            json!({"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"a\""}}]}}]}),
            json!({"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":":1}"}}]}}]}),
            json!({"choices":[{"delta":{},"finish_reason":"tool_calls"}]}),
            json!({"choices":[],"usage":{"prompt_tokens":7,"completion_tokens":3,"prompt_tokens_details":{"cached_tokens":2}}}),
        ];
        let mut st = StreamState::default();
        let mut got = String::new();
        for c in &chunks {
            st.chunk(c, &mut |t| got.push_str(t)).unwrap();
        }
        let c = st.finish();
        assert_eq!(got, "Hi there");
        assert_eq!(c.stop_reason, StopReason::ToolUse);
        assert_eq!((c.usage.input_tokens, c.usage.output_tokens, c.usage.cache_read_tokens), (5, 3, 2));
        assert!(matches!(&c.message.content[0], Block::Text(t) if t == "Hi there"));
        assert!(matches!(&c.message.content[1], Block::ToolUse { id, input, .. } if id == "c1" && input["a"] == 1));
    }
}
