//! Legacy sign-in: the Codex CLI's public OAuth client and the `chatgpt.com/backend-api/codex`
//! Responses endpoint. Same wire format as `responses.rs`, billed to the ChatGPT plan.

use super::auth::{TokenError, id_claims, now, random_b64, token_request_to};
use super::*;
use anyhow::bail;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const AUTHORIZE_URL: &str = "https://auth.openai.com/oauth/authorize";
const TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
const REDIRECT_URI: &str = "http://localhost:1455/auth/callback";
const CALLBACK_PORT: u16 = 1455;
const CALLBACK_PATH: &str = "/auth/callback";
const SCOPE: &str = "openid profile email offline_access";
const API_BASE: &str = "https://chatgpt.com/backend-api/codex";
const ORIGINATOR: &str = "august";
const CLAIM: &str = "https://api.openai.com/auth";
const EXPIRY_MARGIN_SECS: u64 = 180;
const AUTH_FILE: &str = "codex-auth.json";

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CodexFile {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<CodexProfile>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CodexProfile {
    pub access_token: String,
    pub refresh_token: String,
    pub account_id: String,
    pub email: String,
    /// Unix seconds.
    pub expires_at: u64,
}

impl CodexFile {
    pub async fn load() -> Result<Self> {
        crate::load("chatgpt-codex", AUTH_FILE).await
    }
    async fn save(&self) -> Result<()> {
        crate::save("chatgpt-codex", self).await
    }
}

pub async fn logout() -> Result<()> {
    let mut f = CodexFile::load().await?;
    f.profile = None;
    f.save().await
}

fn profile_from(tok: super::auth::TokenResponse, old: Option<&CodexProfile>) -> Result<CodexProfile> {
    let claims = id_claims(&tok.access_token)?;
    let account_id = claims[CLAIM]["chatgpt_account_id"]
        .as_str()
        .context("access token has no chatgpt_account_id")?
        .to_string();
    let email = tok
        .id_token
        .as_deref()
        .and_then(|t| id_claims(t).ok())
        .and_then(|c| c["email"].as_str().map(String::from))
        .or_else(|| old.map(|p| p.email.clone()))
        .unwrap_or_default();
    Ok(CodexProfile {
        refresh_token: tok
            .refresh_token
            .or_else(|| old.map(|p| p.refresh_token.clone()))
            .context("no refresh_token in token response")?,
        access_token: tok.access_token,
        account_id,
        email,
        expires_at: now() + tok.expires_in,
    })
}

/// Browser sign-in, run through August.
pub async fn login(http: &reqwest::Client, steps: &august_ext::Login) -> Result<CodexProfile> {
    let verifier = random_b64(32);
    let challenge = B64.encode(Sha256::digest(verifier.as_bytes()));
    let state = random_b64(16);

    let mut url = reqwest::Url::parse(AUTHORIZE_URL)?;
    url.query_pairs_mut()
        .append_pair("response_type", "code")
        .append_pair("client_id", CLIENT_ID)
        .append_pair("redirect_uri", REDIRECT_URI)
        .append_pair("scope", SCOPE)
        .append_pair("code_challenge", &challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("state", &state)
        .append_pair("id_token_add_organizations", "true")
        .append_pair("codex_cli_simplified_flow", "true")
        .append_pair("originator", ORIGINATOR);

    steps.callback(CALLBACK_PORT, CALLBACK_PATH).await?;
    steps.open(url.as_str(), "Open this link to sign in with ChatGPT:").await?;
    let q = steps.wait_callback(crate::LOGIN_TIMEOUT).await?;
    let get = |k: &str| q[k].as_str().map(str::trim).filter(|v| !v.is_empty());
    if let Some(err) = get("error") {
        bail!("sign-in failed: {err} {}", get("error_description").unwrap_or_default());
    }
    anyhow::ensure!(get("state") == Some(state.as_str()), "OAuth state mismatch");
    let code = get("code").context("missing authorization code")?.to_string();
    let tok = token_request_to(
        http,
        TOKEN_URL,
        &[
            ("grant_type", "authorization_code"),
            ("client_id", CLIENT_ID),
            ("code", &code),
            ("code_verifier", &verifier),
            ("redirect_uri", REDIRECT_URI),
        ],
    )
    .await?;
    let profile = profile_from(tok, None)?;
    let mut file = CodexFile::load().await?;
    file.profile = Some(profile.clone());
    file.save().await?;
    Ok(profile)
}

async fn refresh(http: &reqwest::Client, p: &mut CodexProfile) -> Result<()> {
    let res = token_request_to(
        http,
        TOKEN_URL,
        &[
            ("grant_type", "refresh_token"),
            ("client_id", CLIENT_ID),
            ("refresh_token", &p.refresh_token),
        ],
    )
    .await;
    let tok = match res {
        Ok(t) => t,
        Err(e) => {
            if let Some(te) = e.downcast_ref::<TokenError>()
                && matches!(te.code.as_str(), "invalid_grant" | "refresh_token_expired" | "refresh_token_reused")
            {
                logout().await?;
                bail!("ChatGPT session expired, run /login chatgpt-codex ({e})");
            }
            return Err(e);
        }
    };
    *p = profile_from(tok, Some(p))?;
    let mut file = CodexFile::load().await?;
    file.profile = Some(p.clone());
    file.save().await
}

pub struct Session {
    http: reqwest::Client,
    profile: Mutex<CodexProfile>,
    account_id: String,
}

fn headers_for(account_id: &str) -> Vec<(&'static str, String)> {
    vec![
        ("chatgpt-account-id", account_id.to_string()),
        ("originator", ORIGINATOR.to_string()),
        ("OpenAI-Beta", "responses=experimental".to_string()),
    ]
}

#[async_trait]
impl TokenSource for Session {
    async fn token(&self) -> Result<String> {
        let mut p = self.profile.lock().await;
        if p.expires_at <= now() + EXPIRY_MARGIN_SECS {
            refresh(&self.http, &mut p).await?;
        }
        Ok(p.access_token.clone())
    }

    async fn on_unauthorized(&self) -> Result<bool> {
        let mut p = self.profile.lock().await;
        refresh(&self.http, &mut p).await?;
        Ok(true)
    }

    fn headers(&self) -> Vec<(&'static str, String)> {
        headers_for(&self.account_id)
    }
}

async fn load_profile() -> Result<CodexProfile> {
    CodexFile::load().await?
        .profile
        .context("not signed in to ChatGPT (Codex login), run /login chatgpt-codex")
}

/// Best effort: the Codex backend's model list, if it serves one.
pub async fn list_models(http: &reqwest::Client) -> Result<Vec<String>> {
    let mut p = load_profile().await?;
    if p.expires_at <= now() + EXPIRY_MARGIN_SECS {
        refresh(http, &mut p).await?;
    }
    let mut req = http
        .get(format!("{API_BASE}/models?client_version=1.0.0"))
        .bearer_auth(&p.access_token);
    for (k, v) in headers_for(&p.account_id) {
        req = req.header(k, v);
    }
    let resp = req.send().await?;
    let status = resp.status();
    let v: Value = resp.json().await?;
    if !status.is_success() {
        bail!("HTTP {status}: {v}");
    }
    Ok(v["models"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|m| m["visibility"].as_str().is_none_or(|s| s == "list"))
        .filter_map(|m| m["slug"].as_str().or(m["id"].as_str()).map(String::from))
        .collect())
}

/// The signed-in session (one per process: refresh tokens rotate).
pub async fn session() -> Result<Session> {
    let profile = load_profile().await?;
    let account_id = profile.account_id.clone();
    Ok(Session { http: august_llm::http_client(), profile: Mutex::new(profile), account_id })
}

pub fn model(session: std::sync::Arc<Session>, model: String, effort: String) -> Responses<std::sync::Arc<Session>> {
    Responses::new(API_BASE, session, model, Some(effort))
}
