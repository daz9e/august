//! Account commands: `login`, `logout`, `model`, `models`, `status`, `config`.

pub mod service;

use crate::llm::chatgpt::{auth, codex};
use crate::llm::providers::{self, Auth, ProviderDef, registry};
use crate::config::{self, ApiCredential, Config, Credentials};
use anyhow::{Result, bail};
use dialoguer::{Confirm, FuzzySelect, Input, Password, Select, theme::ColorfulTheme};

pub async fn run(cmd: &str, arg: Option<&str>) -> Result<()> {
    match cmd {
        "login" => login().await,
        "logout" => logout().await,
        "model" => {
            let sel = providers::selection()?;
            choose_model(sel.provider.def, sel.model.as_deref()).await
        }
        "models" => {
            let sel = providers::selection()?;
            let cred = providers::credential(sel.provider.def)?;
            for m in providers::list_models(sel.provider.def, cred.as_ref()).await? {
                println!("{m}");
            }
            Ok(())
        }
        "status" => status(),
        "connect" => connect(arg).await,
        "config" => config_command(arg, std::env::args().nth(3).as_deref()),
        other => bail!("unknown command: {other} (login | logout | model | models | status | connect | config | serve | stop | logs | gateway)"),
    }
}

fn theme() -> ColorfulTheme {
    ColorfulTheme::default()
}

fn is_connected(p: &dyn ProviderDef, creds: &Credentials) -> Result<bool> {
    Ok(match p.auth() {
        Auth::OAuth => auth::AuthFile::load()?.profile.is_some(),
        Auth::CodexOAuth => codex::is_signed_in()?,
        _ => creds.contains_key(p.id()),
    })
}

async fn login() -> Result<()> {
    let creds: Credentials = config::credentials()?;
    let labels = registry()
        .iter()
        .map(|p| {
            let mark = if is_connected(*p, &creds)? { "  ✓ connected" } else { "" };
            Ok(format!("{}{mark}", p.label()))
        })
        .collect::<Result<Vec<_>>>()?;
    let idx = Select::with_theme(&theme())
        .with_prompt("Provider")
        .items(&labels)
        .default(0)
        .interact()?;
    let p = registry()[idx];

    match p.auth() {
        Auth::OAuth => {
            let profile = auth::login(&crate::util::http_client()).await?;
            println!("signed in as {}", profile.email);
            if !profile.has_plan_scope() {
                bail!("ChatGPT plan usage was not granted, the chatgpt provider will not work");
            }
        }
        Auth::CodexOAuth => {
            let profile = codex::login(&crate::util::http_client()).await?;
            println!("signed in as {}", profile.email);
        }
        Auth::Cli => {
            crate::llm::claude_cli::check_installed().await?;
            println!("using `{}`; sign in there with `claude auth login` if needed", crate::llm::claude_cli::bin());
        }
        Auth::ApiKey | Auth::KeyAndUrl => {
            let existing = creds.get(p.id()).cloned();
            let keep = existing.is_some()
                && Confirm::with_theme(&theme())
                    .with_prompt("A key is already saved. Keep it?")
                    .default(true)
                    .interact()?;
            if !keep {
                let cred = ask_credential(p, existing.as_ref())?;
                // Fail before saving if the key is rejected (where the API can tell).
                if p.id() == "anthropic" {
                    providers::list_models(p, Some(&cred)).await?;
                }
                config::save_credential(p.id(), Some(&cred))?;
                println!("saved to {}", config::home().join(format!("config/providers/{}.json", p.id())).display());
            }
        }
    }

    let cfg: Config = config::app()?;
    let current = (cfg.provider.as_deref() == Some(p.id()))
        .then_some(cfg.model.as_deref())
        .flatten();
    choose_model(p, current.or(p.default_model())).await
}

fn ask_credential(p: &dyn ProviderDef, existing: Option<&ApiCredential>) -> Result<ApiCredential> {
    let base_url = if p.auth() == Auth::KeyAndUrl {
        let default = existing
            .and_then(|c| c.base_url.clone())
            .unwrap_or_else(|| "https://api.openai.com/v1".into());
        Some(
            Input::<String>::with_theme(&theme())
                .with_prompt("Base URL")
                .default(default)
                .interact_text()?,
        )
    } else {
        None
    };
    let key = Password::with_theme(&theme())
        .with_prompt(if p.auth() == Auth::KeyAndUrl {
            "API key (empty for local servers)"
        } else {
            "API key"
        })
        .allow_empty_password(p.auth() == Auth::KeyAndUrl)
        .interact()?
        .trim()
        .to_string();
    Ok(ApiCredential { key, base_url })
}

/// Picks a model from the provider's list (or a typed id) and makes it the default.
async fn choose_model(p: &dyn ProviderDef, current: Option<&str>) -> Result<()> {
    let cred = providers::credential(p)?;
    let models = match providers::list_models(p, cred.as_ref()).await {
        Ok(m) => m,
        Err(e) => {
            println!("could not list models: {e:#}");
            Vec::new()
        }
    };
    const OTHER: &str = "other (type a model id)";
    let model = if models.is_empty() {
        type_model(current)?
    } else {
        let mut items = models.clone();
        items.push(OTHER.into());
        let default = current
            .and_then(|c| models.iter().position(|m| m == c))
            .unwrap_or(0);
        let idx = FuzzySelect::with_theme(&theme())
            .with_prompt("Model (type to filter)")
            .items(&items)
            .default(default)
            .interact()?;
        if idx == models.len() {
            type_model(current)?
        } else {
            models[idx].clone()
        }
    };

    let mut cfg: Config = config::app()?;
    cfg.provider = Some(p.id().to_string());
    cfg.model = Some(model.clone());
    config::save_app(&cfg)?;
    println!("using {} · {model}", p.id());
    Ok(())
}

fn type_model(current: Option<&str>) -> Result<String> {
    let theme = theme();
    let mut input = Input::<String>::with_theme(&theme).with_prompt("Model id");
    if let Some(c) = current {
        input = input.default(c.to_string());
    }
    Ok(input.interact_text()?.trim().to_string())
}

async fn logout() -> Result<()> {
    let creds: Credentials = config::credentials()?;
    let connected: Vec<&dyn ProviderDef> = registry()
        .iter()
        .filter(|p| is_connected(**p, &creds).unwrap_or(false))
        .copied()
        .collect();
    if connected.is_empty() {
        println!("no saved credentials");
        return Ok(());
    }
    let labels: Vec<_> = connected.iter().map(|p| p.label()).collect();
    let idx = Select::with_theme(&theme())
        .with_prompt("Log out of")
        .items(&labels)
        .default(0)
        .interact()?;
    let p = connected[idx];

    if p.auth() == Auth::OAuth {
        auth::logout(&crate::util::http_client()).await?;
    } else if p.auth() == Auth::CodexOAuth {
        codex::logout()?;
    } else {
        config::save_credential(p.id(), None)?;
    }
    let mut cfg: Config = config::app()?;
    if cfg.provider.as_deref() == Some(p.id()) {
        cfg.provider = None;
        cfg.model = None;
        config::save_app(&cfg)?;
    }
    println!("logged out of {}", p.id());
    Ok(())
}

fn status() -> Result<()> {
    let creds: Credentials = config::credentials()?;
    match providers::selection() {
        Ok(sel) => println!(
            "active: {} · {} · effort {}",
            sel.provider.id,
            sel.model.as_deref().unwrap_or("(no model)"),
            sel.effort
        ),
        Err(e) => println!("active: none ({e})"),
    }
    for p in registry().iter().copied() {
        let env_set = !p.key_env().is_empty() && std::env::var(p.key_env()).is_ok_and(|v| !v.is_empty());
        let state = match (is_connected(p, &creds)?, env_set) {
            (_, true) => format!("key from ${}", p.key_env()),
            (true, false) => "connected".into(),
            (false, false) => "-".into(),
        };
        println!("  {:<12} {state}", p.id());
    }
    Ok(())
}

/// Sets up a messenger (Telegram, ...): token, owner pairing, allowlist.
async fn connect(which: Option<&str>) -> Result<()> {
    let defs = crate::messengers::registry();
    let def = match which {
        Some(id) => crate::messengers::def(id).ok_or_else(|| {
            let ids: Vec<_> = defs.iter().map(|d| d.id()).collect();
            anyhow::anyhow!("unknown messenger: {id} ({})", ids.join(" | "))
        })?,
        None => {
            let labels = defs
                .iter()
                .map(|d| {
                    let mark = if d.is_configured().unwrap_or(false) { "  ✓ connected" } else { "" };
                    format!("{}{mark}", d.label())
                })
                .collect::<Vec<_>>();
            let idx = Select::with_theme(&theme())
                .with_prompt("Messenger")
                .items(&labels)
                .default(0)
                .interact()?;
            defs.into_iter().nth(idx).unwrap()
        }
    };
    def.setup().await
}

/// `august config <path> [value]`: shows (secrets included: it's the owner's terminal) or
/// changes a setting; `null` deletes it. Takes effect when August next reads it (a running
/// gateway: on restart, or right away for what it reads on each use).
fn config_command(path: Option<&str>, value: Option<&str>) -> Result<()> {
    let Some(path) = path else {
        println!("usage: august config <path> [value]");
        println!("paths: august.<field>, providers.<id>.<field>, messengers.<id>.<field>, extensions.<name>.settings.<field>");
        println!("files: {}", config::home().join("config").display());
        return Ok(());
    };
    if let Some(v) = value {
        config::set(path, serde_json::from_str(v).unwrap_or_else(|_| serde_json::Value::String(v.into())))?;
    }
    println!("{}", serde_json::to_string_pretty(&config::get(path)?)?);
    Ok(())
}
