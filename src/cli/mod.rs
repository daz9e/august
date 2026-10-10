//! Account commands: `login`, `logout`, `model`, `models`, `status`, `config`.

pub mod service;

use crate::config;
use anyhow::{Result, bail};

pub async fn run(cmd: &str, arg: Option<&str>) -> Result<()> {
    match cmd {
        // Providers live in extensions: these run as commands of a terminal thread.
        // `connect telegram`: messengers in extensions are accounts to sign in to.
        "login" | "logout" | "model" | "models" | "status" | "connect" => {
            let cmd = if cmd == "connect" { "login" } else { cmd };
            let line = [format!("/{cmd}"), arg.unwrap_or_default().to_string()].join(" ");
            crate::messengers::terminal::client::run_with(Some(line.trim())).await
        }
        "config" => config_command(arg, std::env::args().nth(3).as_deref()),
        other => bail!("unknown command: {other} (login | logout | model | models | status | connect | config | serve | stop | logs | gateway)"),
    }
}

/// `august config <path> [value]`: shows (secrets included: it's the owner's terminal) or
/// changes a setting; `null` deletes it. Takes effect when August next reads it (a running
/// gateway: on restart, or right away for what it reads on each use).
fn config_command(path: Option<&str>, value: Option<&str>) -> Result<()> {
    let root = config::Root::new(config::home(), Default::default(), Default::default());
    let Some(path) = path else {
        println!("usage: august config <path> [value]");
        println!("paths: august.<field>, extensions.<name>.settings.<field>");
        println!("files: {}", config::home().join("config").display());
        return Ok(());
    };
    if let Some(v) = value {
        root.set(path, serde_json::from_str(v).unwrap_or_else(|_| serde_json::Value::String(v.into())))?;
    }
    println!("{}", serde_json::to_string_pretty(&root.get(path)?)?);
    Ok(())
}
