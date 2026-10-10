//! The `august` program: its own commands (`help`, `serve`, `stop`, `restart`, `logs`,
//! `config`) and the ones extensions add (`open`), reached over the core's control socket.

pub mod service;

use crate::config;
use crate::gateway::control;
use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};
use std::os::unix::process::CommandExt;
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

/// How long August may take to start or stop.
const WAIT: Duration = Duration::from_secs(30);

const OWN: &[(&str, &str)] = &[
    ("serve", "Run August in the background, started at login"),
    ("stop", "Stop August and its extensions (and remove it from autostart)"),
    ("restart", "Stop August and its extensions, then start them again"),
    ("logs", "Follow August's log"),
    ("config", "Show or change a setting: august config <path> [value]"),
    ("help", "This list"),
];

fn socket() -> std::path::PathBuf {
    control::socket_in(&config::home())
}

async fn running() -> bool {
    UnixStream::connect(socket()).await.is_ok()
}

/// One request to the running core.
async fn request(req: Value) -> Result<Value> {
    let stream = UnixStream::connect(socket()).await.context("August is not running")?;
    let (read, mut write) = stream.into_split();
    write.write_all(format!("{req}\n").as_bytes()).await?;
    let line = BufReader::new(read).lines().next_line().await?.ok_or_else(|| anyhow!("August went away"))?;
    let reply: Value = serde_json::from_str(&line)?;
    match reply["error"].as_str() {
        Some(e) => Err(anyhow!("{e}")),
        None => Ok(reply["result"].clone()),
    }
}

/// Waits until August is `up` (or down).
async fn wait(up: bool) -> Result<()> {
    let started = Instant::now();
    while running().await != up {
        if started.elapsed() > WAIT {
            bail!("August did not {}; see {}", if up { "start" } else { "stop" }, service::log_path().display());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    Ok(())
}

/// Starts August (the service if installed, else on its own in the background).
async fn start() -> Result<()> {
    if service::installed() {
        service::kick()?;
    } else {
        service::spawn()?;
    }
    wait(true).await
}

/// Stops the running August, telling its extensions `reason`.
async fn shut_down(reason: &str) -> Result<()> {
    request(json!({"op": "shutdown", "reason": reason})).await?;
    wait(false).await
}

pub async fn help() -> Result<()> {
    println!("usage: august [command] [args...]\n");
    println!("  {:<10} {}", "(none)", "Open the terminal interface an extension offers");
    for (name, about) in OWN {
        println!("  {name:<10} {about}");
    }
    if !running().await {
        println!("\nStart August (august serve) to list the commands its extensions add.");
        return Ok(());
    }
    let added = request(json!({"op": "cli"})).await?;
    let added: Vec<&Value> = added.as_array().into_iter().flatten().filter(|c| c["name"] != "").collect();
    if !added.is_empty() {
        println!("\nfrom extensions:");
        for c in added {
            let (name, about, owner) = (c["name"].as_str().unwrap_or_default(), c["description"].as_str().unwrap_or_default(), c["owner"].as_str().unwrap_or_default());
            println!("  {name:<10} {about} ({owner})");
        }
    }
    Ok(())
}

pub async fn serve() -> Result<()> {
    // One started on its own gives way to the service.
    if running().await && !service::installed() {
        shut_down("restart").await?;
    }
    service::start()
}

pub async fn stop() -> Result<()> {
    let (up, installed) = (running().await, service::installed());
    if !up && !installed {
        println!("August is not running");
        return Ok(());
    }
    if up {
        shut_down("stop").await?;
    }
    if installed {
        service::uninstall()?;
    }
    println!("August stopped");
    Ok(())
}

pub async fn restart() -> Result<()> {
    if !running().await {
        println!("August is not running (start it with `august serve`)");
        return Ok(());
    }
    shut_down("restart").await?;
    start().await?;
    println!("August restarted");
    Ok(())
}

/// `august <name> args...` (`""`: `august` alone): runs what the extension offering it
/// registered, in this terminal, starting August first if it isn't running.
pub async fn open(name: &str, args: &[String]) -> Result<()> {
    if !running().await {
        eprintln!("starting August…");
        start().await?;
    }
    let found = request(json!({"op": "cli_open", "name": name})).await;
    let Ok(found) = found else {
        if name.is_empty() {
            bail!("no terminal interface: `august` alone opens what an extension offers for it (the default `terminal`, or your own registered as \"\")");
        }
        bail!("unknown command `{name}` (see `august help`)");
    };
    let exec: Vec<String> = serde_json::from_value(found["exec"].clone())?;
    let (program, rest) = exec.split_first().ok_or_else(|| anyhow!("`{name}` has nothing to run"))?;
    let err = std::process::Command::new(program)
        .args(rest)
        .args(args)
        .env("AUGUST_SOCKET", socket())
        .env("AUGUST_TOKEN", found["token"].as_str().unwrap_or_default())
        .exec();
    Err(anyhow!("could not run {program}: {err}"))
}

/// `august config <path> [value]`: shows (secrets included: it's the owner's terminal) or
/// changes a setting; `null` deletes it. Takes effect when August next reads it (a running
/// August: on restart, or right away for what it reads on each use).
pub fn config_command(path: Option<&str>, value: Option<&str>) -> Result<()> {
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
