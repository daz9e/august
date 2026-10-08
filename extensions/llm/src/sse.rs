//! Minimal Server-Sent Events parser and a retrying streaming POST.

use anyhow::Result;
use serde_json::Value;

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
                out.push(SseEvent {
                    event,
                    data: data.join("\n"),
                });
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

/// POST JSON and return the successful response before its body is read.
/// Retries on 429 / 5xx / network errors (only before the stream starts).
pub(crate) async fn post_stream(
    req: reqwest::RequestBuilder,
    body: &Value,
) -> Result<reqwest::Response> {
    let mut delay = std::time::Duration::from_secs(2);
    for attempt in 0..4 {
        let resp = req
            .try_clone()
            .expect("request without streaming body")
            .header("accept", "text/event-stream")
            .json(body)
            .send()
            .await;
        match resp {
            Ok(r) if r.status().is_success() => return Ok(r),
            Ok(r) => {
                let status = r.status();
                let text = r.text().await.unwrap_or_default();
                let retryable = status.as_u16() == 429 || status.is_server_error();
                if !retryable || attempt == 3 {
                    return Err(super::error::ProviderError::http(status.as_u16(), &text).into());
                }
            }
            Err(e) if attempt == 3 => return Err(e.into()),
            Err(_) => {}
        }
        tokio::time::sleep(delay).await;
        delay *= 2;
    }
    unreachable!()
}

/// Reads the whole stream, calling `f` for every event; `f` returns `true` to stop early.
pub(crate) async fn for_each_event(
    mut resp: reqwest::Response,
    mut f: impl FnMut(SseEvent) -> Result<bool>,
) -> Result<()> {
    let mut p = SseParser::default();
    while let Some(chunk) = resp.chunk().await? {
        for ev in p.push(&chunk) {
            if f(ev)? {
                return Ok(());
            }
        }
    }
    Ok(())
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
