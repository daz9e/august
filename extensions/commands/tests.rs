//! `commands` against a fake core: each slash command calls its operation as the user and
//! words the reply.

use super::serve;
use august_ext::FakeAugust;
use august_ext::fake::{ctx, thread};
use serde_json::json;

async fn commands() -> FakeAugust {
    let (fake, august) = FakeAugust::new(&[]);
    tokio::spawn(serve(august));
    fake.started().await;
    fake
}

async fn run(fake: &FakeAugust, name: &str, args: &str) -> String {
    fake.command(name, args, ctx("1")).await.unwrap().unwrap_or_default()
}

#[tokio::test]
async fn help_lists_the_cores_commands_and_every_call_is_the_users() {
    let fake = commands().await;
    fake.on("commands", |_| Ok(json!([{"name": "new", "description": "Start a fresh conversation"}, {"name": "ping", "description": "Pong"}])));
    let help = run(&fake, "help", "").await;
    assert!(help.ends_with("/new — Start a fresh conversation\n/ping — Pong\n"), "{help}");
    assert_eq!(fake.calls("commands"), vec![json!({"as_user": true})]);

    run(&fake, "new", "").await;
    assert_eq!(fake.calls("session_new"), vec![json!({"thread": thread("1"), "as_user": true})]);
}

#[tokio::test]
async fn stop_says_stopping_until_nothing_runs() {
    let fake = commands().await;
    fake.on("stop", |_| Ok(json!({"cancelled": 0})));
    assert_eq!(run(&fake, "stop", "").await, "Nothing is running.");

    fake.on("stop", |_| Ok(json!({"cancelled": 1})));
    let busy = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let b = busy.clone();
    fake.on("status", move |_| Ok(json!({"busy": b.fetch_add(1, std::sync::atomic::Ordering::SeqCst) < 2})));
    assert_eq!(fake.command("stop", "", ctx("1")).await.unwrap(), None);
    assert_eq!(fake.texts(), vec!["Stopped."]);
    assert_eq!(fake.calls("status").len(), 3);
}

#[tokio::test]
async fn model_and_models_show_and_switch() {
    let fake = commands().await;
    fake.on("status", |_| Ok(json!({"provider": "openai", "model": "gpt", "workspace": "/ws", "busy": false})));
    fake.on("model_set", |p| match p["model"].as_str() {
        Some("bad") => Err(anyhow::anyhow!("no model `bad`")),
        _ => Ok(json!({"provider": "openai", "model": "other-model"})),
    });
    fake.on("models", |_| Ok(json!((0..70).map(|i| json!({"id": format!("m{i}")})).collect::<Vec<_>>())));

    assert_eq!(run(&fake, "model", "").await, "Current model: `openai · gpt`\nChange with `/model <id>`.");
    assert_eq!(run(&fake, "model", "other-model").await, "Now using `openai · other-model` (applies to the next message).");
    assert!(run(&fake, "model", "bad").await.starts_with("Could not switch model: no model `bad`"));
    assert_eq!(run(&fake, "status", "").await, "Model: `openai · gpt`\nWorkspace: `/ws`\nBusy: no");

    assert_eq!(run(&fake, "models", "m6").await, "m6\nm60\nm61\nm62\nm63\nm64\nm65\nm66\nm67\nm68\nm69");
    assert_eq!(run(&fake, "models", "zzz").await, "No models match.");
    assert!(run(&fake, "models", "").await.starts_with("70 models; narrow with `/models <filter>`"));
}

#[tokio::test]
async fn login_picks_an_account_signs_in_and_switches_to_its_provider() {
    let fake = commands().await;
    fake.on("accounts", |_| {
        Ok(json!([
            {"id": "anthropic", "label": "Anthropic", "status": "connected", "providers": ["anthropic"]},
            {"id": "telegram", "label": "Telegram", "status": "none", "providers": []}
        ]))
    });
    fake.on("login", |_| Ok(json!({"who": "me@x"})));
    fake.on("model_set", |_| Ok(json!({"provider": "anthropic", "model": "claude"})));

    // No account named: the user picks one (✓ marks the signed-in ones).
    let f = fake.clone();
    let reply = tokio::spawn(async move { f.command("login", "", ctx("1")).await.unwrap().unwrap() });
    let ask = fake.wait_sent("Sign in to which account?").await;
    let labels: Vec<&str> = ask.buttons.iter().map(|b| b.label.as_str()).collect();
    assert_eq!(labels, ["Anthropic ✓", "Telegram"]);
    fake.press("1", &ask.buttons[0].id);
    let reply = reply.await.unwrap();
    assert!(reply.starts_with("Signed in to Anthropic as me@x.\nNow using `anthropic · claude`."), "{reply}");
    assert_eq!(fake.calls("login"), vec![json!({"account": "anthropic", "thread": thread("1"), "as_user": true})]);
    assert_eq!(fake.calls("model_set")[0]["model"], "anthropic:");

    // A named account without a provider: no switch. Logout offers only accounts in use.
    assert_eq!(run(&fake, "login", "telegram").await, "Signed in to Telegram as me@x.");
    assert_eq!(fake.calls("model_set").len(), 1);
    assert_eq!(run(&fake, "logout", "anthropic").await, "Signed out of Anthropic.");
    let err = fake.command("logout", "telegram", ctx("1")).await.unwrap_err().to_string();
    assert!(err.contains("no account `telegram` (have: anthropic)"), "{err}");

    // A failed sign-in is told.
    fake.on("login", |_| Err(anyhow::anyhow!("sign-in cancelled")));
    assert_eq!(run(&fake, "login", "anthropic").await, "Sign-in failed: sign-in cancelled");
}

#[tokio::test]
async fn extensions_shows_each_ones_state_and_enables_or_disables() {
    let fake = commands().await;
    fake.on("extensions", |_| {
        Ok(json!([
            {"name": "weather", "state": "running", "tools": [{"name": "forecast"}], "commands": ["w"], "memory_kb": 2048},
            {"name": "meek", "state": "disabled"},
            {"name": "broken", "state": "failed", "error": "crashed"},
            {"name": "probe", "state": "degraded", "health": {"detail": "no API key"}},
            {"name": "idle", "state": "running"}
        ]))
    });
    fake.on("extension_enable", |_| Err(anyhow::anyhow!("no extension `ghost`")));

    let status = run(&fake, "extensions", "").await;
    assert_eq!(
        status,
        "✅ weather — tools: forecast; commands: /w; 2.0 MB\n⏸ meek — disabled\n❌ broken — crashed\n\
         ⚠️ probe — degraded: no API key; registers nothing\n✅ idle — registers nothing"
    );
    run(&fake, "extensions", "disable meek").await;
    assert_eq!(fake.calls("extension_disable"), vec![json!({"name": "meek", "as_user": true})]);
    let failed = run(&fake, "extensions", "enable ghost").await;
    assert!(failed.starts_with("no extension `ghost`\n\n✅ weather"), "{failed}");
    assert_eq!(run(&fake, "extensions", "frobnicate").await, "Usage: /extensions [enable|disable <name>]");
}

#[tokio::test]
async fn config_home_and_queue() {
    let fake = commands().await;
    fake.on("config_get", |_| Ok(json!(["tidy"])));

    // A value is JSON if it parses, else a string; the setting is shown after.
    let shown = run(&fake, "config", r#"august.hooks.order ["tidy"]"#).await;
    assert_eq!(shown, "`august.hooks.order` = ```\n[\n  \"tidy\"\n]\n```");
    run(&fake, "config", "extensions.weather.settings.city Paris").await;
    let set: Vec<_> = fake.calls("config_set").iter().map(|p| p["value"].clone()).collect();
    assert_eq!(set, vec![json!(["tidy"]), json!("Paris")]);
    assert!(run(&fake, "config", "").await.starts_with("Usage: /config"));

    assert!(run(&fake, "home", "").await.contains("home thread"));
    assert_eq!(fake.calls("config_set")[2], json!({"path": "august.home", "value": "test:1", "as_user": true}));

    assert_eq!(run(&fake, "queue", "").await, "Usage: /queue <message>");
    assert_eq!(run(&fake, "queue", "third").await, "📋 Queued.");
    assert_eq!(fake.calls("prompt"), vec![json!({"text": "third", "source": "user", "deliver": "followUp", "thread": thread("1")})]);
}
