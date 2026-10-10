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

/// Signed in to `opencode` (active) and `chatgpt`, not to `openai`; both offer `gpt-5.5`.
async fn two_providers() -> FakeAugust {
    let fake = commands().await;
    fake.on("status", |_| Ok(json!({"provider": "opencode", "model": "big-pickle", "workspace": "/ws", "busy": false})));
    fake.on("accounts", |_| {
        Ok(json!([
            {"id": "openai", "providers": ["openai"], "status": "none"},
            {"id": "chatgpt", "providers": ["chatgpt"], "status": "connected"},
            {"id": "opencode", "providers": ["opencode"], "status": "connected"},
        ]))
    });
    fake.on("models", |p| {
        let ids: Vec<String> = match p["provider"].as_str() {
            Some("opencode") => ["gpt-5.5", "big-pickle"].into_iter().map(String::from).chain((0..50).map(|i| format!("m{i}"))).collect(),
            Some("chatgpt") => vec!["gpt-5.5".into(), "gpt-5.4".into()],
            other => panic!("listed models of {other:?}"),
        };
        Ok(json!(ids.iter().map(|id| json!({"id": id})).collect::<Vec<_>>()))
    });
    fake.on("model_set", |p| match p["model"].as_str().unwrap().split_once(':') {
        Some((provider, model)) => Ok(json!({"provider": provider, "model": model})),
        None => Err(anyhow::anyhow!("no model `{}`", p["model"].as_str().unwrap())),
    });
    fake
}

#[tokio::test]
async fn model_switches_to_the_provider_that_offers_it() {
    let fake = two_providers().await;
    assert_eq!(run(&fake, "model", "").await, "Current model: `opencode · big-pickle`\nChange with `/model <id>`.");
    assert_eq!(run(&fake, "model", "gpt-5.4").await, "Now using `chatgpt · gpt-5.4` (applies to the next message).");
    assert_eq!(run(&fake, "model", "opencode:gpt-5.5").await, "Now using `opencode · gpt-5.5` (applies to the next message).");
    // Listed nowhere: the core decides.
    assert!(run(&fake, "model", "bad").await.starts_with("Could not switch model: no model `bad`"));
    assert_eq!(run(&fake, "status", "").await, "Model: `opencode · big-pickle`\nWorkspace: `/ws`\nBusy: no");
}

#[tokio::test]
async fn a_model_several_providers_offer_asks_which() {
    let fake = two_providers().await;
    let switch = tokio::spawn({
        let fake = fake.clone();
        async move { run(&fake, "model", "gpt-5.5").await }
    });
    let ask = fake.wait_sent("more than one provider").await;
    let labels: Vec<&str> = ask.buttons.iter().map(|b| b.label.as_str()).collect();
    assert_eq!(labels, ["opencode", "chatgpt"], "the active provider first");
    fake.press("1", &ask.buttons[1].id);
    assert_eq!(switch.await.unwrap(), "Now using `chatgpt · gpt-5.5` (applies to the next message).");
}

#[tokio::test]
async fn models_lists_every_signed_in_provider() {
    let fake = two_providers().await;
    let all = run(&fake, "models", "").await;
    assert!(all.starts_with("**opencode**: gpt-5.5, big-pickle, m0"), "{all}");
    assert!(all.contains("… 12 more"), "{all}");
    assert!(all.contains("**chatgpt**: gpt-5.5, gpt-5.4\n"), "{all}");
    assert_eq!(run(&fake, "models", "gpt-5.4").await, "**chatgpt**: gpt-5.4\n\nSwitch with `/model <id>` or `/model <provider>:<id>`.");
    assert_eq!(run(&fake, "models", "zzz").await, "No models match.");
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
