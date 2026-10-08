//! Account commands: `login`, `logout`, `model`, `models`, `status`, `config`.

pub mod service;

use crate::config;
use anyhow::{Result, bail};
use dialoguer::{Select, theme::ColorfulTheme};

pub async fn run(cmd: &str, arg: Option<&str>) -> Result<()> {
    match cmd {
        // Providers live in extensions: these run as commands of a terminal thread.
        "login" | "logout" | "model" | "models" | "status" => {
            let line = [format!("/{cmd}"), arg.unwrap_or_default().to_string()].join(" ");
            crate::messengers::terminal::client::run_with(Some(line.trim())).await
        }
        "connect" => connect(arg).await,
        "config" => config_command(arg, std::env::args().nth(3).as_deref()),
        other => bail!("unknown command: {other} (login | logout | model | models | status | connect | config | serve | stop | logs | gateway)"),
    }
}

fn theme() -> ColorfulTheme {
    ColorfulTheme::default()
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
        println!("paths: august.<field>, messengers.<id>.<field>, extensions.<name>.settings.<field>");
        println!("files: {}", config::home().join("config").display());
        return Ok(());
    };
    if let Some(v) = value {
        config::set(path, serde_json::from_str(v).unwrap_or_else(|_| serde_json::Value::String(v.into())))?;
    }
    println!("{}", serde_json::to_string_pretty(&config::get(path)?)?);
    Ok(())
}
