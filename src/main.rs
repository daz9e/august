mod agent;
mod channels;
mod cli;
mod config;
mod db;
mod gateway;
mod llm;
mod scheduler;
mod skills;
mod tools;
mod util;

use anyhow::Result;

#[tokio::main]
async fn main() -> Result<()> {
    dotenvy::dotenv().ok();

    let Some(cmd) = std::env::args().nth(1) else {
        return channels::terminal::run().await;
    };
    match cmd.as_str() {
        // Foreground gateway; this is what the background service runs.
        "gateway" => gateway::serve().await,
        "serve" => {
            // Fail early so the service is not installed only to crash-loop.
            channels::build_configured()?;
            cli::service::start()
        }
        "stop" => cli::service::stop(),
        "logs" => cli::service::logs(),
        _ => cli::run(&cmd, std::env::args().nth(2).as_deref()).await,
    }
}
