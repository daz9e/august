//! Offline end-to-end tests: the real `august gateway` binary against a fake
//! Telegram Bot API and a fake OpenAI-compatible LLM.

#[path = "../support/mod.rs"]
mod support;

mod media;
mod extensions;
