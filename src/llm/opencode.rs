//! OpenCode Zen (pay-as-you-go) and OpenCode Go (subscription) gateways.
//! One API key, but each model speaks its own wire format; models.dev says which.

use super::responses::{ApiKey, Responses};
use super::*;
use anyhow::Context;
use std::sync::Arc;

const ZEN_BASE: &str = "https://opencode.ai/zen/v1";
const GO_BASE: &str = "https://opencode.ai/zen/go/v1";
/// Required by OpenCode Go: a stable id per conversation, for routing and caching.
const SESSION_HEADER: &str = "x-opencode-session";
const MODELS_DEV: &str = "https://models.dev/api.json";

#[derive(Debug, Clone, Copy)]
pub enum Plan {
    Zen,
    Go,
}

impl Plan {
    fn base_url(self) -> &'static str {
        match self {
            Plan::Zen => ZEN_BASE,
            Plan::Go => GO_BASE,
        }
    }

    /// Provider id in models.dev.
    fn catalog_id(self) -> &'static str {
        match self {
            Plan::Zen => "opencode",
            Plan::Go => "opencode-go",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    /// `/chat/completions`
    Chat,
    /// `/responses`
    Responses,
    /// `/messages` (Anthropic)
    Messages,
}

impl std::str::FromStr for Format {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self> {
        Ok(match s {
            "chat" => Format::Chat,
            "responses" => Format::Responses,
            "messages" => Format::Messages,
            other => anyhow::bail!("unknown AUGUST_API_FORMAT: {other} (chat | responses | messages)"),
        })
    }
}

/// Looks up the model's AI SDK package in models.dev, the catalog opencode itself uses.
async fn catalog_format(http: &reqwest::Client, plan: Plan, model: &str) -> Result<Option<Format>> {
    let catalog: Value = http
        .get(MODELS_DEV)
        .timeout(std::time::Duration::from_secs(15))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let provider = &catalog[plan.catalog_id()];
    let entry = &provider["models"][model];
    if entry.is_null() {
        return Ok(None);
    }
    let npm = entry["provider"]["npm"]
        .as_str()
        .or(provider["npm"].as_str())
        .unwrap_or_default();
    Ok(Some(match npm {
        "@ai-sdk/anthropic" => Format::Messages,
        "@ai-sdk/openai" => Format::Responses,
        "@ai-sdk/openai-compatible" => Format::Chat,
        other => anyhow::bail!("model {model} uses {other}, which August does not support yet"),
    }))
}

/// Name-based fallback when models.dev is unreachable or doesn't list the model.
fn guess_format(model: &str) -> Format {
    if model.starts_with("claude-") {
        Format::Messages
    } else if model.starts_with("gpt-") {
        Format::Responses
    } else {
        Format::Chat
    }
}

pub async fn provider(
    plan: Plan,
    api_key: String,
    model: String,
    effort: String,
    format: Option<Format>,
) -> Result<Arc<dyn LlmProvider>> {
    let format = match format {
        Some(f) => f,
        None => match catalog_format(&crate::util::http_client(), plan, &model).await {
            Ok(Some(f)) => f,
            Ok(None) | Err(_) => guess_format(&model),
        },
    };
    let base = plan.base_url();
    Ok(match format {
        Format::Messages => Arc::new(
            anthropic::Anthropic::new(base, api_key, model, effort)
                .with_session_header(SESSION_HEADER),
        ),
        Format::Chat => Arc::new(
            openai::OpenAi::new(base.to_string(), api_key, model)
                .with_session_header(SESSION_HEADER),
        ),
        Format::Responses => {
            // Only OpenAI's own models take `reasoning` (grok, muse, ... may reject it).
            let effort = model.starts_with("gpt-").then_some(effort);
            Arc::new(
                Responses::new(base, ApiKey(api_key), model, effort)
                    .with_session_header(SESSION_HEADER),
            )
        }
    })
}

/// Model ids served by the gateway (public endpoint, no key needed). Free-tier
/// models (`*-free`) are left out: OpenCode only serves them to its own client.
pub async fn list_models(plan: Plan) -> Result<Vec<String>> {
    let v: Value = crate::util::http_client()
        .get(format!("{}/models", plan.base_url()))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    v["data"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|m| m["id"].as_str())
                .filter(|id| !id.ends_with("-free"))
                .map(String::from)
                .collect()
        })
        .context("unexpected /models response")
}
