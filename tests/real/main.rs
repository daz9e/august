//! Tests against the real thing (the LLM provider configured in `~/.august` or via
//! `AUGUST_*` env vars). Telegram is still faked: a bot can't message itself.
//! Run with `cargo test --test real -- --ignored`.

#[path = "../support/mod.rs"]
mod support;

mod extensions;
mod media;
mod service;
