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
    let updates = vec![
        message(1, json!({"text": "hello colour"})),
        message(2, json!({"text": "run the forbidden thing"})),
        message(3, json!({"text": "secret"})),
        message(4, json!({"text": "/ping x", "entities": [{"type": "bot_command", "offset": 0, "length": 5}]})),
        message(5, json!({"text": "/extensions", "entities": [{"type": "bot_command", "offset": 0, "length": 11}]})),
    ];
    let pick: fn(&str) -> Value = |text| {
        if text.contains("forbidden") {
            reply_tool("shell", json!({"command": "echo forbidden"}))
        } else {
            reply_tool("shout", json!({"text": "hi"}))
        }
    };
    let fake = Fake::start(updates, HashMap::new(), Some(llm(pick))).await;
    let _gw = spawn_gateway_with_home(
        &fake,
        LlmSetup::Fake,
        &[],
        &[("extensions/demo/index.ts", DEMO), ("extensions/broken/index.ts", BROKEN)],
    );

    fake.wait_for(TIMEOUT, |f| {
        sent_any(f, "turn ended: Result: HI via telegram")
            && sent_any(f, "blocked by an extension")
            && sent_any(f, "intercepted")
            && sent_any(f, "pong x")
            && sent_any(f, "broken")
    })
    .await;

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
