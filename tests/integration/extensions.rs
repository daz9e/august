//! Extensions: TypeScript in `AUGUST_HOME/extensions` adding tools, commands and hooks,
//! and the agent installing one itself. Needs bun; skipped when it is not installed.

use crate::support::*;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::time::Duration;

const TIMEOUT: Duration = Duration::from_secs(30);

fn have_bun() -> bool {
    let found = std::env::var_os("PATH")
        .is_some_and(|p| std::env::split_paths(&p).any(|d| d.join("bun").is_file()));
    if !found {
        eprintln!("skipping: bun is not installed");
    }
    found
}

const DEMO: &str = r#"
import type { August } from "august";

export default function (august: August) {
  august.registerTool({
    name: "shout",
    description: "Uppercase some text",
    parameters: { type: "object", properties: { text: { type: "string" } }, required: ["text"] },
    execute: ({ text }, ctx) => `${text.toUpperCase()} via ${ctx.chat?.channel}`,
  });
  august.on("message_in", ({ text }) =>
    text === "secret" ? { handled: true, reply: "intercepted" } : { text: text.replace("colour", "color") });
  august.on("before_turn", ({ system }) => ({ system: system + "\nEXTENSION-CONTEXT" }));
  august.on("tool_call", ({ tool, input }) =>
    tool === "shell" && input.command.includes("forbidden") ? { block: "no forbidden commands" } : undefined);
  august.on("turn_end", async ({ reply }, ctx) => { await ctx.send(`turn ended: ${reply}`); });
  august.registerCommand("ping", { description: "Replies pong", handler: (args) => `pong ${args}` });
}
"#;

const BROKEN: &str = r#"throw new Error("boom at load");"#;

fn messages(req: &Value) -> &Vec<Value> {
    req["messages"].as_array().unwrap()
}

fn last_user_text(req: &Value) -> String {
    let m = messages(req).iter().rev().find(|m| m["role"] == "user").unwrap();
    m["content"].as_str().map(String::from).unwrap_or_else(|| m["content"].to_string())
}

/// After a tool ran, answers with its output; otherwise `pick` chooses a tool call.
fn llm(pick: fn(&str) -> Value) -> Llm {
    Box::new(move |req| {
        let last = messages(req).last().unwrap();
        if last["role"] == "tool" {
            reply_text(&format!("Result: {}", last["content"].as_str().unwrap_or("")))
        } else {
            pick(&last_user_text(req))
        }
    })
}

fn sent_any(f: &Fake, needle: &str) -> bool {
    f.sent_texts().iter().any(|t| t.contains(needle))
}

#[tokio::test]
async fn extensions_add_tools_commands_and_hooks() {
    if !have_bun() {
        return;
    }
    let pick: fn(&str) -> Value = |text| {
        if text.contains("forbidden") {
            reply_tool("shell", json!({"command": "echo forbidden"}))
        } else {
            reply_tool("shout", json!({"text": "hi"}))
        }
    };
    let fake = Fake::start(vec![message(1, json!({"text": "hello colour"}))], HashMap::new(), Some(llm(pick))).await;
    let _gw = spawn_gateway_with_home(
        &fake,
        LlmSetup::Fake,
        &[],
        &[("extensions/demo/index.ts", DEMO), ("extensions/broken/index.ts", BROKEN)],
    );

    // One at a time: a message sent while a turn runs would join that turn.
    fake.wait_for(TIMEOUT, |f| sent_any(f, "turn ended: Result: HI via telegram")).await;
    fake.push_updates(vec![message(2, json!({"text": "run the forbidden thing"}))]);
    fake.wait_for(TIMEOUT, |f| sent_any(f, "blocked by an extension")).await;
    fake.push_updates(vec![
        message(3, json!({"text": "secret"})),
        message(4, json!({"text": "/ping x", "entities": [{"type": "bot_command", "offset": 0, "length": 5}]})),
        message(5, json!({"text": "/extensions", "entities": [{"type": "bot_command", "offset": 0, "length": 11}]})),
    ]);
    fake.wait_for(TIMEOUT, |f| sent_any(f, "intercepted") && sent_any(f, "pong x") && sent_any(f, "broken")).await;

    // The extension's tool was offered and the before_turn hook extended the prompt.
    let reqs = fake.llm_requests();
    let first = reqs.iter().find(|r| last_user_text(r).contains("hello")).unwrap();
    let tools: Vec<&str> = first["tools"].as_array().unwrap().iter().filter_map(|t| t["function"]["name"].as_str()).collect();
    assert!(tools.contains(&"shout") && tools.contains(&"shell"), "{tools:?}");
    assert!(messages(first)[0]["content"].as_str().unwrap().contains("EXTENSION-CONTEXT"));
    // message_in rewrote one message and swallowed another before the model saw them.
    assert!(last_user_text(first).contains("hello color"));
    assert!(reqs.iter().all(|r| !last_user_text(r).contains("secret")));
    // tool_call blocked the shell command; the model got the reason as a tool error.
    assert!(reqs.iter().any(|r| messages(r).iter().any(|m| m["role"] == "tool"
        && m["content"].as_str().unwrap_or("").contains("blocked by an extension: no forbidden commands"))));

    // /extensions reports the working one and the error of the broken one.
    let status = fake.sent_texts().into_iter().find(|t| t.contains("broken")).unwrap();
    assert!(status.contains("demo") && status.contains("shout") && status.contains("/ping"), "{status}");
    assert!(status.contains("boom at load"), "{status}");
    // The extension command is announced to Telegram.
    let commands = fake.calls("setMyCommands");
    assert!(commands.iter().any(|r| r.text().contains("\"ping\"")));
}

const GREETER: &str = r#"export default function (august) {
  august.registerTool({
    name: "greet",
    description: "Greets someone",
    parameters: { type: "object", properties: { name: { type: "string" } }, required: ["name"] },
    execute: ({ name }) => `Hello, ${name}!`,
  });
}"#;

#[tokio::test]
async fn agent_installs_an_extension_with_approval() {
    if !have_bun() {
        return;
    }
    let pick: fn(&str) -> Value = |text| {
        if text.contains("add a greeter") {
            reply_tool("save_extension", json!({"name": "greeter", "code": GREETER}))
        } else {
            reply_tool("greet", json!({"name": "Bob"}))
        }
    };
    let fake = Fake::start(vec![message(1, json!({"text": "add a greeter"}))], HashMap::new(), Some(llm(pick))).await;
    let gw = spawn_gateway_with_home(&fake, LlmSetup::Fake, &[], &[]);

    // The install waits for the owner's approval, which shows the code.
    fake.wait_for(TIMEOUT, |f| f.calls("sendMessage").iter().any(|r| r.text().contains("ap:"))).await;
    let ask = fake.calls("sendMessage").into_iter().find(|r| r.text().contains("ap:")).unwrap().json();
    assert!(ask["text"].as_str().unwrap().contains("greeter"));
    assert!(ask["text"].as_str().unwrap().contains("Hello, ${name}!"));
    let allow = ask["reply_markup"]["inline_keyboard"][0][0]["callback_data"].as_str().unwrap().to_string();
    assert!(allow.ends_with(":y"));
    fake.push_updates(vec![button_press(2, &allow)]);

    fake.wait_for(TIMEOUT, |f| sent_any(f, "Result: saved")).await;
    assert!(sent_any(&fake, "greeter — tools: greet"), "{:?}", fake.sent_texts());
    assert_eq!(std::fs::read_to_string(gw.home.join("extensions/greeter/index.ts")).unwrap(), GREETER);

    // The new tool is usable on the next message, without a restart.
    fake.push_updates(vec![message(3, json!({"text": "greet Bob"}))]);
    fake.wait_for(TIMEOUT, |f| sent_any(f, "Result: Hello, Bob!")).await;
}

const FRAGILE: &str = r#"export default function (august) {
  august.registerTool({
    name: "crash",
    description: "Exits the extension process",
    execute: () => { setTimeout(() => process.exit(1), 10); return "bye"; },
  });
  august.registerTool({ name: "alive", description: "Answers if running", execute: () => "still here" });
}"#;

#[tokio::test]
async fn crashed_extension_is_restarted() {
    if !have_bun() {
        return;
    }
    let pick: fn(&str) -> Value =
        |text| reply_tool(if text.contains("crash") { "crash" } else { "alive" }, json!({}));
    let fake = Fake::start(vec![message(1, json!({"text": "crash please"}))], HashMap::new(), Some(llm(pick))).await;
    let _gw = spawn_gateway_with_home(&fake, LlmSetup::Fake, &[], &[("extensions/fragile/index.ts", FRAGILE)]);
    fake.wait_for(TIMEOUT, |f| sent_any(f, "Result: bye")).await;

    // The first restart comes after a second; the gateway keeps serving meanwhile.
    tokio::time::sleep(Duration::from_millis(2_500)).await;
    fake.push_updates(vec![message(2, json!({"text": "are you there?"}))]);
    fake.wait_for(TIMEOUT, |f| sent_any(f, "Result: still here")).await;
}

const PROBE: &str = r#"
import type { August } from "august";

export default function (august: August) {
  august.on("llm_call", ({ step, system }) => (step === 1 ? { system: system + "\nSTEP-ONE" } : undefined));
  august.on("llm_result", async ({ step, text, toolCalls, usage }, ctx) => {
    await ctx.send(`llm_result ${step}: ${toolCalls.map((c) => c.name).join(",")}|${text}|${typeof usage.inputTokens}`);
  });
  august.on("session_start", async ({ previous, session }, ctx) => {
    await ctx.send(`session_start ${previous !== session ? "fresh" : "same"}`);
  });
  august.registerCommand("peek", async (args, ctx) => {
    const { output, isError } = await ctx.callTool("read_file", { path: "note.txt" });
    return `peek ${args}: ${output} (error: ${isError})`;
  });
  august.registerCommand("ask", async (args, ctx) => `llm says: ${await ctx.llm(args, { system: "SYS-X" })}`);
}
"#;

fn command(id: i64, text: &str) -> Value {
    let len = text.split_whitespace().next().unwrap().len();
    message(id, json!({"text": text, "entities": [{"type": "bot_command", "offset": 0, "length": len}]}))
}

#[tokio::test]
async fn extensions_hook_model_calls_call_into_august_and_can_be_disabled() {
    if !have_bun() {
        return;
    }
    let pick: fn(&str) -> Value = |text| {
        if text.contains("read the note") {
            reply_tool("read_file", json!({"path": "note.txt"}))
        } else {
            reply_text("forty-two")
        }
    };
    let fake = Fake::start(vec![message(1, json!({"text": "read the note"}))], HashMap::new(), Some(llm(pick))).await;
    let gw = spawn_gateway_with_home(
        &fake,
        LlmSetup::Fake,
        &[("note.txt", b"note body")],
        &[("extensions/probe/index.ts", PROBE)],
    );

    // llm_result fires after every model call of the turn.
    fake.wait_for(TIMEOUT, |f| {
        sent_any(f, "llm_result 0: read_file||number") && sent_any(f, "llm_result 1: |Result: note body")
    })
    .await;
    // llm_call changed the system prompt of the second call only.
    let reqs = fake.llm_requests();
    let system = |r: &Value| messages(r)[0]["content"].as_str().unwrap().to_string();
    let second = reqs.iter().find(|r| messages(r).last().unwrap()["role"] == "tool").unwrap();
    assert!(system(second).ends_with("\nSTEP-ONE"));
    assert_eq!(reqs.iter().filter(|r| system(r).contains("STEP-ONE")).count(), 1);

    fake.push_updates(vec![command(2, "/new")]);
    fake.wait_for(TIMEOUT, |f| sent_any(f, "session_start fresh")).await;

    // ctx.callTool runs a built-in tool in the chat; ctx.llm asks the model without tools.
    fake.push_updates(vec![command(3, "/peek one")]);
    fake.wait_for(TIMEOUT, |f| sent_any(f, "peek one: note body (error: false)")).await;
    fake.push_updates(vec![command(4, "/ask what is six times seven")]);
    fake.wait_for(TIMEOUT, |f| sent_any(f, "llm says: forty-two")).await;
    let ask = fake.llm_requests().into_iter().find(|r| last_user_text(r).contains("six times seven")).unwrap();
    assert_eq!(system(&ask), "SYS-X");
    assert!(ask["tools"].as_array().is_none_or(|t| t.is_empty()));

    // Disabled: listed as paused, not running, remembered on disk; enabling restores it.
    fake.push_updates(vec![command(5, "/extensions disable probe")]);
    fake.wait_for(TIMEOUT, |f| sent_any(f, "⏸ probe")).await;
    assert!(gw.home.join("extensions/probe/disabled").exists());
    fake.push_updates(vec![command(6, "/peek two")]);
    fake.wait_for(TIMEOUT, |f| sent_any(f, "Unknown command /peek")).await;
    fake.push_updates(vec![command(7, "/extensions enable probe")]);
    fake.wait_for(TIMEOUT, |f| sent_any(f, "✅ probe")).await;
    assert!(!gw.home.join("extensions/probe/disabled").exists());
    fake.push_updates(vec![command(8, "/peek three")]);
    fake.wait_for(TIMEOUT, |f| sent_any(f, "peek three: note body")).await;
}

const WATCHER: &str = r#"export default function (august) {
  august.on("compaction", async ({ before, after }, ctx) => {
    await ctx.send(`compaction event: ${before > after ? "smaller" : "not smaller"}`);
  });
}"#;

#[tokio::test]
async fn compaction_event_reaches_extensions() {
    if !have_bun() {
        return;
    }
    let llm: Llm = Box::new(|req| {
        let system = messages(req)[0]["content"].as_str().unwrap_or("");
        reply_text(if system.contains("You compress conversations") { "## Goal\nchat" } else { "ok" })
    });
    let fake = Fake::start(vec![message(1, json!({"text": "message 1"}))], HashMap::new(), Some(llm)).await;
    let _gw = spawn_gateway_with_home(&fake, LlmSetup::Fake, &[], &[("extensions/watcher/index.ts", WATCHER)]);
    fake.wait_for(TIMEOUT, |f| f.sent_texts().len() >= 1).await;
    for i in 2..=6 {
        let n = fake.sent_texts().len();
        fake.push_updates(vec![message(i, json!({"text": format!("message {i} {}", "x".repeat(300))}))]);
        fake.wait_for(TIMEOUT, |f| f.sent_texts().len() > n).await;
    }
    fake.push_updates(vec![message(7, json!({"text": "/compact", "entities": [{"type": "bot_command", "offset": 0, "length": 8}]}))]);
    fake.wait_for(TIMEOUT, |f| sent_any(f, "compaction event: smaller")).await;
}

const RECALL: &str = r#"
import type { August } from "august";

export default function (august: August) {
  august.on("context", ({ messages }) => ({
    messages: [{ role: "user", content: [{ type: "text", text: "RECALLED: the user likes tea" }] }, ...messages],
  }));
}
"#;

#[tokio::test]
async fn context_hook_changes_what_the_model_sees_but_not_the_history() {
    if !have_bun() {
        return;
    }
    let updates = vec![message(1, json!({"text": "hi"})), message(2, json!({"text": "again"}))];
    let llm: Llm = Box::new(|req| reply_text(&format!("seen {}", messages(req).len())));
    let fake = Fake::start(updates, HashMap::new(), Some(llm)).await;
    let _gw = spawn_gateway_with_home(&fake, LlmSetup::Fake, &[], &[("extensions/recall/index.ts", RECALL)]);
    fake.wait_for(TIMEOUT, |f| f.sent_texts().iter().filter(|t| t.starts_with("seen")).count() >= 2).await;

    for req in fake.llm_requests() {
        let all = req.to_string();
        assert_eq!(all.matches("RECALLED: the user likes tea").count(), 1, "{all}");
    }
}

const POLICY: &str = r#"
import type { August } from "august";

export default function (august: August) {
  august.registerTool({ name: "list_dir", description: "List a folder", execute: () => "custom listing" });
  august.on("tool_call", ({ tool, input }) => {
    if (tool === "shell" && input.command.startsWith("touch ")) return { approve: true };
    if (tool === "read_file") return { ask: "Let the agent read notes.txt?" };
  });
}
"#;

#[tokio::test]
async fn extensions_replace_builtin_tools_and_decide_approvals() {
    if !have_bun() {
        return;
    }
    let pick = |text: &str| match text {
        t if t.contains("list") => reply_tool("list_dir", json!({"path": "."})),
        t if t.contains("touch") => reply_tool("shell", json!({"command": "touch made.txt"})),
        _ => reply_tool("read_file", json!({"path": "notes.txt"})),
    };
    let fake = Fake::start(vec![message(1, json!({"text": "list"}))], HashMap::new(), Some(llm(pick))).await;
    let gw = spawn_gateway_with_home(&fake, LlmSetup::Fake, &[("notes.txt", b"private")], &[("extensions/policy/index.ts", POLICY)]);

    // An extension tool replaces the built-in of the same name.
    fake.wait_for(TIMEOUT, |f| sent_any(f, "Result: custom listing")).await;

    // `approve: true` runs a risky command without asking.
    fake.push_updates(vec![message(2, json!({"text": "touch"}))]);
    fake.wait_for(TIMEOUT, |f| sent_any(f, "Result: exit code: 0")).await;
    assert!(gw.workspace.join("made.txt").exists());
    assert!(!fake.calls("sendMessage").iter().any(|r| r.text().contains("ap:")), "asked anyway");

    // `ask` asks even for a tool that never does; a denial reaches the model.
    fake.push_updates(vec![message(3, json!({"text": "read"}))]);
    fake.wait_for(TIMEOUT, |f| f.calls("sendMessage").iter().any(|r| r.text().contains("ap:"))).await;
    let ask = fake.calls("sendMessage").into_iter().find(|r| r.text().contains("ap:")).unwrap().json();
    assert!(ask["text"].as_str().unwrap().contains("Let the agent read notes.txt?"));
    let deny = ask["reply_markup"]["inline_keyboard"][0][1]["callback_data"].as_str().unwrap().to_string();
    assert!(deny.ends_with(":n"), "{deny}");
    fake.push_updates(vec![button_press(4, &deny)]);
    fake.wait_for(TIMEOUT, |f| sent_any(f, "Result: the user denied this")).await;
}

const LATE: &str = r#"export default function (august) {
  august.registerTool({ name: "early", description: "Gone after /arm", execute: () => "early ran" });
  august.registerCommand("arm", () => {
    august.unregisterTool("early");
    august.registerTool({ name: "late", description: "Added by /arm", execute: () => "late ran" });
    return "armed";
  });
}"#;

#[tokio::test]
async fn extension_changes_its_tools_after_startup() {
    if !have_bun() {
        return;
    }
    let arm = message(1, json!({"text": "/arm", "entities": [{"type": "bot_command", "offset": 0, "length": 4}]}));
    let fake = Fake::start(vec![arm], HashMap::new(), Some(llm(|_| reply_tool("late", json!({}))))).await;
    let _gw = spawn_gateway_with_home(&fake, LlmSetup::Fake, &[], &[("extensions/late/index.ts", LATE)]);

    fake.wait_for(TIMEOUT, |f| sent_any(f, "armed")).await;
    fake.push_updates(vec![message(2, json!({"text": "go"}))]);
    fake.wait_for(TIMEOUT, |f| sent_any(f, "Result: late ran")).await;

    let first = &fake.llm_requests()[0];
    let tools: Vec<&str> = first["tools"].as_array().unwrap().iter().filter_map(|t| t["function"]["name"].as_str()).collect();
    assert!(tools.contains(&"late") && !tools.contains(&"early"), "{tools:?}");
}
