//! The wire formats of model APIs (OpenAI chat and responses, Anthropic messages) with
//! their streaming, shared by the provider extensions.

pub mod anthropic;
pub mod openai;
pub mod responses;
pub mod sse;

pub use august_ext::llm::*;

use anyhow::Result;
use async_trait::async_trait;
use serde_json::Value;

pub const USER_AGENT: &str = concat!("august/", env!("CARGO_PKG_VERSION"));

/// Sets each field of `options` (a JSON object) on the request `body`, over what is there.
pub fn merge_options(body: &mut Value, options: &Value) {
    if let (Some(body), Some(options)) = (body.as_object_mut(), options.as_object()) {
        body.extend(options.clone());
    }
}

pub fn http_client() -> reqwest::Client {
    reqwest::Client::builder().user_agent(USER_AGENT).timeout(std::time::Duration::from_secs(600)).build().expect("http client")
}
