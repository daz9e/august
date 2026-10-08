//! "Sign in with ChatGPT": OAuth PKCE over a loopback redirect, token storage, refresh.
//! https://developers.openai.com/siwc/token-sharing-open-source

use anyhow::{Context, Result, bail};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::time::{SystemTime, UNIX_EPOCH};

const ISSUER: &str = "https://auth.openai.com";
const AUTHORIZE_URL: &str = "https://auth.openai.com/api/accounts/authorize";
const TOKEN_URL: &str = "https://auth.openai.com/api/accounts/oauth/token";
pub const RESOURCE: &str = "https://api.openai.com/v1";
const SCOPE: &str = "openid profile email offline_access resource.invoke chatgpt.tokens.use.direct";
pub const PLAN_SCOPE: &str = "chatgpt.tokens.use.direct";
const APP_NAME: &str = "August";
const CALLBACK_PORT: u16 = 1455;
const CALLBACK_PATH: &str = "/auth/callback";
const REVOKE_URL: &str = "https://auth.openai.com/api/accounts/oauth/revoke";
/// Every sign-in registers with this id; the issued `oaiapp_...` id arrives in the callback.
const DYNAMIC_CLIENT_ID: &str = "dynamic_agent_client";
/// Refresh this long before expiry so a request never starts with a dying token.
const EXPIRY_MARGIN_SECS: u64 = 180;

/// Everything persisted in `~/.august/chatgpt-auth.json` (mode 0600).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AuthFile {
    /// Stable per-machine id, sent as `ext_agent_host_id`.
    pub host_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<Profile>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Profile {
    /// Issued by OpenAI on first sign-in (dynamic registration).
    pub client_id: String,
    pub email: String,
    pub sub: String,
    pub access_token: String,
    pub refresh_token: String,
    pub id_token: String,
    pub scope: String,
    /// Unix seconds.
    pub expires_at: u64,
    #[serde(default)]
    pub earliest_refresh_at: u64,
}

impl Profile {
    pub fn has_plan_scope(&self) -> bool {
        self.scope.split_whitespace().any(|s| s == PLAN_SCOPE)
    }
}

pub(super) fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

pub(super) fn random_b64(n: usize) -> String {
    let mut buf = vec![0u8; n];
    getrandom::fill(&mut buf).expect("os rng");
    B64.encode(buf)
}

fn new_host_id() -> String {
    format!("urn:uuid:{}", crate::new_uuid())
}

const AUTH_FILE: &str = "chatgpt-auth.json";

impl AuthFile {
    pub async fn load() -> Result<Self> {
        crate::load("chatgpt", AUTH_FILE).await
    }

    pub async fn save(&self) -> Result<()> {
        crate::save("chatgpt", self).await
    }
}

#[derive(Deserialize)]
pub(super) struct TokenResponse {
    pub(super) access_token: String,
    pub(super) refresh_token: Option<String>,
    pub(super) id_token: Option<String>,
    pub(super) expires_in: u64,
    #[serde(default)]
    pub(super) scope: String,
    #[serde(default)]
    pub(super) earliest_refresh_at: Option<u64>,
}

async fn token_request(http: &reqwest::Client, form: &[(&str, &str)]) -> Result<TokenResponse> {
    token_request_to(http, TOKEN_URL, form).await
}

pub(super) async fn token_request_to(
    http: &reqwest::Client,
    url: &str,
    form: &[(&str, &str)],
) -> Result<TokenResponse> {
    if std::env::var_os("AUGUST_DEBUG_AUTH").is_some() {
        let safe: Vec<_> = form.iter().filter(|(k, _)| *k != "code_verifier").collect();
        eprintln!("token request: {safe:?}");
    }
    let resp = http
        .post(url)
        .header("accept", "application/json")
        .form(form)
        .send()
        .await?;
    let status = resp.status();
    let text = resp.text().await?;
    if !status.is_success() {
        let code = serde_json::from_str::<Value>(&text)
            .ok()
            .and_then(|v| v["error"].as_str().map(String::from))
            .unwrap_or_default();
        bail!(TokenError { code, detail: format!("HTTP {status}: {text}") });
    }
    Ok(serde_json::from_str(&text)?)
}

#[derive(Debug)]
pub struct TokenError {
    pub code: String,
    detail: String,
}

impl std::fmt::Display for TokenError {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "token endpoint: {}", self.detail)
    }
}
impl std::error::Error for TokenError {}

/// Claims we read from the ID token. The token comes straight from the token endpoint
/// over TLS, so per OIDC Core 3.1.3.7 we check claims without verifying the signature.
pub(super) fn id_claims(id_token: &str) -> Result<Value> {
    let payload = id_token.split('.').nth(1).context("malformed id_token")?;
    Ok(serde_json::from_slice(&B64.decode(payload.trim_end_matches('='))?)?)
}

fn issued_client_id(access_token: &str) -> Result<String> {
    id_claims(access_token)?["client_id"]
        .as_str()
        .map(String::from)
        .context("access token has no client_id")
}

/// Browser sign-in (same flow as pi), run through August: registers a client for this
/// host, exchanges the code with the issued client id, and saves the profile.
pub async fn login(http: &reqwest::Client, steps: &august_ext::Login) -> Result<Profile> {
    let mut file = AuthFile::load().await?;
    if file.host_id.is_empty() {
        file.host_id = new_host_id();
        file.save().await?;
    }

    let verifier = random_b64(32);
    let challenge = B64.encode(Sha256::digest(verifier.as_bytes()));
    let state = random_b64(32);
    let nonce = random_b64(32);
    let redirect_uri = format!("http://127.0.0.1:{CALLBACK_PORT}{CALLBACK_PATH}");

    let mut url = reqwest::Url::parse(AUTHORIZE_URL)?;
    {
        let mut q = url.query_pairs_mut();
        q.append_pair("client_id", DYNAMIC_CLIENT_ID)
            .append_pair("agent_name_hint", APP_NAME)
            .append_pair("ext_agent_host_id", &file.host_id)
            .append_pair("response_type", "code")
            .append_pair("redirect_uri", &redirect_uri)
            .append_pair("scope", SCOPE)
            .append_pair("resource", RESOURCE)
            .append_pair("state", &state)
            .append_pair("nonce", &nonce)
            .append_pair("code_challenge_method", "S256")
            .append_pair("code_challenge", &challenge);
    }

    steps.callback(CALLBACK_PORT, CALLBACK_PATH).await?;
    steps.open(url.as_str(), "Open this link to sign in with ChatGPT:").await?;
    let q = steps.wait_callback(crate::LOGIN_TIMEOUT).await?;
    let get = |k: &str| q[k].as_str().map(str::trim).filter(|v| !v.is_empty());
    if let Some(err) = get("error") {
        bail!("sign-in failed: {err} {}", get("error_description").unwrap_or_default());
    }
    anyhow::ensure!(get("state") == Some(state.as_str()), "OAuth state mismatch");
    let code = get("code").context("missing authorization code")?.to_string();
    let client_id = get("client_id").filter(|c| *c != DYNAMIC_CLIENT_ID).context("the callback did not contain an issued client ID")?.to_string();

    let tok = token_request(
        http,
        &[
            ("grant_type", "authorization_code"),
            ("client_id", &client_id),
            ("code", &code),
            ("code_verifier", &verifier),
            ("redirect_uri", &redirect_uri),
            ("resource", RESOURCE),
        ],
    )
    .await?;

    let id_token = tok.id_token.context("no id_token in token response")?;
    let claims = id_claims(&id_token)?;
    let issued = issued_client_id(&tok.access_token).unwrap_or(client_id);
    if claims["nonce"].as_str() != Some(nonce.as_str()) {
        bail!("id_token nonce mismatch");
    }
    if claims["iss"].as_str() != Some(ISSUER) {
        bail!("unexpected id_token issuer: {}", claims["iss"]);
    }
    let aud_ok = match &claims["aud"] {
        Value::String(a) => *a == issued,
        Value::Array(a) => a.iter().any(|x| x.as_str() == Some(issued.as_str())),
        _ => false,
    };
    if !aud_ok {
        bail!("id_token audience does not match client id");
    }
    if claims["exp"].as_u64().unwrap_or(0) < now() {
        bail!("id_token expired");
    }

    let profile = Profile {
        client_id: issued,
        email: claims["email"].as_str().unwrap_or_default().to_string(),
        sub: claims["sub"].as_str().context("id_token has no sub")?.to_string(),
        access_token: tok.access_token,
        refresh_token: tok.refresh_token.context("no refresh_token (offline_access denied?)")?,
        id_token,
        scope: tok.scope,
        expires_at: now() + tok.expires_in,
        earliest_refresh_at: tok.earliest_refresh_at.unwrap_or(0),
    };
    file.profile = Some(profile.clone());
    file.save().await?;
    Ok(profile)
}

/// Rotates tokens and persists them. Unusable refresh tokens clear the profile.
pub async fn refresh(http: &reqwest::Client, profile: &mut Profile) -> Result<()> {
    let res = token_request(
        http,
        &[
            ("grant_type", "refresh_token"),
            ("client_id", &profile.client_id),
            ("refresh_token", &profile.refresh_token),
            ("resource", RESOURCE),
        ],
    )
    .await;
    let tok = match res {
        Ok(t) => t,
        Err(e) => {
            if let Some(te) = e.downcast_ref::<TokenError>()
                && matches!(te.code.as_str(), "invalid_grant" | "token_expired" | "refresh_token_expired" | "refresh_token_reused")
            {
                let mut file = AuthFile::load().await?;
                file.profile = None;
                file.save().await?;
                bail!("ChatGPT session expired, run /login chatgpt ({e})");
            }
            return Err(e);
        }
    };
    profile.access_token = tok.access_token;
    if let Some(r) = tok.refresh_token {
        profile.refresh_token = r;
    }
    if let Some(i) = tok.id_token {
        profile.id_token = i;
    }
    if !tok.scope.is_empty() {
        profile.scope = tok.scope;
    }
    profile.expires_at = now() + tok.expires_in;
    profile.earliest_refresh_at = tok.earliest_refresh_at.unwrap_or(0);

    let mut file = AuthFile::load().await?;
    file.profile = Some(profile.clone());
    file.save().await
}

/// Refreshes if the access token expires within `EXPIRY_MARGIN_SECS`.
pub async fn ensure_fresh(http: &reqwest::Client, profile: &mut Profile) -> Result<()> {
    if profile.expires_at > now() + EXPIRY_MARGIN_SECS {
        return Ok(());
    }
    refresh(http, profile).await
}

/// Revokes the refresh token (best effort) and forgets the profile; host id is kept.
pub async fn logout(http: &reqwest::Client) -> Result<Option<String>> {
    let mut file = AuthFile::load().await?;
    let Some(p) = file.profile.take() else {
        return Ok(None);
    };
    // Best effort: local credentials are dropped regardless of the outcome.
    let _ = http
        .post(REVOKE_URL)
        .form(&[
            ("token", p.refresh_token.as_str()),
            ("token_type_hint", "refresh_token"),
            ("client_id", p.client_id.as_str()),
        ])
        .send()
        .await;
    file.save().await?;
    Ok(Some(p.email))
}
