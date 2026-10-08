//! The everyday slash commands (`/new`, `/stop`, `/model`, `/help`, ...). Each wraps an
//! operation of the core's table and words its reply; the core itself knows no commands.

use anyhow::Result;
use august_ext::{August, Ctx};
use serde_json::{Value, json};

const COMMANDS: &[(&str, &str)] = &[
    ("new", "Start a fresh conversation"),
    ("stop", "Cancel the current task"),
    ("queue", "Run a message as its own turn after the current one"),
    ("compact", "Summarise older messages to free up context"),
    ("usage", "Show token usage of this conversation and today"),
    ("memory", "Show what I remember about you"),
    ("model", "Show or change the model"),
    ("status", "Show provider, model and workspace"),
    ("extensions", "List extensions; enable or disable one"),
    ("config", "Show or change a setting: /config [path [value]]"),
    ("home", "Make this thread the home thread (where extensions report)"),
    ("reload", "Restart all extensions"),
    ("help", "List commands"),
    ("start", "Say hello and list commands"),
];

fn str(v: &Value) -> &str {
    v.as_str().unwrap_or("")
}

/// Every call is the user's: they typed the command.
fn mine(mut params: Value) -> Value {
    params["as_user"] = json!(true);
    params
}

async fn command(august: &August, name: &str, args: &str, ctx: &Ctx) -> Result<String> {
    Ok(match name {
        "start" | "help" => {
            let mut s = String::from("**August** — your personal agent. Just write a message.\n\n");
            for c in august.call("commands", mine(json!({}))).await?.as_array().into_iter().flatten() {
                s += &format!("/{} — {}\n", str(&c["name"]), str(&c["description"]));
            }
            s
        }
        "stop" => match ctx.call("stop", mine(json!({}))).await?["cancelled"].as_u64() {
            Some(0) | None => "Nothing is running.".into(),
            Some(_) => "Stopping…".into(),
        },
        "queue" if args.is_empty() => "Usage: /queue <message>".into(),
        "queue" => {
            ctx.prompt_with(args, json!({"source": "user", "deliver": "followUp"})).await?;
            "📋 Queued.".into()
        }
        "new" => {
            ctx.call("session_new", mine(json!({}))).await?;
            "Started a new conversation.".into()
        }
        "compact" => match ctx.call("compact", mine(json!({}))).await {
            Ok(v) if v.is_null() => "Nothing to compact yet.".into(),
            Ok(v) => format!("Compacted: ~{} → ~{} tokens.", v["before"], v["after"]),
            Err(e) => format!("Could not compact: {e:#}"),
        },
        "memory" => {
            let facts = august.call("memory", mine(json!({}))).await?;
            let lines: Vec<String> = facts.as_array().into_iter().flatten().map(|f| format!("#{} {}", f["id"], str(&f["text"]))).collect();
            if lines.is_empty() { "I haven't saved any facts yet.".into() } else { lines.join("\n") }
        }
        "usage" => {
            let u = ctx.call("usage", mine(json!({}))).await?;
            let line = |u: &Value| {
                format!(
                    "{} calls · in {} · cache read {} · cache write {} · out {}",
                    u["calls"], u["input"], u["cache_read"], u["cache_write"], u["output"]
                )
            };
            format!("This session: {}\nToday, all chats: {}", line(&u["session"]), line(&u["today"]))
        }
        "status" => {
            let s = ctx.call("status", mine(json!({}))).await?;
            let busy = if s["busy"] == true { "yes" } else { "no" };
            format!("Model: `{} · {}`\nWorkspace: `{}`\nBusy: {busy}", str(&s["provider"]), str(&s["model"]), str(&s["workspace"]))
        }
        "model" if args.is_empty() => {
            let s = august.call("status", mine(json!({}))).await?;
            format!("Current model: `{} · {}`\nChange with `/model <id>`.", str(&s["provider"]), str(&s["model"]))
        }
        "model" => match august.call("model_set", mine(json!({"model": args}))).await {
            Ok(m) => format!("Now using `{} · {}` (applies to the next message).", str(&m["provider"]), str(&m["model"])),
            Err(e) => format!("Could not switch model: {e:#}"),
        },
        "extensions" => {
            let changed = match args.split_whitespace().collect::<Vec<_>>()[..] {
                [] => Ok(()),
                ["enable", name] => august.call("extension_enable", mine(json!({"name": name}))).await.map(drop),
                ["disable", name] => august.call("extension_disable", mine(json!({"name": name}))).await.map(drop),
                _ => return Ok("Usage: /extensions [enable|disable <name>]".into()),
            };
            let status = status(&august.call("extensions", mine(json!({}))).await?);
            match changed {
                Ok(()) => status,
                Err(e) => format!("{e:#}\n\n{status}"),
            }
        }
        "config" => {
            let (path, value) = args.split_once(char::is_whitespace).map_or((args, ""), |(p, v)| (p, v.trim()));
            if path.is_empty() {
                return Ok("Usage: /config <path> [value], e.g. /config august.model, /config extensions.web.settings.\n\
                           Units: august, providers.<id>, messengers.<id>, extensions.<name>; `null` deletes."
                    .into());
            }
            if !value.is_empty() {
                let value = serde_json::from_str(value).unwrap_or_else(|_| json!(value));
                august.call("config_set", mine(json!({"path": path, "value": value}))).await?;
            }
            let shown = august.call("config_get", mine(json!({"path": path}))).await?;
            format!("`{path}` = ```\n{}\n```", serde_json::to_string_pretty(&shown)?)
        }
        "home" => {
            august.call("config_set", mine(json!({"path": "august.home", "value": ctx.key()}))).await?;
            "🏠 This is the home thread now: extensions report here.".into()
        }
        "reload" => format!("Extensions reloaded.\n{}", status(&august.call("extensions_reload", mine(json!({}))).await?)),
        _ => unreachable!("not one of ours: {name}"),
    })
}

/// `/extensions`' text for the `extensions` operation's list.
fn status(list: &Value) -> String {
    let all = list.as_array().cloned().unwrap_or_default();
    if all.is_empty() {
        return "No extensions.".into();
    }
    all.iter().map(status_line).collect::<Vec<_>>().join("\n")
}

fn status_line(e: &Value) -> String {
    let name = str(&e["name"]);
    match e["state"].as_str() {
        Some("failed") => format!("❌ {name} — {}", str(&e["error"])),
        Some("disabled") => format!("⏸ {name} — disabled"),
        _ => {
            let mut parts = Vec::new();
            for (key, label, prefix) in [
                ("tools", "tools", ""),
                ("commands", "commands", "/"),
                ("hooks", "hooks", ""),
                ("needs", "needs", ""),
                ("sections", "prompt", ""),
                ("replaces", "replaces built-in", ""),
            ] {
                let items: Vec<String> = e[key].as_array().into_iter().flatten().filter_map(Value::as_str).map(|s| format!("{prefix}{s}")).collect();
                if !items.is_empty() {
                    parts.push(format!("{label}: {}", items.join(", ")));
                }
            }
            if parts.is_empty() {
                parts.push("registers nothing".into());
            }
            format!("✅ {name} — {}", parts.join("; "))
        }
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let august = August::new();
    august.needs(&["messaging", "turns", "sessions", "models", "memory", "admin", "config", "user"]);
    for (name, description) in COMMANDS {
        let a = august.clone();
        august.register_command(name, description, move |args, ctx| {
            let a = a.clone();
            async move { command(&a, name, &args, &ctx).await.map(Some) }
        });
    }
    august.run().await;
}
