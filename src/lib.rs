//! August: an always-on personal agent. The core is a small set of primitives
//! (messengers, model providers, tools, hooks, turns, ...) that modules and extensions
//! implement and compose; see `.project/concept.md`.

mod agent;
pub mod messengers;
pub mod cli;
mod config;
pub use config::Root;
mod db;
mod extensions;
pub mod gateway;
mod llm;
mod tools;
pub mod util;

use anyhow::Result;

/// The `august` command. A distribution with its own binary (the default extensions next to
/// it) calls this from its `main`.
pub fn main() -> Result<()> {
    dotenvy::dotenv().ok();
    if std::env::args().nth(1).as_deref() == Some("gateway") {
        let path = util::login_path();
        // SAFETY: before the runtime starts, no other thread reads the environment.
        unsafe { std::env::set_var("PATH", path) };
    }
    tokio::runtime::Runtime::new()?.block_on(run())
}

async fn run() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cmd = args.first().map(String::as_str).unwrap_or_default();
    match cmd {
        // Foreground; this is what `serve` and the background service run.
        "gateway" => gateway::serve().await,
        "help" | "--help" | "-h" => cli::help().await,
        "serve" => cli::serve().await,
        "stop" => cli::stop().await,
        "restart" => cli::restart().await,
        "logs" => cli::service::logs(),
        "config" => cli::config_command(args.get(1).map(String::as_str), args.get(2).map(String::as_str)),
        // `august` alone and anything else: what an extension offers.
        _ => cli::open(cmd, args.get(1..).unwrap_or_default()).await,
    }
}
