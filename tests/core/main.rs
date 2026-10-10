//! The core, tested in-process with fakes behind its contracts: a fake messenger, a fake
//! model provider and fake extensions. See `tests/support/core.rs`.

#[path = "../support/core.rs"]
mod support;

mod extensions;
mod hooks;
mod lifecycle;
mod login;
mod messaging;
mod render;
mod sessions;
mod terminal;
mod turns;
