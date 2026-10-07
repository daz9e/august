//! How a model call failed, as a kind callers decide by (retry, fall back, compact, give
//! up) instead of reading messages.

/// What went wrong with a provider call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    /// Too many requests; trying again later helps.
    RateLimit,
    /// The service is busy or failed on its side.
    Overloaded,
    /// The connection or the stream broke.
    Network,
    /// The key or login is missing, wrong or expired.
    Auth,
    /// The plan's or account's limit is used up.
    Quota,
    /// The request was too long for the model's context window.
    ContextTooLong,
    /// The model declined to answer.
    Refused,
    /// The request is wrong in a way trying again won't fix.
    BadRequest,
    Other,
}

impl ErrorKind {
    /// Whether trying again (or another provider) can help.
    pub fn transient(self) -> bool {
        matches!(self, Self::RateLimit | Self::Overloaded | Self::Network)
    }

    /// From an HTTP status and the response body.
    pub fn of_http(status: u16, body: &str) -> Self {
        let body = body.to_lowercase();
        match status {
            _ if too_long(&body) => Self::ContextTooLong,
            402 => Self::Quota,
            429 if body.contains("quota") || body.contains("billing") || body.contains("credit") => Self::Quota,
            429 => Self::RateLimit,
            401 | 403 => Self::Auth,
            408 | 425 => Self::Network,
            500..=599 => Self::Overloaded,
            400..=499 => Self::BadRequest,
            _ => Self::Other,
        }
    }

    /// The kind of any error a provider returned: a `ProviderError` keeps its own; anything
    /// else (a broken connection, a stream error object) is read from its message.
    pub fn of(e: &anyhow::Error) -> Self {
        if let Some(p) = e.chain().find_map(|c| c.downcast_ref::<ProviderError>()) {
            return p.kind;
        }
        let msg = format!("{e:#}").to_lowercase();
        // An untyped "HTTP 503: …" still has its status.
        if let Some(rest) = msg.split("http ").nth(1) {
            let code: String = rest.chars().take_while(char::is_ascii_digit).collect();
            if let Ok(code) = code.parse::<u16>() {
                return Self::of_http(code, &msg);
            }
        }
        let has = |words: &[&str]| words.iter().any(|w| msg.contains(w));
        if too_long(&msg) {
            Self::ContextTooLong
        } else if has(&["unauthorized", "expired", "no credentials", "invalid_api_key", "authentication"]) {
            Self::Auth
        } else if has(&["refused", "refusal"]) {
            Self::Refused
        } else if has(&["insufficient_quota", "quota", "usage limit"]) {
            Self::Quota
        } else if has(&["rate limit", "rate_limit", "too many requests"]) {
            Self::RateLimit
        } else if has(&["overloaded", "server_error", "temporarily", "unavailable"]) {
            Self::Overloaded
        } else if has(&[
            "stream ended", "stream error", "timed out", "timeout", "connection", "error sending request",
            "error decoding", "unexpected eof", "reset by peer", "incomplete message",
        ]) {
            Self::Network
        } else {
            Self::Other
        }
    }
}

fn too_long(text: &str) -> bool {
    ["context_length_exceeded", "prompt is too long", "maximum context length", "context window", "too many tokens"]
        .iter()
        .any(|w| text.contains(w))
}

/// A failed provider call with its kind; reads as `HTTP 429: …` like the raw error did.
#[derive(Debug)]
pub struct ProviderError {
    pub kind: ErrorKind,
    pub message: String,
}

impl ProviderError {
    pub fn http(status: u16, body: &str) -> Self {
        Self { kind: ErrorKind::of_http(status, body), message: format!("HTTP {status}: {body}") }
    }
}

impl std::fmt::Display for ProviderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ProviderError {}
