mod agent;
mod channel;
mod commands;
mod config;
mod cron;
mod llm;
mod markdown;
mod service;
mod telegram;
mod tools;
mod tui;

use agent::{App, Cancelled};
use anyhow::{Result, bail};
use channel::{Channel, Input};
use clap::{Parser, Subcommand};
use config::Config;
use dialoguer::theme::ColorfulTheme;
use dialoguer::{Confirm, Input as Prompt, Password, Select};
use std::io::{IsTerminal, Write};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

#[derive(Parser)]
#[command(
    name = "august",
    version,
    about = "A personal AI agent for your terminal and Telegram",
    after_help = "Run `august` to chat, `august login` to set things up, `august serve` (or `august service install`) to run the Telegram bot."
)]
struct Cli {
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Chat in the terminal (the default)
    Chat,
    /// Set up a model provider, Telegram or voice transcription
    Login,
    /// Show what is configured and running
    Status,
    /// Send one message in a new conversation; the reply goes to stdout
    Ask {
        #[arg(required = true, trailing_var_arg = true)]
        prompt: Vec<String>,
    },
    /// Run the Telegram bot and scheduled jobs in the foreground
    Serve,
    /// Run `august serve` in the background, starting at login (macOS)
    Service {
        #[command(subcommand)]
        action: ServiceAction,
    },
    /// Follow the log
    Logs,
}

#[derive(Subcommand)]
enum ServiceAction {
    Install,
    Uninstall,
    Restart,
    Status,
}

#[tokio::main]
async fn main() {
    let app = App::new();
    let res = match Cli::parse().cmd.unwrap_or(Cmd::Chat) {
        Cmd::Chat => chat(app).await,
        Cmd::Login => login().await,
        Cmd::Status => status().await,
        Cmd::Ask { prompt } => ask(app, &prompt.join(" ")).await,
        Cmd::Serve => serve(app).await,
        Cmd::Service { action } => service_cmd(action),
        Cmd::Logs => logs(),
    };
    if let Err(e) = res {
        eprintln!("{RED}error:{RESET} {e:#}");
        std::process::exit(1);
    }
}

const DIM: &str = "\x1b[2m";
const BOLD: &str = "\x1b[1m";
const RED: &str = "\x1b[31m";
const GREEN: &str = "\x1b[32m";
const RESET: &str = "\x1b[0m";

/// Plain output for pipes and `august ask`: the reply on stdout, progress on stderr.
struct Plain;

impl Channel for Plain {
    fn text(&self, delta: &str) {
        print!("{delta}");
        std::io::stdout().flush().ok();
    }
    fn tool(&self, summary: &str) {
        eprintln!("{DIM}● {summary}{RESET}");
    }
    /// Asks on the terminal when there is one (`august ask`); denies in pipes.
    fn approve<'a>(&'a self, command: &'a str, reason: &'a str) -> channel::Fut<'a, channel::Decision> {
        Box::pin(async move {
            if !std::io::stdin().is_terminal() {
                eprintln!("{DIM}denied ({reason}): no terminal to ask on{RESET}");
                return channel::Decision::No;
            }
            eprint!("{BOLD}Allow{RESET} ({reason}): {command}\n  [y]es / [a]lways / [N]o: ");
            let answer = tokio::task::spawn_blocking(|| {
                let mut line = String::new();
                std::io::stdin().read_line(&mut line).ok();
                line
            })
            .await
            .unwrap_or_default();
            match answer.trim() {
                "y" | "Y" | "yes" => channel::Decision::Yes,
                "a" | "A" | "always" => channel::Decision::Always,
                _ => channel::Decision::No,
            }
        })
    }
}

fn ensure_provider() -> Result<()> {
    if Config::load()?.provider.is_empty() {
        bail!("no model provider is set up yet, run `august login`");
    }
    Ok(())
}

async fn chat(app: Arc<App>) -> Result<()> {
    if Config::load()?.provider.is_empty() {
        if !std::io::stdin().is_terminal() {
            ensure_provider()?;
        }
        println!("{BOLD}Welcome to August.{RESET} Let's connect a model first.\n");
        setup_provider().await?;
        println!();
    }
    if std::io::stdin().is_terminal() && std::io::stdout().is_terminal() {
        return tui::run(app).await;
    }
    // Not a terminal: one message per line.
    for line in std::io::stdin().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        if let Some((name, args)) = commands::parse(&line) {
            println!("{}", commands::run(&app, "cli", name, args).await?);
            continue;
        }
        match agent::turn(&app, "cli", Input::text(line), &CancellationToken::new(), &Plain).await {
            Ok(_) => println!(),
            Err(e) => eprintln!("{RED}error:{RESET} {e:#}"),
        }
    }
    Ok(())
}

async fn ask(app: Arc<App>, prompt: &str) -> Result<()> {
    ensure_provider()?;
    agent::archive("ask")?;
    let cancel = CancellationToken::new();
    let c = cancel.clone();
    tokio::spawn(async move {
        tokio::signal::ctrl_c().await.ok();
        c.cancel();
    });
    match agent::turn(&app, "ask", Input::text(prompt), &cancel, &Plain).await {
        Ok(_) => {
            println!();
            Ok(())
        }
        Err(e) if e.downcast_ref::<Cancelled>().is_some() => std::process::exit(130),
        Err(e) => Err(e),
    }
}

async fn serve(app: Arc<App>) -> Result<()> {
    agent::VERBOSE.store(true, std::sync::atomic::Ordering::Relaxed);
    ensure_provider()?;
    let cfg = Config::load()?;
    let bot = cfg.telegram_token.as_deref().map(telegram::Bot::new);
    tokio::spawn(cron::run(app.clone(), bot.clone()));
    match bot {
        Some(bot) => telegram::serve(app, bot).await,
        None => {
            agent::log("Telegram is not connected (run `august login`); running scheduled jobs only");
            std::future::pending().await
        }
    }
}

fn service_cmd(action: ServiceAction) -> Result<()> {
    match action {
        ServiceAction::Install => {
            ensure_provider()?;
            service::install()?;
            println!("{GREEN}✓{RESET} August now runs in the background and starts at login. Logs: `august logs`");
        }
        ServiceAction::Uninstall => {
            if service::uninstall()? {
                println!("{GREEN}✓{RESET} Background service removed.");
            } else {
                println!("The background service is not installed.");
            }
        }
        ServiceAction::Restart => {
            service::restart()?;
            println!("{GREEN}✓{RESET} Restarted.");
        }
        ServiceAction::Status => println!("{}", service::status().unwrap_or_else(|| "not installed".into())),
    }
    Ok(())
}

fn logs() -> Result<()> {
    let path = config::home().join("august.log");
    if !path.exists() {
        bail!("no log yet ({})", path.display());
    }
    let status = std::process::Command::new("tail").args(["-n", "100", "-f"]).arg(&path).status()?;
    if !status.success() {
        bail!("tail failed");
    }
    Ok(())
}

async fn status() -> Result<()> {
    let cfg = Config::load()?;
    let ok = |s: &str| format!("{GREEN}●{RESET} {s}");
    let off = |s: &str| format!("{DIM}○ {s}{RESET}");
    let row = |k: &str, v: String| println!("  {k:<11}{v}");
    println!("{BOLD}August{RESET}  {DIM}{}{RESET}\n", config::home().display());
    if cfg.provider.is_empty() {
        row("Model", off("not set up · `august login`"));
    } else {
        row("Model", ok(&format!("{} · {} · effort {}", cfg.model, cfg.provider, cfg.effort)));
    }
    row("Anthropic", if cfg.anthropic_key.is_some() { ok("API key saved") } else { off("no API key") });
    row(
        "ChatGPT",
        match llm::codex::signed_in() {
            Some(email) => ok(&format!("signed in as {email}")),
            None => off("not signed in"),
        },
    );
    let telegram = match &cfg.telegram_token {
        Some(token) => match tokio::time::timeout(std::time::Duration::from_secs(5), telegram::username(token)).await {
            Ok(Ok(name)) => ok(&format!("@{name} · {} allowed user(s)", cfg.telegram_allowed.len())),
            _ => format!("{RED}●{RESET} token saved, but Telegram did not answer"),
        },
        None => off("not connected"),
    };
    row("Telegram", telegram);
    row("Voice", if cfg.openai_key.is_some() { ok(&format!("transcription with {}", cfg.transcribe_model)) } else { off("no transcription (add an OpenAI key in `august login`)") });
    row(
        "Service",
        match service::status() {
            Some(s) if s.starts_with("running") => ok(&s),
            Some(s) => format!("{RED}●{RESET} {s}"),
            None => off("not installed · `august service install`"),
        },
    );
    let flag = |b: bool| if b { "on" } else { "off" };
    row("Safety", format!("approvals for dangerous commands {} · web search {}", flag(cfg.approvals), flag(cfg.web_search)));
    Ok(())
}

// ---------- login ----------

fn theme() -> ColorfulTheme {
    ColorfulTheme::default()
}

fn pick_model(models: Vec<String>, preferred: &str) -> Result<String> {
    if models.is_empty() {
        return Ok(Prompt::with_theme(&theme()).with_prompt("Model id").default(preferred.to_string()).interact_text()?);
    }
    let default = models.iter().position(|m| m == preferred).unwrap_or(0);
    let i = Select::with_theme(&theme()).with_prompt("Model").items(&models).default(default).max_length(12).interact()?;
    Ok(models[i].clone())
}

async fn setup_provider() -> Result<()> {
    let items = ["ChatGPT — sign in with your ChatGPT account", "Anthropic — API key"];
    match Select::with_theme(&theme()).with_prompt("Model provider").items(&items).default(0).interact()? {
        0 => setup_chatgpt().await,
        _ => setup_anthropic().await,
    }
}

async fn setup_anthropic() -> Result<()> {
    let http = llm::http_client();
    let mut cfg = Config::load()?;
    let key = match &cfg.anthropic_key {
        Some(k) if Confirm::with_theme(&theme()).with_prompt("Use the saved Anthropic API key?").default(true).interact()? => k.clone(),
        _ => Password::with_theme(&theme()).with_prompt("Anthropic API key").interact()?.trim().to_string(),
    };
    let models = llm::anthropic::models(&http, &key).await?;
    cfg.model = pick_model(models, "claude-opus-5-5")?;
    cfg.anthropic_key = Some(key);
    cfg.provider = "anthropic".into();
    cfg.save()?;
    println!("{GREEN}✓{RESET} Using {} via Anthropic", cfg.model);
    Ok(())
}

async fn setup_chatgpt() -> Result<()> {
    let http = llm::http_client();
    let email = match llm::codex::signed_in() {
        Some(email) if Confirm::with_theme(&theme()).with_prompt(format!("Stay signed in as {email}?")).default(true).interact()? => email,
        _ => llm::codex::login(&http).await?.email,
    };
    println!("{GREEN}✓{RESET} Signed in as {email}");
    let models = llm::codex::models(&http).await.unwrap_or_else(|e| {
        println!("{DIM}Could not list models ({e:#}); type one in.{RESET}");
        Vec::new()
    });
    let mut cfg = Config::load()?;
    let preferred = if cfg.provider == "chatgpt" { cfg.model.clone() } else { String::new() };
    cfg.model = pick_model(models, &preferred)?;
    cfg.provider = "chatgpt".into();
    cfg.save()?;
    println!("{GREEN}✓{RESET} Using {} via ChatGPT", cfg.model);
    Ok(())
}

async fn setup_telegram() -> Result<()> {
    let mut cfg = Config::load()?;
    println!("{DIM}Create a bot with @BotFather in Telegram (/newbot) and paste its token.{RESET}");
    let token = match &cfg.telegram_token {
        Some(t) if Confirm::with_theme(&theme()).with_prompt("Keep the saved bot token?").default(true).interact()? => t.clone(),
        _ => Password::with_theme(&theme()).with_prompt("Bot token").interact()?.trim().to_string(),
    };
    let (id, name) = telegram::pair(&token, |bot| {
        println!("\nNow open {BOLD}https://t.me/{bot}{RESET} and send the bot any message from your account…");
    })
    .await?;
    if !Confirm::with_theme(&theme()).with_prompt(format!("Allow {name} (id {id}) to use the bot?")).default(true).interact()? {
        bail!("not allowed, nothing saved");
    }
    cfg.telegram_token = Some(token);
    if !cfg.telegram_allowed.contains(&id) {
        cfg.telegram_allowed.push(id);
    }
    cfg.save()?;
    println!("{GREEN}✓{RESET} Telegram connected.");
    if service::status().is_some() {
        service::restart()?;
        println!("{GREEN}✓{RESET} Restarted the background service.");
    } else {
        println!("Run `august service install` to keep the bot running in the background (or `august serve`).");
    }
    Ok(())
}

async fn setup_voice() -> Result<()> {
    let mut cfg = Config::load()?;
    println!("{DIM}Voice messages are transcribed with the OpenAI API (about $0.003 per minute).{RESET}");
    let key = Password::with_theme(&theme()).with_prompt("OpenAI API key (empty to turn transcription off)").allow_empty_password(true).interact()?;
    cfg.openai_key = (!key.trim().is_empty()).then(|| key.trim().to_string());
    cfg.save()?;
    println!("{GREEN}✓{RESET} Voice transcription {}", if cfg.openai_key.is_some() { "on" } else { "off" });
    Ok(())
}

async fn login() -> Result<()> {
    let cfg = Config::load()?;
    let mark = |on: bool, s: String| if on { format!("{s}  ✓") } else { s };
    let items = [
        mark(cfg.provider == "chatgpt", "ChatGPT — sign in with your ChatGPT account".into()),
        mark(cfg.provider == "anthropic", "Anthropic — API key".into()),
        mark(cfg.telegram_token.is_some(), "Telegram bot".into()),
        mark(cfg.openai_key.is_some(), "Voice transcription — OpenAI API key".into()),
    ];
    match Select::with_theme(&theme()).with_prompt("Set up").items(&items).default(0).interact()? {
        0 => setup_chatgpt().await?,
        1 => setup_anthropic().await?,
        2 => return setup_telegram().await,
        _ => return setup_voice().await,
    }
    if Config::load()?.telegram_token.is_none() && Confirm::with_theme(&theme()).with_prompt("Connect a Telegram bot too?").default(false).interact()? {
        setup_telegram().await?;
    }
    Ok(())
}
