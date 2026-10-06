//! The terminal REPL: one local chat with plain-text output.

use crate::agent::{self, Agent, Event};
use crate::db::Db;
use crate::extensions::{self, Extensions};
use crate::llm::providers;
use crate::tools::{Approver, ToolCtx, ToolRegistry};
use anyhow::Result;
use async_trait::async_trait;
use serde_json::Value;
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
struct TerminalCore {
    provider: Arc<dyn crate::llm::LlmProvider>,
    workspace: std::path::PathBuf,
    db: Arc<Db>,
    ext: std::sync::Weak<Extensions>,
    stdin: StdinLines,
}

#[async_trait]
impl extensions::Core for TerminalCore {
    async fn send(&self, _channel: &str, _chat: &str, text: &str) -> Result<()> {
        println!("\n{text}");
        Ok(())
    }

    async fn prompt(&self, _channel: &str, _chat: &str, _text: &str) -> Result<()> {
        anyhow::bail!("queuing turns is not supported in the terminal")
    }

    async fn agent(&self, _channel: &str, _chat: &str, _task: &str, _opts: extensions::AgentOpts) -> Result<String> {
        anyhow::bail!("sub-agents are not supported in the terminal")
    }

    async fn ask(&self, _channel: &str, _chat: &str, question: &str, options: &[String]) -> Result<Option<String>> {
        println!("\n❓ {question}");
        for (i, o) in options.iter().enumerate() {
            println!("   {}. {o}", i + 1);
        }
        print!("   Choose 1-{}: ", options.len());
        std::io::stdout().flush().ok();
        let answer = self.stdin.lock().await.next_line().await.ok().flatten();
        let pick = answer.and_then(|a| a.trim().parse::<usize>().ok()).and_then(|n| n.checked_sub(1));
        Ok(pick.and_then(|i| options.get(i).cloned()))
    }

    async fn approve(&self, _channel: &str, _chat: &str, action: &str) -> Result<bool> {
        Ok(CliApprover(self.stdin.clone()).approve(action).await)
    }

    async fn call_tool(&self, _channel: &str, _chat: &str, name: &str, input: &Value) -> Result<(String, bool)> {
        let ext = self.ext.upgrade().ok_or_else(|| anyhow::anyhow!("August is shutting down"))?;
        let ctx = ToolCtx {
            workspace: self.workspace.clone(),
            approver: Arc::new(CliApprover(self.stdin.clone())),
            db: self.db.clone(),
            origin: None,
            files: None,
            extensions: Some(ext.clone()),
            unattended: false,
            notify: None,
            inbox: None,
        };
        let tools = ToolRegistry::with_defaults().with_extensions(ext);
        Ok(tools.call(name, input, &ctx).await)
    }

    async fn llm(&self, prompt: &str, system: &str) -> Result<String> {
        let messages = [crate::llm::Message::user_text(prompt)];
        Ok(self.provider.complete(&crate::util::new_uuid(), system, &messages, &[]).await?.message.text())
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
    let stdin: StdinLines = Arc::new(Mutex::new(BufReader::new(tokio::io::stdin()).lines()));
    ext.set_core(Arc::new(TerminalCore {
        provider: provider.clone(),
        workspace: workspace.clone(),
        db: db.clone(),
        ext: Arc::downgrade(&ext),
        stdin: stdin.clone(),
    }));
    let status = ext.reload().await;
    let ctx = ToolCtx {
        workspace: workspace.clone(),
        approver: Arc::new(CliApprover(stdin.clone())),
        db: db.clone(),
        origin: None,
        files: None,
        extensions: Some(ext.clone()),
        unattended: false,
        notify: Some(Arc::new(TerminalNotes)),
        inbox: None,
    };
    let mut agent = Agent::new(
        provider.clone(),
        ToolRegistry::with_defaults().with_extensions(ext.clone()),
        agent::system_prompt(&workspace, "The user reads replies in a terminal (plain text)."),
        db.clone(),
        "cli",
    )?;

    println!(
        "august · {provider_id} · {} · workspace {}\n/reset — new session, /compact — summarise old messages, /usage — tokens used, /extensions [enable|disable <name>], /reload, /mcp, /exit — quit",
        provider.name(),
        workspace.display()
    );
    if !ext.status().starts_with("No extensions") {
        println!("{status}");
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
            cmd if cmd.split_whitespace().next() == Some("/extensions") => {
                println!("{}", ext.command(&cmd["/extensions".len()..]).await)
            }
            "/reload" => println!("{}", ext.reload().await),
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
                    let data = ext.emit("message_in", serde_json::json!({"text": text, "files": []}), &None).await;
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
