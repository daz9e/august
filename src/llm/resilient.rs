//! Wraps a provider with retries (transient errors, with backoff) and an optional
//! fallback provider that takes over when the primary keeps failing.

use super::{Completion, LlmProvider, Message, ToolSpec};
use anyhow::Result;
use async_trait::async_trait;
use std::sync::Arc;
use std::time::Duration;

const PRIMARY_ATTEMPTS: u32 = 3;
const FALLBACK_ATTEMPTS: u32 = 2;
const FIRST_DELAY: Duration = Duration::from_secs(2);

pub struct Resilient {
    primary: Arc<dyn LlmProvider>,
    fallback: Option<Arc<dyn LlmProvider>>,
}

impl Resilient {
    pub fn new(primary: Arc<dyn LlmProvider>, fallback: Option<Arc<dyn LlmProvider>>) -> Self {
        Self { primary, fallback }
    }
}

/// Whether trying again (or another provider) can help: rate limits, server errors,
/// dropped connections and broken streams do; auth, quota and bad-request errors don't.
pub fn is_transient(e: &anyhow::Error) -> bool {
    let msg = format!("{e:#}").to_lowercase();
    if let Some(rest) = msg.split("http ").nth(1) {
        let code: String = rest.chars().take_while(char::is_ascii_digit).collect();
        if let Ok(code) = code.parse::<u16>() {
            return matches!(code, 408 | 425 | 429) || (500..600).contains(&code);
        }
    }
    const FATAL: &[&str] = &["unauthorized", "expired", "no credentials", "invalid_api_key", "refused"];
    if FATAL.iter().any(|f| msg.contains(f)) {
        return false;
    }
    const TRANSIENT: &[&str] = &[
        "stream ended", "stream error", "overloaded", "timed out", "timeout", "connection",
        "error sending request", "error decoding", "rate limit", "unexpected eof", "reset by peer",
        "incomplete message", "temporarily",
    ];
    TRANSIENT.iter().any(|t| msg.contains(t))
}

/// Runs `attempt` up to `tries` times while errors are transient and nothing has been
/// streamed to the caller yet (a retry would replay text the user has already seen).
async fn with_retries<'a, F, Fut>(who: &str, tries: u32, emitted: &std::sync::atomic::AtomicBool, mut attempt: F) -> Result<Completion>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<Completion>> + 'a,
{
    use std::sync::atomic::Ordering::Relaxed;
    let mut delay = FIRST_DELAY;
    let mut n = 1;
    loop {
        match attempt().await {
            Ok(c) => return Ok(c),
            Err(e) if n < tries && is_transient(&e) && !emitted.load(Relaxed) => {
                eprintln!("{who}: {e:#} (retry {n}/{} in {}s)", tries - 1, delay.as_secs());
                tokio::time::sleep(delay).await;
                delay *= 2;
                n += 1;
            }
            Err(e) => return Err(e),
        }
    }
}

#[async_trait]
impl LlmProvider for Resilient {
    fn name(&self) -> &str {
        self.primary.name()
    }

    async fn complete(&self, session: &str, system: &str, messages: &[Message], tools: &[ToolSpec]) -> Result<Completion> {
        self.complete_stream(session, system, messages, tools, &mut |_| {}).await
    }

    async fn complete_stream(
        &self,
        session: &str,
        system: &str,
        messages: &[Message],
        tools: &[ToolSpec],
        on_text: &mut (dyn for<'a> FnMut(&'a str) + Send),
    ) -> Result<Completion> {
        use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
        let emitted = AtomicBool::new(false);
        let on_text = std::sync::Mutex::new(on_text);
        let run = |p: &Arc<dyn LlmProvider>| {
            let (p, emitted, on_text) = (p.clone(), &emitted, &on_text);
            async move {
                let mut cb = |t: &str| {
                    emitted.store(true, Relaxed);
                    (on_text.lock().unwrap())(t)
                };
                p.complete_stream(session, system, messages, tools, &mut cb).await
            }
        };
        let err = match with_retries(self.primary.name(), PRIMARY_ATTEMPTS, &emitted, || run(&self.primary)).await {
            Ok(c) => return Ok(c),
            Err(e) => e,
        };
        match &self.fallback {
            Some(fb) if is_transient(&err) && !emitted.load(Relaxed) => {
                eprintln!("{}: {err:#}; switching to fallback {}", self.primary.name(), fb.name());
                with_retries(fb.name(), FALLBACK_ATTEMPTS, &emitted, || run(fb)).await.map_err(|e2| {
                    anyhow::anyhow!("{e2:#} (after the primary provider failed: {err:#})")
                })
            }
            _ => Err(err),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::{Block, Role, StopReason, Usage};
    use std::sync::atomic::{AtomicU32, Ordering::SeqCst};

    struct Flaky {
        fail_times: u32,
        error: &'static str,
        calls: AtomicU32,
        label: &'static str,
    }

    #[async_trait]
    impl LlmProvider for Flaky {
        fn name(&self) -> &str {
            self.label
        }
        async fn complete(&self, _: &str, _: &str, _: &[Message], _: &[ToolSpec]) -> Result<Completion> {
            if self.calls.fetch_add(1, SeqCst) < self.fail_times {
                anyhow::bail!(self.error);
            }
            Ok(Completion {
                message: Message { role: Role::Assistant, content: vec![Block::Text(self.label.into())] },
                stop_reason: StopReason::EndTurn,
                usage: Usage::default(),
            })
        }
    }

    fn flaky(label: &'static str, fail_times: u32, error: &'static str) -> Arc<Flaky> {
        Arc::new(Flaky { fail_times, error, calls: AtomicU32::new(0), label })
    }

    #[test]
    fn classifies_errors() {
        let e = |s: &str| anyhow::anyhow!(s.to_string());
        assert!(is_transient(&e("HTTP 429 Too Many Requests: slow down")));
        assert!(is_transient(&e("HTTP 529: overloaded")));
        assert!(is_transient(&e("stream ended before message_stop")));
        assert!(!is_transient(&e("HTTP 400 Bad Request: bad schema")));
        assert!(!is_transient(&e("HTTP 401: unauthorized")));
        assert!(!is_transient(&e("something odd")));
    }

    #[tokio::test(start_paused = true)]
    async fn retries_then_succeeds() {
        let p = flaky("primary", 2, "HTTP 503: down");
        let r = Resilient::new(p.clone(), None);
        let c = r.complete("s", "", &[], &[]).await.unwrap();
        assert_eq!(c.message.text(), "primary");
        assert_eq!(p.calls.load(SeqCst), 3);
    }

    #[tokio::test(start_paused = true)]
    async fn falls_back_after_exhausting_retries() {
        let p = flaky("primary", 99, "HTTP 503: down");
        let f = flaky("fallback", 0, "");
        let r = Resilient::new(p.clone(), Some(f));
        assert_eq!(r.complete("s", "", &[], &[]).await.unwrap().message.text(), "fallback");
        assert_eq!(p.calls.load(SeqCst), PRIMARY_ATTEMPTS);
    }

    #[tokio::test(start_paused = true)]
    async fn fatal_errors_fail_fast_without_fallback() {
        let p = flaky("primary", 99, "HTTP 400: bad request");
        let f = flaky("fallback", 0, "");
        let r = Resilient::new(p.clone(), Some(f.clone()));
        assert!(r.complete("s", "", &[], &[]).await.is_err());
        assert_eq!(p.calls.load(SeqCst), 1);
        assert_eq!(f.calls.load(SeqCst), 0);
    }
}
