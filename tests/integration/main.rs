//! Offline end-to-end tests: the real `august gateway` binary against a fake
//! Telegram Bot API and a fake OpenAI-compatible LLM.

#[path = "../support/mod.rs"]
mod support;

mod claude_cli;
mod media;
mod extensions;
mod memory;
mod compaction;
mod skills;
mod review;
mod inbox;
mod limits;
mod tasks;
mod subtasks;
mod goal;
mod usage;
mod mcp;
mod browser;
mod web;
mod clarify;
