//! The `august` command; everything else lives in the library (`lib.rs`).

use august::{cli, gateway, messengers};

use anyhow::Result;

#[tokio::main]
async fn main() -> Result<()> {
    dotenvy::dotenv().ok();

    let Some(cmd) = std::env::args().nth(1) else {
        return messengers::terminal::client::run().await;
    };
    match cmd.as_str() {
        // Foreground gateway; this is what the background service runs.
        "gateway" => gateway::serve().await,
        "serve" => cli::service::start(),
        "stop" => cli::service::stop(),
        "logs" => cli::service::logs(),
        _ => cli::run(&cmd, std::env::args().nth(2).as_deref()).await,
    }
}
