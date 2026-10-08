//! August: an always-on personal agent. The core is a small set of primitives
//! (messengers, model providers, tools, hooks, turns, ...) that modules and extensions
//! implement and compose; see `.project/concept.md`.

mod agent;
pub mod messengers;
pub mod cli;
mod config;
mod db;
mod extensions;
pub mod gateway;
mod llm;
mod tools;
mod util;
