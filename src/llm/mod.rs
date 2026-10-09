//! Model providers: the types and the `LlmProvider` trait live in the SDK
//! (`august_ext::llm`), the wire formats in `august-llm`.

pub mod providers;
pub mod remote;

pub use august_ext::llm::*;

use anyhow::Result;
use async_trait::async_trait;
use serde_json::Value;
