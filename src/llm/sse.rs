//! Minimal Server-Sent Events parser.

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SseEvent {
    pub event: Option<String>,
    pub data: String,
}

/// Incremental SSE parser: feed raw chunks, get complete events back.
#[derive(Default)]
pub struct SseParser {
    buf: Vec<u8>,
}

impl SseParser {
    pub fn push(&mut self, chunk: &[u8]) -> Vec<SseEvent> {
        self.buf.extend_from_slice(chunk);
        let mut out = Vec::new();
        while let Some(pos) = find_event_end(&self.buf) {
            let raw: Vec<u8> = self.buf.drain(..pos).collect();
            let text = String::from_utf8_lossy(&raw);
            let mut event = None;
            let mut data = Vec::new();
            for line in text.lines() {
                if let Some(v) = line.strip_prefix("data:") {
                    data.push(v.strip_prefix(' ').unwrap_or(v).to_string());
                } else if let Some(v) = line.strip_prefix("event:") {
                    event = Some(v.trim().to_string());
                }
            }
            if !data.is_empty() {
                out.push(SseEvent { event, data: data.join("\n") });
            }
        }
        out
    }
}

/// Index just past the blank line that terminates an SSE event.
fn find_event_end(buf: &[u8]) -> Option<usize> {
    let lf = buf.windows(2).position(|w| w == b"\n\n").map(|p| p + 2);
    let crlf = buf.windows(4).position(|w| w == b"\r\n\r\n").map(|p| p + 4);
    match (lf, crlf) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    }
}

#[derive(Debug)]
pub struct HttpError {
    /// None for network errors.
    pub status: Option<u16>,
    pub body: String,
}

impl std::fmt::Display for HttpError {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        let Some(status) = self.status else { return write!(f, "network error: {}", self.body) };
        // Show the API's own message instead of the raw JSON body.
        let v: serde_json::Value = serde_json::from_str(&self.body).unwrap_or_default();
        let msg = v["error"]["message"].as_str().or(v["error"].as_str()).or(v["detail"].as_str()).or(v["message"].as_str());
        let text: String = msg.unwrap_or(&self.body).chars().take(500).collect();
        write!(f, "HTTP {status}: {text}")
    }
}
impl std::error::Error for HttpError {}

/// Sends a streaming POST and returns the response before its body is read.
/// Retries 429 / 5xx / network errors with backoff (only before the stream starts).
pub async fn post(req: reqwest::RequestBuilder, body: &serde_json::Value) -> Result<reqwest::Response, HttpError> {
    let mut delay = std::time::Duration::from_secs(2);
    let mut attempt = 0;
    loop {
        let res = req.try_clone().expect("cloneable request").header("accept", "text/event-stream").json(body).send().await;
        let err = match res {
            Ok(r) if r.status().is_success() => return Ok(r),
            Ok(r) => {
                let status = r.status().as_u16();
                HttpError { status: Some(status), body: r.text().await.unwrap_or_default() }
            }
            Err(e) => HttpError { status: None, body: e.to_string() },
        };
        let retryable = matches!(err.status, None | Some(429) | Some(500..=599));
        if !retryable || attempt == 3 {
            return Err(err);
        }
        attempt += 1;
        tokio::time::sleep(delay).await;
        delay *= 2;
    }
}

/// Reads the stream, calling `f` per event until it returns `true`; errors if the stream ends first.
pub async fn read(mut resp: reqwest::Response, mut f: impl FnMut(SseEvent) -> anyhow::Result<bool>) -> anyhow::Result<()> {
    let mut p = SseParser::default();
    while let Some(chunk) = resp.chunk().await? {
        for ev in p.push(&chunk) {
            if f(ev)? {
                return Ok(());
            }
        }
    }
    anyhow::bail!("stream ended early")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_split_and_crlf_events() {
        let mut p = SseParser::default();
        assert!(p.push(b"event: a\ndata: {\"x\"").is_empty());
        let evs = p.push(b":1}\n\ndata: l1\r\ndata: l2\r\n\r\n: comment\n\n");
        assert_eq!(evs.len(), 2);
        assert_eq!(evs[0].event.as_deref(), Some("a"));
        assert_eq!(evs[0].data, "{\"x\":1}");
        assert_eq!(evs[1].data, "l1\nl2");
    }
}
