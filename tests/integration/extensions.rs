//! Extensions: TypeScript in `AUGUST_HOME/extensions` adding tools, commands and hooks,
//! and the agent installing one itself. Needs bun; skipped when it is not installed.

use crate::support::*;
use serde_json::{Value, json};
use std::time::Duration;


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
    execute: ({ text }, ctx) => `${text.toUpperCase()} via ${ctx.thread?.messenger}`,
  });
  august.on("message_in", ({ text }) =>
    text === "secret" ? { handled: true, reply: "intercepted" } : { text: text.replace("colour", "color") });
  august.on("before_turn", ({ system }) => ({ system: system + "\nEXTENSION-CONTEXT" }));
  august.on("tool_call", ({ tool, input }) =>
    tool === "bash" && input.command.includes("forbidden") ? { block: "no forbidden commands" } : undefined);
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

#[tokio::test]
async fn extensions_add_tools_commands_and_hooks() {
    if !have_bun() {
        return;
    }
    let pick: fn(&str) -> Value = |text| {
        if text.contains("forbidden") {
            reply_tool("bash", json!({"command": "echo forbidden"}))
        } else {
            reply_tool("shout", json!({"text": "hi"}))
        }
    };
    let fake = Fake::llm(llm(pick)).await;
    let home = [("extensions/demo/index.ts", DEMO), ("extensions/broken/index.ts", BROKEN)];
    let gw = august(&fake, Setup { home: &home, ..Default::default() }).await;
    let mut chat = gw.chat().await;

    // One at a time: a message sent while a turn runs would join that turn.
    chat.ask("hello colour", "turn ended: Result: HI via cli").await;
    chat.ask("run the forbidden thing", "blocked by an extension").await;
    chat.ask("secret", "intercepted").await;
    chat.ask("/ping x", "pong x").await;
    let status = chat.ask("/extensions", "broken").await;

    // The extension's tool was offered and the before_turn hook extended the prompt.
    let reqs = fake.llm_requests();
    let first = reqs.iter().find(|r| last_user_text(r).contains("hello")).unwrap();
    let tools: Vec<&str> = first["tools"].as_array().unwrap().iter().filter_map(|t| t["function"]["name"].as_str()).collect();
    assert!(tools.contains(&"shout") && tools.contains(&"bash"), "{tools:?}");
    // The core's own file and command tools are these four; searching is bash's job.
    assert!(["read", "write", "edit", "bash"].iter().all(|t| tools.contains(t)), "{tools:?}");
    assert!(!["grep", "glob", "list_dir", "shell"].iter().any(|t| tools.contains(t)), "{tools:?}");
    assert!(messages(first)[0]["content"].as_str().unwrap().contains("EXTENSION-CONTEXT"));
    // message_in rewrote one message and swallowed another before the model saw them.
    assert!(last_user_text(first).contains("hello color"));
    assert!(reqs.iter().all(|r| !last_user_text(r).contains("secret")));
    // tool_call blocked the bash command; the model got the reason as a tool error.
    assert!(reqs.iter().any(|r| messages(r).iter().any(|m| m["role"] == "tool"
        && m["content"].as_str().unwrap_or("").contains("blocked by an extension: no forbidden commands"))));

    // /extensions reports the working one and the error of the broken one.
    assert!(status.contains("demo") && status.contains("shout") && status.contains("/ping"), "{status}");
    assert!(status.contains("boom at load"), "{status}");
    // The extension command is in /help.
    chat.ask("/help", "/ping").await;
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
    let fake = Fake::llm(llm(pick)).await;
    let gw = august(&fake, Setup::default()).await;
    let mut chat = gw.chat().await;
    chat.say("add a greeter").await;

    // The install waits for the owner's approval, which shows the code.
    let ask = chat.question().await;
    assert!(ask.text.contains("greeter") && ask.text.contains("Hello, ${name}!"), "{}", ask.text);
    chat.press(&ask.button("Allow")).await;

    let saved = chat.wait_for("Result: saved").await;
    assert!(saved.text.contains("greeter — tools: greet"), "{}", saved.text);
    assert_eq!(std::fs::read_to_string(gw.home.join("extensions/greeter/index.ts")).unwrap(), GREETER);
    // The core marks it as the agent's.
    let unit = std::fs::read_to_string(gw.home.join("config/extensions/greeter.json")).unwrap();
    assert!(unit.contains(r#""origin": "agent""#), "{unit}");

    // The new tool is usable on the next message, without a restart.
    chat.ask("greet Bob", "Result: Hello, Bob!").await;
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
    let fake = Fake::llm(llm(pick)).await;
    let gw = august(&fake, Setup { home: &[("extensions/fragile/index.ts", FRAGILE)], ..Default::default() }).await;
    let mut chat = gw.chat().await;
    chat.ask("crash please", "Result: bye").await;

    // The first restart comes after a second; the gateway keeps serving meanwhile.
    tokio::time::sleep(Duration::from_millis(2_500)).await;
    chat.ask("are you there?", "Result: still here").await;
}

const PROBE: &str = r#"
import type { August } from "august";

export default function (august: August) {
  august.needs("tools", "llm");
  august.on("llm_call", ({ step, system }) => (step === 1 ? { system: system + "\nSTEP-ONE" } : undefined));
  august.on("llm_result", async ({ step, text, toolCalls, usage }, ctx) => {
    await ctx.send(`llm_result ${step}: ${toolCalls.map((c) => c.name).join(",")}|${text}|${typeof usage.inputTokens}`);
  });
  august.registerCommand("peek", async (args, ctx) => {
    const { output, isError } = await ctx.callTool("read", { path: "note.txt" });
    return `peek ${args}: ${output} (error: ${isError})`;
  });
  august.registerCommand("ask", async (args, ctx) => `llm says: ${await ctx.llm(args, { system: "SYS-X" })}`);
  august.registerCommand("share", async (_, ctx) => (await ctx.callTool("send_file", { path: "note.txt", caption: "the note" })).output);
}
"#;

#[tokio::test]
async fn extensions_hook_model_calls_call_into_august_and_can_be_disabled() {
    if !have_bun() {
        return;
    }
    let pick: fn(&str) -> Value = |text| {
        if text.contains("read the note") {
            reply_tool("read", json!({"path": "note.txt"}))
        } else {
            reply_text("forty-two")
        }
    };
    let fake = Fake::llm(llm(pick)).await;
    let setup = Setup { seed: &[("note.txt", b"note body")], home: &[("extensions/probe/index.ts", PROBE)], ..Default::default() };
    let gw = august(&fake, setup).await;
    let mut chat = gw.chat().await;
    chat.say("read the note").await;

    // llm_result fires after every model call of the turn.
    chat.wait_for("llm_result 0: read||number").await;
    chat.wait_for("llm_result 1: |Result: note body").await;
    // llm_call changed the system prompt of the second call only.
    let reqs = fake.llm_requests();
    let system = |r: &Value| messages(r)[0]["content"].as_str().unwrap().to_string();
    let second = reqs.iter().find(|r| messages(r).last().unwrap()["role"] == "tool").unwrap();
    assert!(system(second).ends_with("\nSTEP-ONE"));
    assert_eq!(reqs.iter().filter(|r| system(r).contains("STEP-ONE")).count(), 1);

    // ctx.callTool runs a built-in tool in the chat; ctx.llm asks the model without tools.
    chat.ask("/peek one", "peek one: note body (error: false)").await;
    chat.ask("/ask what is six times seven", "llm says: forty-two").await;
    // A tool run by an extension can send files to the thread.
    chat.ask("/share", "sent note.txt").await;
    assert!(chat.files().iter().any(|(p, c)| p.ends_with("note.txt") && c == "the note"), "{:?}", chat.files());
    let ask = fake.llm_requests().into_iter().find(|r| last_user_text(r).contains("six times seven")).unwrap();
    assert_eq!(system(&ask), "SYS-X");
    assert!(ask["tools"].as_array().is_none_or(|t| t.is_empty()));

    // Disabled: listed as paused, not running, remembered on disk; enabling restores it.
    let settings = || std::fs::read_to_string(gw.home.join("config/extensions/probe.json")).unwrap_or_default();
    chat.ask("/extensions disable probe", "⏸ probe").await;
    assert!(settings().contains(r#""enabled": false"#), "{}", settings());
    chat.ask("/peek two", "Unknown command /peek").await;
    chat.ask("/extensions enable probe", "✅ probe").await;
    assert!(!settings().contains("enabled"), "{}", settings());
    chat.ask("/peek three", "peek three: note body").await;
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
    let fake = Fake::llm(llm).await;
    let gw = august(&fake, Setup { home: &[("extensions/watcher/index.ts", WATCHER)], ..Default::default() }).await;
    let mut chat = gw.chat().await;
    for i in 1..=6 {
        chat.ask(&format!("message {i} {}", "x".repeat(300)), "ok").await;
    }
    chat.ask("/compact", "compaction event: smaller").await;
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
    let llm: Llm = Box::new(|req| reply_text(&format!("seen {}", messages(req).len())));
    let fake = Fake::llm(llm).await;
    let gw = august(&fake, Setup { home: &[("extensions/recall/index.ts", RECALL)], ..Default::default() }).await;
    let mut chat = gw.chat().await;
    chat.ask("hi", "seen").await;
    chat.ask("again", "seen").await;

    for req in fake.llm_requests() {
        let all = req.to_string();
        assert_eq!(all.matches("RECALLED: the user likes tea").count(), 1, "{all}");
    }
}

const REPLACER: &str = r#"export default function (august) {
  august.registerTool({ name: "edit", description: "Edit a file", execute: () => "custom edit" });
}"#;

#[tokio::test]
async fn extensions_replace_builtin_tools() {
    if !have_bun() {
        return;
    }
    let fake = Fake::llm(llm(|_| reply_tool("edit", json!({"path": "notes.txt"})))).await;
    let gw = august(&fake, Setup { home: &[("extensions/replacer/index.ts", REPLACER)], ..Default::default() }).await;
    let mut chat = gw.chat().await;
    chat.ask("edit", "Result: custom edit").await;
}

/// The judge's verdict on a command (a separate model call), else the conversation.
fn judged(pick: fn(&str) -> Value) -> Llm {
    let chat = llm(pick);
    Box::new(move |req| {
        if messages(req)[0]["content"].as_str().is_some_and(|s| s.contains("You check a shell command")) {
            return reply_text(if last_user_text(req).contains("touch") { "SAFE" } else { "ASK" });
        }
        chat(req)
    })
}

#[tokio::test]
async fn approvals_judge_commands_apart_and_ask_about_the_rest() {
    let pick = |text: &str| match text {
        t if t.contains("touch") => reply_tool("bash", json!({"command": "touch made.txt"})),
        _ => reply_tool("bash", json!({"command": "rm -f notes.txt"})),
    };
    let fake = Fake::llm(judged(pick)).await;
    let gw = august(&fake, Setup { seed: &[("notes.txt", b"keep")], ..Default::default() }).await;
    let mut chat = gw.chat().await;

    // The judge found it harmless: it runs without a question.
    chat.ask("touch", "Result: exit code: 0").await;
    assert!(gw.workspace.join("made.txt").exists());
    assert!(chat.messages().iter().all(|m| m.buttons.is_empty()), "asked anyway");

    // It didn't: the user is asked, and a denial blocks the call.
    chat.say("remove").await;
    let ask = chat.question().await;
    assert!(ask.text.contains("rm -f notes.txt"), "{}", ask.text);
    chat.press(&ask.button("Deny")).await;
    chat.wait_for("Result: blocked by an extension: the user denied this").await;
    assert!(gw.workspace.join("notes.txt").exists());
}

const GATEKEEPER: &str = r#"export default function (august) {
  august.on("tool_call", ({ tool, input }) =>
    tool === "bash" && input.command.startsWith("rm ") ? { block: "no deleting" } : undefined);
}"#;

#[tokio::test]
async fn approvals_are_an_ordinary_extension_the_user_can_replace() {
    if !have_bun() {
        return;
    }
    let pick = |text: &str| match text {
        t if t.contains("touch") => reply_tool("bash", json!({"command": "touch made.txt"})),
        _ => reply_tool("bash", json!({"command": "rm -f notes.txt"})),
    };
    let fake = Fake::llm(llm(pick)).await;
    let off = r#"{"enabled": false}"#;
    let home = [("config/extensions/approvals.json", off), ("extensions/gatekeeper/index.ts", GATEKEEPER)];
    let gw = august(&fake, Setup { seed: &[("notes.txt", b"keep")], home: &home, ..Default::default() }).await;
    let mut chat = gw.chat().await;

    // The core asks nobody; the user's own policy decides.
    chat.ask("touch", "Result: exit code: 0").await;
    assert!(gw.workspace.join("made.txt").exists());
    chat.ask("remove", "Result: blocked by an extension: no deleting").await;
    assert!(gw.workspace.join("notes.txt").exists());
    assert!(chat.messages().iter().all(|m| m.buttons.is_empty()), "asked anyway");
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
    let fake = Fake::llm(llm(|_| reply_tool("late", json!({})))).await;
    let gw = august(&fake, Setup { home: &[("extensions/late/index.ts", LATE)], ..Default::default() }).await;
    let mut chat = gw.chat().await;
    chat.ask("/arm", "armed").await;
    chat.ask("go", "Result: late ran").await;

    let first = &fake.llm_requests()[0];
    let tools: Vec<&str> = first["tools"].as_array().unwrap().iter().filter_map(|t| t["function"]["name"].as_str()).collect();
    assert!(tools.contains(&"late") && !tools.contains(&"early"), "{tools:?}");
}

const RELAY: &str = r#"
import type { August } from "august";

export default function (august: August) {
  august.needs("messaging");
  august.registerCommand("where", async () => {
    const all = await august.messengers();
    return all.map((m) => `${m.id}[buttons ${m.capabilities.buttons}]: ${m.threads.map((t) => t.id + (t.active ? "*" : "")).join(",")}`).join("; ");
  });
  // Asks in another window and reports the answer back here.
  august.registerCommand("poke", async (id) => {
    const answer = await august.ask({ messenger: "cli", id }, "Coffee?", ["Yes", "No"], { timeout: 10_000 });
    return `answer: ${answer}`;
  });
  august.registerCommand("wait", async (_, ctx) => {
    const l = await august.listen(ctx.thread!, { text: true });
    return `got: ${JSON.stringify(await august.next(l, { timeout: 300 }))}`;
  });
  // A listener nobody collects stops taking messages after its ttl.
  august.registerCommand("forget", async (_, ctx) => {
    await august.listen(ctx.thread!, { text: true, ttl: 200 });
    return "listening briefly";
  });
}
"#;

#[tokio::test]
async fn extensions_see_messengers_and_talk_to_any_thread() {
    if !have_bun() {
        return;
    }
    let fake = Fake::llm(Box::new(|_| reply_text("the agent answered"))).await;
    let gw = august(&fake, Setup { home: &[("extensions/relay/index.ts", RELAY)], ..Default::default() }).await;
    let mut first = gw.chat().await;
    let mut second = gw.chat().await;

    // Messengers describe themselves and list their threads, the latest one active.
    second.ask("/help", "/where").await;
    let wh = first.ask("/where", "cli[").await;
    assert!(wh.contains("cli[buttons 9]: 1*,2"), "{wh}");

    // A question asked in another thread: its buttons answer it…
    first.say("/poke 2").await;
    let q = second.question().await;
    assert!(q.text.contains("Coffee?"), "{}", q.text);
    second.press(&q.button("No")).await;
    first.wait_for("answer: No").await;
    // …or the user's own words, which don't reach the agent as a message.
    first.say("/poke 2").await;
    second.question().await;
    second.say("maybe later").await;
    first.wait_for("answer: maybe later").await;
    assert!(fake.llm_requests().is_empty(), "an answer started a turn");
    // /stop in that thread cancels the question, and it says so.
    first.say("/poke 2").await;
    second.question().await;
    second.ask("/stop", "Nothing is running").await;
    first.wait_for("the user cancelled the question").await;
    second.wait_for("→ ⏹ cancelled").await;

    // A listener without an answer gives up after its timeout, and says so.
    first.ask("/wait", r#"got: {"timeout":true}"#).await;
    first.ask("/forget", "listening briefly").await;
    tokio::time::sleep(std::time::Duration::from_millis(400)).await;
    first.ask("hello", "the agent answered").await;
}

const HINTS: &str = r###"export default function (august) {
  august.registerPromptSection("hints", "## Hints\nHINT-ONE");
  august.registerCommand("hint", (text) => { august.registerPromptSection("hints", `## Hints\n${text}`); return "hinted"; });
}"###;

#[tokio::test]
async fn extensions_add_prompt_sections_fixed_per_conversation() {
    if !have_bun() {
        return;
    }
    let fake = Fake::llm(Box::new(|_| reply_text("ok"))).await;
    let gw = august(&fake, Setup { home: &[("extensions/hints/index.ts", HINTS)], ..Default::default() }).await;
    let mut chat = gw.chat().await;
    let system = |i: usize| fake.llm_requests()[i]["messages"][0]["content"].as_str().unwrap().to_string();

    chat.ask("first", "ok").await;
    assert!(system(0).contains("## Hints\nHINT-ONE"), "{}", system(0));
    // A changed section waits for the next conversation, so the prompt stays cacheable.
    chat.ask("/hint HINT-TWO", "hinted").await;
    chat.ask("second", "ok").await;
    assert_eq!(system(1), system(0));
    chat.ask("/new", "new conversation").await;
    chat.ask("third", "ok").await;
    assert!(system(2).contains("HINT-TWO") && !system(2).contains("HINT-ONE"), "{}", system(2));
}

const COUNTER: &str = r#"export default function (august) {
  august.registerCommand("count", async () => {
    const n = ((await august.store.get("count")) ?? 0) + 1;
    await august.store.set("count", n);
    await august.store.set(`seen:${n}`, { n });
    return `count ${n}, seen ${(await august.store.list("seen:")).length}`;
  });
}"#;

#[tokio::test]
async fn extensions_keep_state_in_the_store_across_reloads() {
    if !have_bun() {
        return;
    }
    let fake = Fake::llm(Box::new(|_| reply_text("ok"))).await;
    let gw = august(&fake, Setup { home: &[("extensions/counter/index.ts", COUNTER)], ..Default::default() }).await;
    let mut chat = gw.chat().await;
    chat.ask("/count", "count 1, seen 1").await;
    chat.ask("/reload", "Extensions reloaded").await;
    chat.ask("/count", "count 2, seen 2").await;
}

const TURNKIT: &str = r#"
import type { August } from "august";

export default function (august: August) {
  august.needs("turns", "messaging");
  const run = async (ctx, turn) => august.turns.wait(await august.turns.start(ctx.thread!, { source: "turnkit", ...turn }));
  august.registerCommand("quiet", async (_, ctx) => {
    const out = await run(ctx, { text: "check quietly", mode: "quiet" });
    return `quiet ${out.status}: ${out.reply}`;
  });
  august.registerCommand("fork", async (_, ctx) => {
    const out = await run(ctx, { text: "look back", mode: "fork", tools: ["read"] });
    return `fork ${out.status}: ${out.reply} | ${out.toolCalls.map((c) => `${c.name}:${c.isError}`).join(",")}`;
  });
  august.registerCommand("spawn", async (_, ctx) => {
    const id = await august.turns.start(ctx.thread!, { text: "a long job", mode: "fresh" });
    setTimeout(async () => ctx.send(`spawned ${(await august.turns.wait(id)).status}`), 0);
    return `running: ${(await august.turns.list(ctx.thread!)).map((t) => t.mode).join(",")}`;
  });
  august.on("turn_end", async ({ status }, ctx) => {
    if (ctx.turn?.source === "turnkit") await ctx.send(`ended ${ctx.turn.mode} ${status}`);
  });
}
"#;

#[tokio::test(flavor = "multi_thread")]
async fn extensions_run_quiet_fork_and_fresh_turns() {
    if !have_bun() {
        return;
    }
    let llm: Llm = Box::new(|req| {
        let all = req["messages"].to_string();
        let last = messages(req).last().unwrap();
        if all.contains("a long job") {
            std::thread::sleep(Duration::from_secs(3));
            return reply_text("job done");
        }
        if last["role"] == "tool" {
            return reply_text("FORK-REPLY");
        }
        match last_user_text(req) {
            t if t.contains("look back") => reply_tool("write", json!({"path": "x.txt", "content": "no"})),
            t if t.contains("check quietly") => reply_text("QUIET-REPLY"),
            _ => reply_text(&format!("saw quiet: {}", all.contains("QUIET-REPLY"))),
        }
    });
    let fake = Fake::llm(llm).await;
    let gw = august(&fake, Setup { home: &[("extensions/turnkit/index.ts", TURNKIT)], ..Default::default() }).await;
    let mut chat = gw.chat().await;

    // A quiet turn shows nothing but stays in the conversation.
    chat.ask("/quiet", "quiet ok: QUIET-REPLY").await;
    chat.wait_for("ended quiet ok").await;
    assert_eq!(chat.texts().iter().filter(|t| t.contains("QUIET-REPLY")).count(), 1, "{:?}", chat.texts());
    chat.ask("hello", "saw quiet: true").await;

    // A fork may only call what it's allowed, and leaves the conversation as it was.
    chat.ask("/fork", "fork ok: FORK-REPLY | write:true").await;
    assert!(!gw.workspace.join("x.txt").exists());
    chat.ask("hello again", "saw quiet: true").await;
    let last = fake.llm_requests().last().unwrap().to_string();
    assert!(!last.contains("look back") && !last.contains("FORK-REPLY"), "the fork was kept");

    // /stop cancels the thread's turns, sub-agents included.
    chat.ask("/spawn", "running: fresh").await;
    chat.ask("/stop", "Stopping…").await;
    chat.wait_for("spawned cancelled").await;
}

const CARD: &str = r#"export default function (august) {
  august.registerCommand("card", async (_, ctx) => {
    await ctx.send({
      text: "Pick one",
      buttons: [[{ id: "a", label: "Alpha" }], [{ id: "b", label: "Beta" }]],
      files: [`${august.workspace}/note.txt`],
    });
  });
}"#;

#[tokio::test]
async fn one_message_carries_text_button_rows_and_files() {
    if !have_bun() {
        return;
    }
    let fake = Fake::llm(Box::new(|_| reply_text("ok"))).await;
    let setup = Setup { seed: &[("note.txt", b"hi")], home: &[("extensions/card/index.ts", CARD)], ..Default::default() };
    let gw = august(&fake, setup).await;
    let mut chat = gw.chat().await;
    chat.say("/card").await;
    let card = chat.wait_for("Pick one").await;
    let labels: Vec<&str> = card.buttons.iter().map(|(_, l)| l.as_str()).collect();
    assert_eq!(labels, ["Alpha", "Beta"]);
    assert!(chat.files().iter().any(|(p, c)| p.ends_with("note.txt") && c == "Pick one"), "{:?}", chat.files());
}

const NOSY: &str = r#"export default function (august) {
  august.registerCommand("peek", async () => `${(await august.messengers()).length} messengers`);
  august.registerCommand("here", async (_, ctx) => { await ctx.send("answering here is fine"); });
}"#;

#[tokio::test]
async fn extensions_only_get_what_they_declare() {
    if !have_bun() {
        return;
    }
    let fake = Fake::llm(Box::new(|_| reply_text("ok"))).await;
    let gw = august(&fake, Setup { home: &[("extensions/nosy/index.ts", NOSY)], ..Default::default() }).await;
    let mut chat = gw.chat().await;
    let refused = chat.ask("/peek", "failed").await;
    assert!(refused.contains("needs the `messaging` permission"), "{refused}");
    // Answering in the thread of the call in progress needs nothing.
    chat.ask("/here", "answering here is fine").await;
}

const LIFECYCLE: &str = r#"
import { writeFileSync } from "node:fs";

export default function (august) {
  const mark = (name) => writeFileSync(`${august.workspace}/${name}`, "yes");
  // A long tool that stops when August stops waiting for it.
  august.registerTool({
    name: "slow",
    description: "Takes long",
    execute: (_, ctx) => new Promise((resolve) => ctx.signal.addEventListener("abort", () => { mark("aborted"); resolve("late"); })),
  });
  // A hook slower than its own timeout is skipped; the turn goes on without it.
  august.on("before_turn", async ({ text }) => {
    if (text.includes("hang")) await new Promise((r) => setTimeout(r, 3000));
    return { text: text + " (seen by the hook)" };
  }, { timeout: 300 });
  august.on("shutdown", () => mark("shut-down"));
}
"#;

#[tokio::test(flavor = "multi_thread")]
async fn extensions_hear_cancels_and_shutdowns_and_set_hook_timeouts() {
    if !have_bun() {
        return;
    }
    let llm: Llm = Box::new(|req| {
        let text = last_user_text(req);
        if text.contains("run slow") && messages(req).last().unwrap()["role"] != "tool" {
            reply_tool("slow", json!({}))
        } else {
            reply_text(&format!("got: {}", text.split("] ").nth(1).unwrap_or(&text)))
        }
    });
    let fake = Fake::llm(llm).await;
    let gw = august(&fake, Setup { home: &[("extensions/life/index.ts", LIFECYCLE)], ..Default::default() }).await;
    let mut chat = gw.chat().await;

    // /stop while the extension's tool runs: the extension is told to stop.
    chat.say("run slow").await;
    chat.wait_for("`slow`").await;
    chat.ask("/stop", "Stopping…").await;
    chat.wait_until("the tool aborted", |_| gw.workspace.join("aborted").exists()).await;

    // Its own timeout: the hanging hook is skipped, the quick one applies.
    chat.ask("hang please", "got: hang please").await;
    let last = chat.texts().last().unwrap().clone();
    assert!(!last.contains("seen by the hook"), "{last}");
    chat.ask("quick", "got: quick (seen by the hook)").await;

    // A reload tells it to clean up first.
    chat.ask("/reload", "Extensions reloaded").await;
    assert!(gw.workspace.join("shut-down").exists());
}

const CONTROL: &str = r#"export default function (august) {
  august.needs("admin", "models");
  august.registerTool({ name: "dial", description: "A dial", execute: () => "turned" });
  august.registerCommand("owners", async () => {
    const tools = await august.tools();
    const owner = (name) => tools.find((t) => t.name === name)?.owner;
    const ops = await august.ops();
    return `dial: ${owner("dial")}, read: ${owner("read")}, model_set needs ${ops.find((o) => o.name === "model_set").permission}`;
  });
  august.registerCommand("swap", async (model) => {
    const now = await august.model.set(model);
    return `swapped to ${now.model}`;
  });
  august.registerCommand("off", async (name) => {
    await august.extensions.disable(name);
    const list = await august.extensions.list();
    return `${name} is ${list.find((e) => e.name === name).state}`;
  });
}"#;

const MEEK: &str = r#"export default function (august) {
  august.registerCommand("coup", async () => { await august.extensions.disable("control"); return "done"; });
}"#;

#[tokio::test]
async fn extensions_drive_the_core_through_its_operations() {
    if !have_bun() {
        return;
    }
    let fake = Fake::llm(Box::new(|_| reply_text("ok"))).await;
    let home = [("extensions/control/index.ts", CONTROL), ("extensions/meek/index.ts", MEEK)];
    let gw = august(&fake, Setup { home: &home, ..Default::default() }).await;
    let mut chat = gw.chat().await;

    // The tables of tools and operations, with who offers what and what it needs.
    chat.ask("/owners", "dial: control, read: august, model_set needs models").await;

    // Switching the model is the same operation /model runs: the next call uses it.
    chat.ask("/swap other-model", "swapped to other-model").await;
    chat.ask("hi", "ok").await;
    assert_eq!(fake.llm_requests().last().unwrap()["model"], "other-model");
    chat.ask("/model", "openai · other-model").await;

    // Without `admin`, an extension can't touch others; with it, it can.
    let refused = chat.ask("/coup", "failed").await;
    assert!(refused.contains("needs the `admin` permission"), "{refused}");
    chat.ask("/off meek", "meek is disabled").await;
    chat.ask("/extensions", "⏸ meek").await;
}

const WEATHER: &str = r#"export default function (august) {
  august.settings.schema({
    type: "object",
    properties: { city: { type: "string", default: "Berlin" }, api_key: { type: "string", secret: true } },
  });
  august.registerCommand("weather", async () => {
    const { city, api_key } = await august.settings.get();
    return `weather for ${city} with ${api_key ?? "no key"}`;
  });
  august.on("config_changed", async ({ path }) => {
    if (path.startsWith("extensions.weather")) await august.send("home", `noticed ${path}`);
  });
  august.needs("messaging");
}"#;

const SNOOP: &str = r#"export default function (august) {
  august.needs("config");
  august.registerCommand("snoop", async () => JSON.stringify(await august.config.get("extensions.weather.settings")));
  august.registerCommand("silence", async () => { await august.config.set("extensions.weather.enabled", false); return "done"; });
}"#;

#[tokio::test]
async fn extensions_have_settings_the_user_sets_and_secrets_stay_hidden() {
    if !have_bun() {
        return;
    }
    let fake = Fake::llm(Box::new(|_| reply_text("ok"))).await;
    let home = [("extensions/weather/index.ts", WEATHER), ("extensions/snoop/index.ts", SNOOP), ("extensions/meek/index.ts", MEEK)];
    let gw = august(&fake, Setup { home: &home, ..Default::default() }).await;
    let mut chat = gw.chat().await;

    // Defaults from the schema until the user sets something.
    chat.ask("/weather", "weather for Berlin with no key").await;
    chat.ask("/home", "home thread").await;

    // The user sets a secret; it's shown masked, and the extension hears of the change.
    let shown = chat.ask("/config extensions.weather.settings.api_key abc123", "settings.api_key` =").await;
    assert!(shown.contains("••••") && !shown.contains("abc123"), "{shown}");
    chat.wait_for("noticed extensions.weather.settings.api_key").await;
    chat.ask("/config extensions.weather.settings.city Paris", "Paris").await;
    chat.ask("/weather", "weather for Paris with abc123").await;
    let file = std::fs::read_to_string(gw.home.join("config/extensions/weather.json")).unwrap();
    assert!(file.contains("abc123") && file.contains("Paris"), "{file}");

    // Another extension with `config` reads settings with secrets masked, and can't turn
    // extensions off; one without `config` can't read them at all.
    let seen = chat.ask("/snoop", "Paris").await;
    assert!(seen.contains("••••") && !seen.contains("abc123"), "{seen}");
    chat.ask("/silence", "only the user turns extensions on and off").await;
    chat.ask("/weather", "weather for Paris").await;
}

const MONITOR: &str = r#"export default function (august) {
  august.needs("messaging");
  august.on("extension_state", async ({ name, state, error }) => {
    if (name === "fragile") await august.send("home", `monitor: ${name} is ${state}${error ? " (" + error.split(":")[0] + ")" : ""}`);
  });
}"#;

#[tokio::test]
async fn a_monitor_reports_crashed_extensions_to_the_home_thread() {
    if !have_bun() {
        return;
    }
    let pick: fn(&str) -> Value = |text| reply_tool(if text.contains("crash") { "crash" } else { "alive" }, json!({}));
    let fake = Fake::llm(llm(pick)).await;
    let home = [("extensions/fragile/index.ts", FRAGILE), ("extensions/monitor/index.ts", MONITOR)];
    let gw = august(&fake, Setup { home: &home, ..Default::default() }).await;
    let mut home_chat = gw.chat().await;
    home_chat.ask("/home", "home thread").await;

    // The crash happens in another thread; the report goes home, then the restart.
    let mut other = gw.chat().await;
    other.ask("crash please", "Result: bye").await;
    home_chat.wait_for("monitor: fragile is failed (crashed)").await;
    home_chat.wait_for("monitor: fragile is running").await;
    assert!(!other.texts().iter().any(|t| t.contains("monitor:")), "{:?}", other.texts());
}
