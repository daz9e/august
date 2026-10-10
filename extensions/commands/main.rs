//! The everyday slash commands (`/new`, `/stop`, `/model`, `/help`, ...). Each wraps an
//! operation of the core's table and words its reply; the core itself knows no commands.

use anyhow::Result;
use august_ext::{August, Ctx};
use serde_json::{Value, json};

const COMMANDS: &[(&str, &str)] = &[
    ("new", "Start a fresh conversation"),
    ("stop", "Cancel the current task"),
    ("queue", "Run a message as its own turn after the current one"),
    ("model", "Show or change the model: /model <id> or <provider>:<id>"),
    ("models", "List the models of every provider you are signed in to: /models [filter]"),
    ("login", "Sign in to an account (a model provider: then use it): /login [account]"),
    ("logout", "Sign out of an account: /logout [account]"),
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
        "queue" if args.is_empty() => "Usage: /queue <message>".into(),
        "queue" => {
            ctx.prompt_with(args, json!({"source": "user", "deliver": "followUp"})).await?;
            "📋 Queued.".into()
        }
        "new" => {
            ctx.call("session_new", mine(json!({}))).await?;
            "Started a new conversation.".into()
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
        "model" => {
            let Some(spec) = resolve_model(august, ctx, args).await? else {
                return Ok("Cancelled.".into());
            };
            match august.call("model_set", mine(json!({"model": spec}))).await {
                Ok(m) => format!("Now using `{} · {}` (applies to the next message).", str(&m["provider"]), str(&m["model"])),
                Err(e) => format!("Could not switch model: {e:#}"),
            }
        }
        "models" => {
            let mut s = String::new();
            for (provider, ids) in catalog(august).await? {
                let ids: Vec<&String> = ids.iter().filter(|id| id.contains(args)).collect();
                if ids.is_empty() {
                    continue;
                }
                let shown: Vec<&str> = ids.iter().take(40).map(|id| id.as_str()).collect();
                let more = if ids.len() > shown.len() { format!(", … {} more (narrow with `/models <filter>`)", ids.len() - shown.len()) } else { String::new() };
                s += &format!("**{provider}**: {}{more}\n\n", shown.join(", "));
            }
            if s.is_empty() { "No models match.".into() } else { format!("{s}Switch with `/model <id>` or `/model <provider>:<id>`.") }
        }
        "login" | "logout" => {
            let Some(a) = pick_account(august, ctx, name, args).await? else {
                return Ok("Cancelled.".into());
            };
            let (id, label) = (str(&a["id"]).to_string(), str(&a["label"]).to_string());
            if name == "logout" {
                return Ok(match august.call("logout", mine(json!({"account": id}))).await {
                    Ok(_) => format!("Signed out of {label}."),
                    Err(e) => format!("Could not sign out: {e:#}"),
                });
            }
            let signed = match ctx.call("login", mine(json!({"account": id}))).await {
                Ok(v) => v,
                Err(e) => return Ok(format!("Sign-in failed: {e:#}")),
            };
            let done = match signed["who"].as_str() {
                Some(who) => format!("Signed in to {label} as {who}."),
                None => format!("Signed in to {label}."),
            };
            let Some(provider) = a["providers"][0].as_str() else { return Ok(done) };
            match august.call("model_set", mine(json!({"model": format!("{provider}:")}))).await {
                Ok(m) => format!("{done}\nNow using `{} · {}`. `/models` lists the others, `/model <id>` switches.", str(&m["provider"]), str(&m["model"])),
                Err(e) => format!("{done}\nCould not switch to it: {e:#}"),
            }
        }
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
                           Units: august, extensions.<name>; `null` deletes."
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

/// Providers the user can use (one of their accounts is connected), the active one first:
/// each with its models (a provider that can't list them is left out).
async fn catalog(august: &August) -> Result<Vec<(String, Vec<String>)>> {
    let active = august.call("status", mine(json!({}))).await?["provider"].as_str().unwrap_or_default().to_string();
    let accounts = august.call("accounts", mine(json!({}))).await?;
    let mut providers: Vec<String> = vec![active];
    for a in accounts.as_array().into_iter().flatten().filter(|a| a["status"] == "connected") {
        providers.extend(a["providers"].as_array().into_iter().flatten().filter_map(|p| p.as_str().map(String::from)));
    }
    let mut seen = std::collections::HashSet::new();
    providers.retain(|p| !p.is_empty() && seen.insert(p.clone()));
    let mut out = Vec::new();
    for p in providers {
        if let Ok(list) = august.call("models", mine(json!({"provider": p}))).await {
            out.push((p, list.as_array().into_iter().flatten().filter_map(|m| m["id"].as_str().map(String::from)).collect()));
        }
    }
    Ok(out)
}

/// What `/model <args>` switches to: `provider:model` as given, else the provider offering
/// that model (asked which, when several do). `None`: the user didn't pick.
async fn resolve_model(august: &August, ctx: &Ctx, args: &str) -> Result<Option<String>> {
    let catalog = catalog(august).await?;
    if args.split_once(':').is_some_and(|(p, _)| catalog.iter().any(|(id, _)| id == p)) {
        return Ok(Some(args.into()));
    }
    let holders: Vec<String> = catalog.into_iter().filter(|(_, ids)| ids.iter().any(|id| id == args)).map(|(p, _)| p).collect();
    Ok(match holders.len() {
        // Not listed anywhere: the active provider may still take it.
        0 => Some(args.into()),
        1 => Some(format!("{}:{args}", holders[0])),
        _ => {
            let question = format!("`{args}` is offered by more than one provider. Which one?");
            ctx.ask(&question, &holders, std::time::Duration::from_secs(300)).await?.map(|p| format!("{p}:{args}"))
        }
    })
}

/// `/stop`: "Stopping…", edited to "Stopped." once nothing runs in the thread any more.
async fn stop(august: &August, ctx: &Ctx) -> Result<Option<String>> {
    if ctx.call("stop", mine(json!({}))).await?["cancelled"].as_u64().unwrap_or(0) == 0 {
        return Ok(Some("Nothing is running.".into()));
    }
    let id = ctx.send("Stopping…").await?;
    for _ in 0..300 {
        if ctx.call("status", mine(json!({}))).await?["busy"] != true {
            august.edit(ctx.thread.as_ref().unwrap(), &id, "Stopped.").await?;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    Ok(None)
}

/// `/extensions`' text for the `extensions` operation's list.
fn status(list: &Value) -> String {
    let all = list.as_array().cloned().unwrap_or_default();
    if all.is_empty() {
        return "No extensions.".into();
    }
    all.iter().map(status_line).collect::<Vec<_>>().join("\n")
}

/// The account named in `args`, or the one the user picks from buttons (✓: signed in).
async fn pick_account(august: &August, ctx: &Ctx, action: &str, args: &str) -> Result<Option<Value>> {
    let all = august.call("accounts", mine(json!({}))).await?;
    let mut all = all.as_array().cloned().unwrap_or_default();
    if action == "logout" {
        all.retain(|a| a["status"] != "none");
    }
    if !args.is_empty() {
        return match all.iter().find(|a| a["id"] == args) {
            Some(a) => Ok(Some(a.clone())),
            None => anyhow::bail!("no account `{args}` (have: {})", all.iter().map(|a| str(&a["id"])).collect::<Vec<_>>().join(", ")),
        };
    }
    anyhow::ensure!(!all.is_empty(), "no accounts to {action}");
    let mark = |a: &Value| match str(&a["status"]) {
        "connected" => " ✓",
        "expired" => " ⚠",
        _ => "",
    };
    let labels: Vec<String> = all.iter().map(|a| format!("{}{}", str(&a["label"]), mark(a))).collect();
    let verb = if action == "login" { "Sign in to" } else { "Sign out of" };
    let picked = ctx.ask(&format!("{verb} which account?"), &labels, std::time::Duration::from_secs(300)).await?;
    Ok(picked.and_then(|l| labels.iter().position(|x| *x == l).map(|i| all[i].clone())))
}

fn status_line(e: &Value) -> String {
    let name = str(&e["name"]);
    match e["state"].as_str() {
        Some("failed") => format!("❌ {name} — {}", str(&e["error"])),
        Some("disabled") => format!("⏸ {name} — disabled"),
        Some("starting") => format!("⏳ {name} — starting"),
        _ => {
            let mut parts = Vec::new();
            for (key, label, prefix) in [
                ("tools", "tools", ""),
                ("commands", "commands", "/"),
                ("hooks", "hooks", ""),
                ("events", "emits", ""),
                ("needs", "needs", ""),
                ("sections", "prompt", ""),
                ("replaces", "replaces built-in", ""),
            ] {
                let items: Vec<String> = e[key].as_array().into_iter().flatten().filter_map(|x| x.as_str().or(x["name"].as_str())).map(|s| format!("{prefix}{s}")).collect();
                if !items.is_empty() {
                    parts.push(format!("{label}: {}", items.join(", ")));
                }
            }
            if parts.is_empty() {
                parts.push("registers nothing".into());
            }
            if let Some(kb) = e["memory_kb"].as_u64() {
                parts.push(format!("{:.1} MB", kb as f64 / 1024.0));
            }
            match e["health"]["detail"].as_str() {
                Some(detail) if e["state"] == "degraded" => format!("⚠️ {name} — degraded: {detail}; {}", parts.join("; ")),
                _ => format!("✅ {name} — {}", parts.join("; ")),
            }
        }
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    serve(August::new()).await
}

async fn serve(august: August) {
    august.needs(&["messaging", "turns", "sessions", "models", "admin", "config", "user"]);
    for (name, description) in COMMANDS {
        let a = august.clone();
        august.register_command(name, description, move |args, ctx| {
            let a = a.clone();
            async move {
                match *name {
                    "stop" => stop(&a, &ctx).await,
                    _ => command(&a, name, &args, &ctx).await.map(Some),
                }
            }
        });
    }
    august.run().await;
}

#[cfg(test)]
mod tests;
