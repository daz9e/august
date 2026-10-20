//! Slash commands, shared by Telegram and the terminal. `/stop` is handled by the channel,
//! since it has to reach the running turn.

use crate::agent::{self, App, Session};
use crate::config::Config;
use anyhow::Result;
use std::sync::Arc;

pub const LIST: &[(&str, &str)] = &[
    ("new", "Start a new conversation"),
    ("resume", "Go back to an earlier conversation"),
    ("stop", "Stop the current reply"),
    ("model", "Show or set the model"),
    ("effort", "Show or set reasoning effort"),
    ("compact", "Summarize the conversation to free context"),
    ("status", "Model, context and spending"),
    ("update", "Install the latest version"),
    ("help", "List commands"),
];

/// Splits "/cmd@bot args" into ("cmd", "args").
pub fn parse(line: &str) -> Option<(&str, &str)> {
    let rest = line.strip_prefix('/')?;
    let (head, args) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
    let name = head.split('@').next().unwrap_or(head);
    Some((name, args.trim()))
}

pub async fn run(app: &Arc<App>, session: &str, name: &str, args: &str) -> Result<String> {
    Ok(match name {
        "new" | "reset" => {
            agent::archive(session)?;
            app.reset(session);
            "Started a new conversation.".into()
        }
        "stop" => "Nothing to stop.".into(),
        "resume" => {
            let list = agent::archived(session, 10);
            if list.is_empty() {
                "No earlier conversations yet.".into()
            } else {
                list.iter().map(|a| format!("- {}", a.label)).collect::<Vec<_>>().join("\n")
            }
        }
        "model" | "effort" => {
            let mut cfg = Config::load()?;
            if !args.is_empty() {
                if name == "model" {
                    cfg.model = args.into();
                } else {
                    cfg.effort = args.into();
                }
                cfg.save()?;
            }
            format!("{} · {} · effort {}", cfg.provider, cfg.model, cfg.effort)
        }
        "compact" => {
            agent::compact(app, session).await?;
            "Conversation compacted.".into()
        }
        "status" | "usage" => {
            let cfg = Config::load()?;
            let s = Session::load(session)?;
            let st = app.stats(session);
            let k = |n: u64| format!("{:.1}k", n as f64 / 1000.0);
            let mut out = format!("**{}** · {} · effort {}\n", cfg.model, cfg.provider, cfg.effort);
            out += &format!("August {}\n", crate::update::version());
            out += &format!("Conversation: {} messages\n", s.messages.len());
            if st.last.context() > 0 {
                out += &format!("Context: {} of {} before compaction\n", k(st.last.context()), k(cfg.context_tokens));
                out += &format!("Since start: {} in ({} cached), {} out", k(st.total.context()), k(st.total.cache_read), k(st.total.output));
                out += &match st.cost {
                    Some(c) => format!(" · ${c:.2}"),
                    None if cfg.provider == "chatgpt" => " · included in your ChatGPT plan".into(),
                    None => String::new(),
                };
            } else {
                out += "No requests yet since start.";
            }
            out
        }
        "update" if args == "check" => match crate::update::check().await? {
            None => format!("Up to date: {}.", crate::update::version()),
            Some(a) => crate::update::describe(&a),
        },
        "update" => crate::update::update(false).await?,
        "help" => help(),
        "start" => format!("Hi! I'm August, your personal agent. Write to me, or send photos, files and voice messages.\n\n{}", help()),
        _ => format!("Unknown command /{name}. Try /help."),
    })
}

fn help() -> String {
    LIST.iter().map(|(n, d)| format!("/{n} — {d}")).collect::<Vec<_>>().join("\n")
}

#[cfg(test)]
mod tests {
    #[test]
    fn parses_commands() {
        assert_eq!(super::parse("/model@my_bot gpt-5 "), Some(("model", "gpt-5")));
        assert_eq!(super::parse("/new"), Some(("new", "")));
        assert_eq!(super::parse("hello"), None);
    }
}
