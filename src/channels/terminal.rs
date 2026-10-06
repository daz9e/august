//! The terminal REPL: one local chat with plain-text output.

use crate::agent::{self, Agent, Event};
use crate::db::Db;
use crate::extensions::{self, Extensions};
use crate::llm::providers;
use crate::mcp::Mcp;
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

/// Extensions in the terminal: messages are printed, turns can't be queued.
struct TerminalCore;

#[async_trait]
impl extensions::Core for TerminalCore {
    async fn send(&self, _channel: &str, _chat: &str, text: &str) -> Result<()> {
        println!("\n{text}");
        Ok(())
    }

    async fn prompt(&self, _channel: &str, _chat: &str, _text: &str) -> Result<()> {
        anyhow::bail!("queuing turns is not supported in the terminal")
    }
}

struct TerminalNotes;

#[async_trait]
impl crate::tools::Notifier for TerminalNotes {
    async fn notify(&self, text: &str) {
        println!("\n{text}");
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
            "  · tokens in {} (cache read {}, write {}) / out {}",
            u.input_tokens, u.cache_read_tokens, u.cache_write_tokens, u.output_tokens
        ),
    }
}

pub async fn run() -> Result<()> {
    let workspace = crate::config::workspace()?;
    let selection = providers::selection()?;
    let provider_id = selection.provider.id;
    let provider = providers::build(selection).await?;
    let db = Db::open()?;
    let ext = Extensions::new(extensions::dir());
    ext.set_core(Arc::new(TerminalCore));
    let status = ext.reload().await;
    let mcp = Mcp::start().await;
    let stdin: StdinLines = Arc::new(Mutex::new(BufReader::new(tokio::io::stdin()).lines()));
    let ctx = ToolCtx {
        workspace: workspace.clone(),
        approver: Arc::new(CliApprover(stdin.clone())),
        db: db.clone(),
        origin: None,
        files: None,
        extensions: Some(ext.clone()),
        scheduled: false,
        notify: Some(Arc::new(TerminalNotes)),
        inbox: None,
    };
    let mut agent = Agent::new(
        provider.clone(),
        ToolRegistry::with_defaults().with_extensions(ext.clone()).with_mcp(mcp.clone()),
        agent::system_prompt(&workspace, "The user reads replies in a terminal (plain text)."),
        db.clone(),
        "cli",
    )?;

    println!(
        "august · {provider_id} · {} · workspace {}\n/reset — new session, /compact — summarise old messages, /usage — tokens used, /extensions, /reload, /mcp, /exit — quit",
        provider.name(),
        workspace.display()
    );
    if !ext.status().starts_with("No extensions") {
        println!("{status}");
    }
    if !mcp.status().starts_with("No MCP") {
        println!("{}", mcp.status());
    }

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
            "/usage" => match db.usage_report("cli") {
                Ok(r) => println!("{r}"),
                Err(e) => eprintln!("error: {e:#}"),
            },
            "/extensions" => println!("{}", ext.status()),
            "/reload" => println!("{}", ext.reload().await),
            "/mcp" => println!("{}", mcp.status()),
            input => {
                if let Some((name, args)) = crate::channels::parse_command(input, None) {
                    match ext.run_command(&name, &args, &None).await {
                        Some(Ok(reply)) => println!("{}", reply.unwrap_or_default()),
                        Some(Err(e)) => eprintln!("/{name} failed: {e}"),
                        None => println!("unknown command /{name}"),
                    }
                    continue;
                }
                let mut text = input.to_string();
                if ext.listens("message_in") {
                    let data = ext.emit("message_in", serde_json::json!({"text": text}), &None).await;
                    if data["handled"] == true {
                        println!("{}", data["reply"].as_str().unwrap_or(""));
                        continue;
                    }
                    if let Some(t) = data["text"].as_str() {
                        text = t.to_string();
                    }
                }
                match agent.run_turn(&text, &ctx, &mut print_event).await {
                    Ok(_) => println!(),
                    Err(e) => eprintln!("\nerror: {e:#}"),
                }
            }
        }
    }
    Ok(())
}
