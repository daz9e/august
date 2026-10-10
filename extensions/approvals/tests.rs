//! `approvals` against a fake core: it lets harmless tool calls through, asks the user in the
//! thread about the rest, and blocks what isn't allowed.

use super::serve;
use august_ext::FakeAugust;
use august_ext::fake::{ctx, turn_ctx};
use serde_json::{Value, json};

async fn approvals() -> FakeAugust {
    let (fake, august) = FakeAugust::new(&[]);
    tokio::spawn(serve(august));
    fake.started().await;
    fake
}

/// The judge finds commands with `touch` harmless, nothing else.
fn judge(fake: &FakeAugust) {
    fake.on_llm(|prompt, system| {
        assert!(system.contains("You check a shell command"));
        Ok(if prompt.contains("touch") { "SAFE" } else { "ASK" }.into())
    });
}

/// Nobody answers questions: anything asked about is denied at once.
fn nobody_answers(fake: &FakeAugust) {
    fake.on("next", |_| Ok(json!({"timeout": true})));
}

fn call(tool: &str, input: Value) -> Value {
    json!({"tool": tool, "input": input})
}

fn bash(cmd: &str) -> Value {
    call("bash", json!({"command": cmd}))
}

/// The tool call in thread 1, as the hook leaves it.
async fn hook(fake: &FakeAugust, data: Value) -> Value {
    fake.event("tool_call", data, ctx("1")).await.unwrap()
}

/// Runs `data` through the hook in `ctx`, answering its question with `answer` once asked;
/// the hook's result and the question as it reads after.
async fn answered(fake: &FakeAugust, data: Value, ctx: Value, answer: impl FnOnce(&FakeAugust, &august_ext::fake::Sent)) -> (Value, String) {
    let f = fake.clone();
    let run = tokio::spawn(async move { f.event("tool_call", data, ctx).await.unwrap() });
    let ask = fake.wait_sent("Approval needed").await;
    answer(fake, &ask);
    let out = run.await.unwrap();
    (out, fake.sent().into_iter().find(|m| m.id == ask.id).unwrap().text)
}

fn button(ask: &august_ext::fake::Sent, label: &str) -> String {
    ask.buttons.iter().find(|b| b.label.contains(label)).unwrap().id.clone()
}

#[tokio::test]
async fn read_only_commands_pass_and_the_judge_decides_the_rest() {
    let fake = approvals().await;
    judge(&fake);
    nobody_answers(&fake);

    // A safe command with no shell tricks passes without the judge.
    assert!(hook(&fake, bash("ls -la")).await["block"].is_null());
    assert!(fake.calls("llm").is_empty());

    // With metacharacters even a safe command goes to the judge.
    assert!(hook(&fake, bash("touch a; ls")).await["block"].is_null());
    assert!(hook(&fake, bash("ls; rm x")).await["block"].is_string());
    assert_eq!(fake.calls("llm").len(), 2);

    // Harmless by the judge: no question; otherwise the user is asked.
    assert!(hook(&fake, bash("touch made.txt")).await["block"].is_null());
    let blocked = hook(&fake, bash("rm -f notes.txt")).await;
    assert_eq!(blocked["block"], "the user didn't answer; denied");
    let asked: Vec<String> = fake.texts();
    assert_eq!(asked.len(), 2, "{asked:?}");
    assert!(asked[1].contains("bash: rm -f notes.txt") && asked[1].starts_with("⌛ Timed out, denied"), "{asked:?}");
}

#[tokio::test]
async fn settings_turn_the_judge_off_and_choose_the_safe_commands() {
    let fake = approvals().await;
    judge(&fake);
    nobody_answers(&fake);
    fake.set_settings(json!({"judge": false, "safe_commands": ["git"]}));
    assert!(hook(&fake, bash("git status")).await["block"].is_null());
    assert!(hook(&fake, bash("ls")).await["block"].is_string());
    assert!(hook(&fake, bash("touch made.txt")).await["block"].is_string());
    assert!(fake.calls("llm").is_empty());
}

#[tokio::test]
async fn changes_are_asked_about_and_reads_pass() {
    let fake = approvals().await;
    nobody_answers(&fake);
    let cases = [
        (call("august", json!({"op": "sessions"})), false),
        (call("august", json!({"op": "extension_logs", "params": {"name": "web"}})), false),
        (call("august", json!({"op": "session_new", "params": {"name": "side project"}})), true),
        (call("extensions", json!({"action": "list"})), false),
        (call("extensions", json!({"action": "disable", "name": "web"})), true),
        (call("config", json!({"action": "get", "path": "august.model"})), false),
        (call("config", json!({"action": "set", "path": "extensions.approvals.settings.judge", "value": false})), true),
        (call("save_skill", json!({"name": "x"})), true),
        (call("edit_skill", json!({"name": "x"})), true),
        (call("save_extension", json!({"name": "x"})), true),
        (call("schedule_task", json!({"prompt": "hi"})), false),
        (call("schedule_task", json!({"script": "echo hi"})), true),
        (call("browser", json!({"args": ["open", "https://example.com"]})), false),
        (call("browser", json!({"args": ["upload", "a.txt"]})), true),
        (call("read", json!({"path": "notes.txt"})), false),
    ];
    for (data, asks) in cases {
        let out = hook(&fake, data.clone()).await;
        assert_eq!(out["block"].is_string(), asks, "{data}");
    }
    // The question shows the tool and its arguments.
    let asked = fake.texts();
    assert!(asked.iter().any(|t| t.contains("august:\nop: session_new\nparams: {\"name\":\"side project\"}")), "{asked:?}");
    assert!(asked.iter().any(|t| t.contains("extensions:\naction: disable\nname: web")), "{asked:?}");
}

#[tokio::test]
async fn the_user_allows_or_denies_with_buttons() {
    let fake = approvals().await;
    judge(&fake);
    let (out, ask) = answered(&fake, bash("rm -f notes.txt"), ctx("1"), |f, ask| f.press("1", &button(ask, "Allow"))).await;
    assert!(out["block"].is_null(), "{out}");
    assert_eq!(ask, "✅ Allowed\n```\nbash: rm -f notes.txt\n```");

    let (out, ask) = answered(&fake, bash("rm -f notes.txt"), ctx("1"), |f, ask| f.press("1", &button(ask, "Deny"))).await;
    assert_eq!(out["block"], "the user denied this");
    assert!(ask.starts_with("❌ Denied"), "{ask}");
}

#[tokio::test]
async fn a_yes_in_words_allows_and_other_words_deny_and_reach_the_agent() {
    let fake = approvals().await;
    judge(&fake);
    for yes in ["y", "Yes", " ok "] {
        let (out, ask) = answered(&fake, bash("rm x"), ctx("1"), |f, _| f.reply("1", yes)).await;
        assert!(out["block"].is_null(), "{yes}: {out}");
        assert!(ask.starts_with("✅ Allowed"), "{ask}");
    }

    let turn = turn_ctx("1", json!({"id": 7, "conversation": "thread", "show": true, "meta": {}}));
    let (out, ask) = answered(&fake, bash("rm x"), turn, |f, _| f.reply("1", "no, call it other.txt")).await;
    assert_eq!(out["block"], "the user denied this and wrote instead");
    assert!(ask.starts_with("❌ Denied"), "{ask}");
    let prompt = fake.wait_call("prompt", 1).await;
    assert_eq!((prompt["text"].as_str(), prompt["source"].as_str(), prompt["from_turn"].as_u64()), (Some("no, call it other.txt"), Some("user"), Some(7)));

    // Outside a visible turn the words have nowhere to go.
    let quiet = turn_ctx("1", json!({"id": 8, "conversation": "thread", "show": false, "meta": {}}));
    let (out, _) = answered(&fake, bash("rm x"), quiet, |f, _| f.reply("1", "not now")).await;
    assert!(out["block"].is_string());
    assert_eq!(fake.calls("prompt").len(), 1);
}

#[tokio::test(start_paused = true)]
async fn an_unanswered_question_denies_after_five_minutes() {
    let fake = approvals().await;
    judge(&fake);
    let started = tokio::time::Instant::now();
    let out = hook(&fake, bash("rm x")).await;
    assert_eq!(out["block"], "the user didn't answer; denied");
    assert!(started.elapsed() >= std::time::Duration::from_secs(300));
    assert!(fake.texts()[0].starts_with("⌛ Timed out, denied"));
    // The core waits for the hook that long and more.
    let timeout = fake.manifest().unwrap()["timeouts"]["tool_call"].clone();
    assert!(timeout.as_u64().is_some_and(|ms| ms > 300_000), "{timeout}");
}

#[tokio::test]
async fn a_cancelled_question_blocks_the_call() {
    let fake = approvals().await;
    judge(&fake);
    fake.on("next", |_| Ok(json!({"cancelled": "stop"})));
    assert_eq!(hook(&fake, bash("rm x")).await["block"], "cancelled");
    assert!(fake.texts()[0].starts_with("⏹ Cancelled"));
}

#[tokio::test]
async fn a_turn_approved_in_advance_or_without_a_thread_asks_nobody() {
    let fake = approvals().await;
    judge(&fake);
    let all = turn_ctx("1", json!({"id": 1, "conversation": "thread", "show": false, "meta": {"approve": "all"}}));
    assert!(fake.event("tool_call", bash("rm -rf build"), all).await.unwrap()["block"].is_null());

    let nowhere = json!({"thread": null, "turn": null, "depth": 0});
    let out = fake.event("tool_call", bash("rm -rf build"), nowhere).await.unwrap();
    assert_eq!(out["block"], "nobody to ask for approval here");
    assert!(fake.sent().is_empty());
}
