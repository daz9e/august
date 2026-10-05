//! The terminal REPL: one local chat with plain-text output.

use crate::agent::{self, Agent, Event};
use crate::db::Db;
use crate::llm::providers;
use crate::tools::{Approver, ToolCtx, ToolRegistry};
use anyhow::Result;
use async_trait::async_trait;
use std::io::Write;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, BufReader, Lines, Stdin};
use tokio::sync::Mutex;

type StdinLines = Arc<Mutex<Lines<BufReader<Stdin>>>>;

struct CliApprover(StdinLines);

#[async_trait]
impl Approver for CliApprover {
    async fn approve(&self, action: &str) -> bool {
        print!("\n⚠️  {action}\n   Allow? [y/N] ");
        std::io::stdout().flush().ok();
        let answer = self.0.lock().await.next_line().await.ok().flatten();
        matches!(answer.as_deref().map(str::trim), Some("y" | "Y" | "yes"))
    }
}

fn print_event(e: Event) {
    match e {
        Event::Text(t) => {
            print!("{t}");
            std::io::stdout().flush().ok();
        }
        Event::Step => println!(),
        Event::ToolCall { name, input } => println!("  → {name} {input}"),
        Event::ToolResult { output, is_error } => {
            let first = output.lines().next().unwrap_or("");
            let mark = if is_error { "✗" } else { "✓" };
            println!("  {mark} {}", first.chars().take(120).collect::<String>());
        }
        Event::Compacted { before, after } => eprintln!("  · context compacted ~{before} → ~{after} tokens"),
        Event::Usage(u) => eprintln!(
            "  · tokens in {} (cache {}) / out {}",
            u.input_tokens, u.cache_read_tokens, u.output_tokens
        ),
    }
}

pub async fn run() -> Result<()> {
    let workspace = crate::config::workspace()?;
    let selection = providers::selection()?;
    let provider_id = selection.provider.id;
    let provider = providers::build(selection).await?;
    let db = Db::open()?;
    let stdin: StdinLines = Arc::new(Mutex::new(BufReader::new(tokio::io::stdin()).lines()));
    let ctx = ToolCtx {
        workspace: workspace.clone(),
        approver: Arc::new(CliApprover(stdin.clone())),
        db: db.clone(),
        origin: None,
    };
    let mut agent = Agent::new(
        provider.clone(),
        ToolRegistry::with_defaults(),
        agent::system_prompt(&workspace, "The user reads replies in a terminal (plain text)."),
        db,
        "cli",
    )?;

    println!(
        "august · {provider_id} · {} · workspace {}\n/reset — new session, /compact — summarise old messages, /exit — quit",
        provider.name(),
        workspace.display()
    );

    loop {
        print!("\n> ");
        std::io::stdout().flush()?;
        let Some(line) = stdin.lock().await.next_line().await? else {
            break;
        };
        match line.trim() {
            "" => continue,
            "/exit" | "/quit" => break,
            "/reset" => {
                agent.reset()?;
                println!("session reset");
            }
            "/compact" => match agent.compact(true).await {
                Ok(Some((b, a))) => println!("compacted: ~{b} → ~{a} tokens"),
                Ok(None) => println!("nothing to compact"),
                Err(e) => eprintln!("error: {e:#}"),
            },
            input => match agent.run_turn(input, &ctx, &mut print_event).await {
                Ok(_) => println!(),
                Err(e) => eprintln!("\nerror: {e:#}"),
            },
        }
    }
    Ok(())
}
