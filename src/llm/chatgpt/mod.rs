//! Responses API billed to the user's ChatGPT plan ("Sign in with ChatGPT").
//! The wire format lives in `responses.rs`; this module only supplies the OAuth token.

pub mod auth;
pub mod codex;

use super::responses::{Responses, TokenSource};
use super::*;
use anyhow::Context;
use auth::{AuthFile, Profile};
use tokio::sync::Mutex;

const API_BASE: &str = "https://api.openai.com/v1";

pub type ChatGpt = Responses<Session>;

/// Signed-in profile; the mutex serializes refreshes (refresh tokens rotate).
pub struct Session {
    http: reqwest::Client,
    profile: Mutex<Profile>,
}

#[async_trait]
impl TokenSource for Session {
    async fn token(&self) -> Result<String> {
        let mut p = self.profile.lock().await;
        auth::ensure_fresh(&self.http, &mut p).await?;
        Ok(p.access_token.clone())
    }

    async fn on_unauthorized(&self) -> Result<bool> {
        let mut p = self.profile.lock().await;
        auth::refresh(&self.http, &mut p).await?;
        Ok(true)
    }
}

fn load_profile() -> Result<Profile> {
    let p = AuthFile::load()?
        .profile
        .context("not signed in to ChatGPT, run `cargo run -- login`")?;
    if !p.has_plan_scope() {
        anyhow::bail!(
            "signed in, but ChatGPT plan usage was not granted; run `cargo run -- login` and allow it"
        );
    }
    Ok(p)
}

/// Models available to the signed-in account (`visibility: "list"` only).
pub async fn list_models(http: &reqwest::Client) -> Result<Vec<String>> {
    let mut profile = load_profile()?;
    auth::ensure_fresh(http, &mut profile).await?;
    let resp = http
        .get(format!("{API_BASE}/models"))
        .bearer_auth(&profile.access_token)
        .send()
        .await?;
    let status = resp.status();
    let v: Value = resp.json().await?;
    if !status.is_success() {
        anyhow::bail!("HTTP {status}: {v}");
    }
    let items = v["models"].as_array().or(v["data"].as_array());
    Ok(items
        .into_iter()
        .flatten()
        .filter(|m| m["visibility"].as_str().is_none_or(|s| s == "list"))
        .filter_map(|m| m["slug"].as_str().or(m["id"].as_str()).map(String::from))
        .collect())
}

/// `model: None` picks the first model the account lists.
pub async fn provider(model: Option<String>, effort: String) -> Result<ChatGpt> {
    let http = crate::util::http_client();
    let model = match model {
        Some(m) => m,
        None => list_models(&http)
            .await?
            .into_iter()
            .next()
            .context("no models are available to this account")?,
    };
    let session = Session {
        http,
        profile: Mutex::new(load_profile()?),
    };
    Ok(Responses::new(API_BASE, session, model, Some(effort)))
}
