//! OpenAI Responses API wire format (streaming, `store: false`, full history each request).
//! Shared by the ChatGPT-plan provider and OpenCode Zen/Go models served over `/responses`.

use super::*;
use serde_json::json;

/// Supplies the bearer token: a static API key, or a refreshable OAuth session.
#[async_trait]
pub trait TokenSource: Send + Sync {
    async fn token(&self) -> Result<String>;
    /// Called once after a 401; returns whether a retry with a new token makes sense.
    async fn on_unauthorized(&self) -> Result<bool> {
        Ok(false)
    }
    /// Extra request headers (e.g. the ChatGPT account id).
    fn headers(&self) -> Vec<(&'static str, String)> {
        Vec::new()
    }
}

#[async_trait]
impl<T: TokenSource + ?Sized> TokenSource for std::sync::Arc<T> {
    async fn token(&self) -> Result<String> {
        (**self).token().await
    }
    async fn on_unauthorized(&self) -> Result<bool> {
        (**self).on_unauthorized().await
    }
    fn headers(&self) -> Vec<(&'static str, String)> {
        (**self).headers()
    }
}

pub struct ApiKey(pub String);

#[async_trait]
impl TokenSource for ApiKey {
    async fn token(&self) -> Result<String> {
        Ok(self.0.clone())
    }
}

pub struct Responses<A> {
    http: reqwest::Client,
    url: String,
    auth: A,
    model: String,
    /// `None` omits the `reasoning` field (models that don't take it).
    effort: Option<String>,
    session_header: Option<&'static str>,
}

impl<A: TokenSource> Responses<A> {
    pub fn new(base_url: &str, auth: A, model: String, effort: Option<String>) -> Self {
        Self {
            http: crate::http_client(),
            url: format!("{}/responses", base_url.trim_end_matches('/')),
            auth,
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

    fn encode(messages: &[Message]) -> Vec<Value> {
        let mut out = Vec::new();
        for m in messages {
            for b in &m.content {
                match (m.role, b) {
                    (Role::User, Block::Text(t)) => {
                        out.push(json!({"role": "user", "content": t}));
                    }
                    (Role::User, Block::Image { media_type, path }) => {
                        let part = match image_data_url(media_type, path) {
                            Some(url) => json!({"type": "input_image", "image_url": url}),
                            None => json!({"type": "input_text", "text": missing_image(path)}),
                        };
                        out.push(json!({"role": "user", "content": [part]}));
                    }
                    (Role::Assistant, Block::Text(t)) => out.push(json!({
                        "type": "message",
                        "role": "assistant",
                        "content": [{"type": "output_text", "text": t}],
                    })),
                    (_, Block::ToolUse { id, name, input }) => out.push(json!({
                        "type": "function_call",
                        "call_id": id,
                        "name": name,
                        "arguments": input.to_string(),
                    })),
                    (_, Block::ToolResult { tool_use_id, content, .. }) => out.push(json!({
                        "type": "function_call_output",
                        "call_id": tool_use_id,
                        "output": content,
                    })),
                    // Reasoning items (with encrypted_content) carry chain of thought
                    // across turns since nothing is stored server-side.
                    (_, Block::Opaque(v)) if v["type"] == "reasoning" => out.push(v.clone()),
                    _ => {}
                }
            }
        }
        out
    }

    fn decode(output: &[Value]) -> Vec<Block> {
        let mut blocks = Vec::new();
        for item in output {
            match item["type"].as_str() {
                Some("message") => {
                    for c in item["content"].as_array().into_iter().flatten() {
                        match c["type"].as_str() {
                            Some("output_text") => blocks.push(Block::Text(
                                c["text"].as_str().unwrap_or_default().to_string(),
                            )),
                            Some("refusal") => blocks.push(Block::Text(
                                c["refusal"].as_str().unwrap_or_default().to_string(),
                            )),
                            _ => {}
                        }
                    }
                }
                Some("function_call") => {
                    let args = item["arguments"].as_str().unwrap_or("{}");
                    blocks.push(Block::ToolUse {
                        id: item["call_id"].as_str().unwrap_or_default().to_string(),
                        name: item["name"].as_str().unwrap_or_default().to_string(),
                        input: serde_json::from_str(args)
                            .unwrap_or_else(|_| json!({ "_raw": args })),
                    });
                }
                _ => blocks.push(Block::Opaque(item.clone())),
            }
        }
        blocks
    }

    /// Sends one request and reads the SSE stream until a terminal event.
    async fn send(
        &self,
        body: &Value,
        token: &str,
        session: &str,
        on_text: &mut (dyn for<'a> FnMut(&'a str) + Send),
    ) -> std::result::Result<Value, ApiError> {
        let mut req = self
            .http
            .post(&self.url)
            .bearer_auth(token)
            .header("accept", "text/event-stream");
        for (k, v) in self.auth.headers() {
            req = req.header(k, v);
        }
        if let Some(h) = self.session_header {
            req = req.header(h, session);
        }
        let mut resp = req
            .json(body)
            .send()
            .await
            .map_err(|e| ApiError::retry(e.to_string()))?;
        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            let code = serde_json::from_str::<Value>(&text)
                .ok()
                .and_then(|v| v["error"]["code"].as_str().map(String::from));
            return Err(ApiError::from_status(status.as_u16(), code, text));
        }

        let mut parser = sse::SseParser::default();
        // Text already shown to the caller must not be replayed by a retry.
        let mut emitted = false;
        // The ChatGPT backend leaves `output` empty in the final event; items only
        // arrive as `response.output_item.done`.
        let mut items = Vec::<Value>::new();
        loop {
            let chunk = resp
                .chunk()
                .await
                .map_err(|e| ApiError::retry(e.to_string()).after(emitted))?;
            let Some(chunk) = chunk else {
                return Err(ApiError::retry("stream ended before response.completed".into())
                    .after(emitted));
            };
            for ev in parser.push(&chunk) {
                if ev.data == "[DONE]" {
                    continue;
                }
                let Ok(v) = serde_json::from_str::<Value>(&ev.data) else {
                    continue;
                };
                match v["type"].as_str() {
                    Some("response.output_text.delta") | Some("response.refusal.delta") => {
                        if let Some(d) = v["delta"].as_str().filter(|d| !d.is_empty()) {
                            emitted = true;
                            on_text(d);
                        }
                    }
                    Some("response.output_item.done") => items.push(v["item"].clone()),
                    Some("response.completed") | Some("response.incomplete") => {
                        let mut response = v["response"].clone();
                        let empty = response["output"].as_array().is_none_or(Vec::is_empty);
                        if empty && !items.is_empty() {
                            response["output"] = Value::Array(items);
                        }
                        return Ok(response);
                    }
                    Some("response.failed") => {
                        let err = &v["response"]["error"];
                        return Err(ApiError::from_code(err["code"].as_str(), err.to_string())
                            .after(emitted));
                    }
                    Some("error") => {
                        let code = v["code"].as_str().or(v["error"]["code"].as_str());
                        return Err(ApiError::from_code(code, v.to_string()).after(emitted));
                    }
                    _ => {}
                }
            }
        }
    }
}

enum ApiErrorKind {
    Retry,
    Unauthorized,
    Fatal,
}

struct ApiError {
    kind: ApiErrorKind,
    msg: String,
}

impl ApiError {
    /// After partial output was streamed, a retry would duplicate it.
    fn after(mut self, emitted: bool) -> Self {
        if emitted && matches!(self.kind, ApiErrorKind::Retry) {
            self.kind = ApiErrorKind::Fatal;
        }
        self
    }

    fn retry(msg: String) -> Self {
        Self {
            kind: ApiErrorKind::Retry,
            msg,
        }
    }

    fn from_code(code: Option<&str>, detail: String) -> Self {
        let (kind, msg) = match code {
            Some("subscription_sharing_usage_limit_exceeded") => (
                ApiErrorKind::Fatal,
                "ChatGPT plan usage limit for August reached; see ChatGPT → Settings → Usage".into(),
            ),
            Some("subscription_sharing_user_not_eligible") => (
                ApiErrorKind::Fatal,
                "this ChatGPT account is not eligible to share plan usage with apps".into(),
            ),
            Some("subscription_sharing_usage_unavailable") | Some("server_error") => {
                (ApiErrorKind::Retry, detail)
            }
            Some("subscription_sharing_invalid_user") => (ApiErrorKind::Unauthorized, detail),
            Some(c) => (ApiErrorKind::Fatal, format!("{c}: {detail}")),
            None => (ApiErrorKind::Fatal, detail),
        };
        Self { kind, msg }
    }

    fn from_status(status: u16, code: Option<String>, text: String) -> Self {
        let detail = format!("HTTP {status}: {text}");
        if code.is_some() {
            return Self::from_code(code.as_deref(), detail);
        }
        let kind = match status {
            401 => ApiErrorKind::Unauthorized,
            429 | 500..=599 => ApiErrorKind::Retry,
            _ => ApiErrorKind::Fatal,
        };
        Self { kind, msg: detail }
    }
}

#[async_trait]
impl<A: TokenSource> LlmProvider for Responses<A> {
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

impl<A: TokenSource> Responses<A> {
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
                    "name": t.name,
                    "description": t.description,
                    "parameters": t.input_schema,
                    "strict": false,
                })
            })
            .collect();
        let mut body = json!({
            "model": self.model,
            "instructions": system,
            "input": Self::encode(messages),
            "tools": tools,
            "stream": true,
            "store": false,
        });
        if let Some(effort) = &self.effort {
            body["reasoning"] = json!({"effort": effort});
            body["include"] = json!(["reasoning.encrypted_content"]);
        }

        let mut delay = std::time::Duration::from_secs(2);
        let mut reauthed = false;
        let mut attempt = 0;
        let resp = loop {
            let token = self.auth.token().await?;
            match self.send(&body, &token, session, on_text).await {
                Ok(r) => break r,
                Err(e) => match e.kind {
                    ApiErrorKind::Unauthorized if !reauthed && self.auth.on_unauthorized().await? => {
                        reauthed = true;
                    }
                    ApiErrorKind::Retry if attempt < 3 => {
                        attempt += 1;
                        tokio::time::sleep(delay).await;
                        delay *= 2;
                    }
                    ApiErrorKind::Unauthorized => anyhow::bail!("unauthorized: {}", e.msg),
                    _ => anyhow::bail!(e.msg),
                },
            }
        };

        let content = Self::decode(resp["output"].as_array().map(Vec::as_slice).unwrap_or(&[]));
        let has_calls = content.iter().any(|b| matches!(b, Block::ToolUse { .. }));
        let stop_reason = match (
            resp["status"].as_str(),
            resp["incomplete_details"]["reason"].as_str(),
        ) {
            _ if has_calls => StopReason::ToolUse,
            (Some("incomplete"), Some("max_output_tokens")) => StopReason::MaxTokens,
            (Some("incomplete"), Some("content_filter")) => StopReason::Refusal,
            (Some("completed"), _) => StopReason::EndTurn,
            (other, _) => StopReason::Other(other.unwrap_or("unknown").to_string()),
        };
        let u = &resp["usage"];
        // `input_tokens` includes the cached part.
        let cached = u["input_tokens_details"]["cached_tokens"].as_u64().unwrap_or(0);
        Ok(Completion {
            message: Message {
                role: Role::Assistant,
                content,
            },
            stop_reason,
            usage: Usage {
                input_tokens: u["input_tokens"].as_u64().unwrap_or(0).saturating_sub(cached),
                output_tokens: u["output_tokens"].as_u64().unwrap_or(0),
                cache_read_tokens: cached,
                cache_write_tokens: 0,
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_final_output() {
        let out = json!([
            {"type":"reasoning","id":"r","encrypted_content":"x"},
            {"type":"message","content":[{"type":"output_text","text":"hi"}]},
            {"type":"function_call","call_id":"c","name":"sh","arguments":"{\"a\":1}"}
        ]);
        let b = Responses::<ApiKey>::decode(out.as_array().unwrap());
        assert!(matches!(&b[0], Block::Opaque(_)));
        assert!(matches!(&b[1], Block::Text(t) if t == "hi"));
        assert!(matches!(&b[2], Block::ToolUse { input, .. } if input["a"] == 1));
    }
}
