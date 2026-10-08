//! The hook pipeline: mutating hooks run as a chain in the user's order, observers run at
//! once. Needs bun; skipped when it is not installed.

use crate::support::*;
use serde_json::Value;

fn have_bun() -> bool {
    let found = std::env::var_os("PATH").is_some_and(|p| std::env::split_paths(&p).any(|d| d.join("bun").is_file()));
    if !found {
        eprintln!("skipping: bun is not installed");
    }
    found
}

fn last_user_text(req: &Value) -> String {
    let m = req["messages"].as_array().unwrap().iter().rev().find(|m| m["role"] == "user").unwrap();
    m["content"].as_str().map(String::from).unwrap_or_else(|| m["content"].to_string())
}

// `alpha` comes first by name. Its turn_end only succeeds if `zeta`'s runs meanwhile.
const ALPHA: &str = r#"
import { existsSync } from "fs";
export default function (august) {
  august.on("message_in", ({ text }) => ({ text: text + " A" }));
  august.on("turn_end", async (_, ctx) => {
    const flag = `${process.env.AUGUST_WORKSPACE}/zeta-ran`;
    for (let i = 0; i < 50 && !existsSync(flag); i++) await new Promise((r) => setTimeout(r, 100));
    await ctx.send(existsSync(flag) ? "observers ran together" : "observers waited");
  });
}
"#;

const ZETA: &str = r#"
import { writeFileSync } from "fs";
export default function (august) {
  august.on("message_in", ({ text }) => ({ text: text + " Z" }));
  august.on("turn_end", () => { writeFileSync(`${process.env.AUGUST_WORKSPACE}/zeta-ran`, ""); });
}
"#;

#[tokio::test]
async fn hooks_chain_in_the_users_order_and_observers_run_at_once() {
    if !have_bun() {
        return;
    }
    let fake = Fake::llm(Box::new(|req| reply_text(&format!("got: {}", last_user_text(req))))).await;
    let home = [("extensions/alpha/index.ts", ALPHA), ("extensions/zeta/index.ts", ZETA)];
    let gw = august(&fake, Setup { home: &home, ..Default::default() }).await;
    let mut chat = gw.chat().await;

    // By default the chain runs by name; each handler sees what the previous one returned.
    chat.ask("one", "one A Z").await;
    chat.wait_for("observers ran together").await;

    // The user puts zeta first.
    chat.ask(r#"/config august.hooks.order ["zeta"]"#, "zeta").await;
    chat.ask("two", "two Z A").await;
}

const WATCH: &str = r#"
export default function (august) {
  august.needs("tools");
  august.on("tool_call", async ({ tool, id, caller }, ctx) => { await ctx.send(`call ${tool} ${id} by ${caller}`); });
  august.on("tool_result", ({ caller, output }) => (caller === "model" ? { output: output + " (seen)" } : undefined));
  august.registerCommand("peek", async (_, ctx) => `peek: ${(await ctx.callTool("read", { path: "note.txt" })).output}`);
}
"#;

#[tokio::test]
async fn tool_hooks_know_the_call_and_who_made_it() {
    if !have_bun() {
        return;
    }
    let fake = Fake::llm(Box::new(|req| {
        let last = req["messages"].as_array().unwrap().last().unwrap();
        match last["role"].as_str() {
            Some("tool") => reply_text(&format!("Result: {}", last["content"].as_str().unwrap_or(""))),
            _ => reply_tool("read", serde_json::json!({"path": "note.txt"})),
        }
    }))
    .await;
    let seed: &[(&str, &[u8])] = &[("note.txt", b"hello")];
    let gw = august(&fake, Setup { home: &[("extensions/watch/index.ts", WATCH)], seed, ..Default::default() }).await;
    let mut chat = gw.chat().await;

    chat.ask("read the note", "Result: hello (seen)").await;
    chat.wait_for("call read call_1 by model").await;
    chat.ask("/peek", "peek: hello").await;
    chat.wait_for("call read null by ext:watch").await;
}

const STOPPER: &str = r#"
export default function (august) {
  august.on("tool_call", async (_, ctx) => { await ctx.send(`turn ${ctx.turn.id} runs`); });
  august.on("stop", async ({ turns }, ctx) => { await ctx.send(`stopping turns ${turns.join(",")}`); });
}
"#;

#[tokio::test]
async fn stop_tells_extensions_which_turns_it_cancels() {
    if !have_bun() {
        return;
    }
    let fake = Fake::llm(Box::new(|_| reply_tool("bash", serde_json::json!({"command": "sleep 30"})))).await;
    // Nobody asks about the command: this is about /stop.
    let home = [("extensions/stopper/index.ts", STOPPER), ("config/extensions/approvals.json", r#"{"enabled": false}"#)];
    let gw = august(&fake, Setup { home: &home, ..Default::default() }).await;
    let mut chat = gw.chat().await;
    chat.say("start a long job").await;
    let runs = chat.wait_for(" runs").await.text;
    let id = runs.trim_start_matches("turn ").trim_end_matches(" runs");
    chat.ask("/stop", "Stopping").await;
    chat.wait_for(&format!("stopping turns {id}")).await;
}

const ROUTER: &str = r#"
export default function (august) {
  august.on("llm_call", async ({ step, model, tools }, ctx) => {
    await ctx.send(`step ${step} on ${model} with ${tools.includes("read") && tools.includes("bash")}`);
    return step === 0 ? { model: "cheap-model", tools: ["read"] } : undefined;
  });
}
"#;

#[tokio::test]
async fn llm_call_picks_the_model_and_tools_of_one_call() {
    if !have_bun() {
        return;
    }
    let fake = Fake::llm(Box::new(|req| {
        let last = req["messages"].as_array().unwrap().last().unwrap();
        match last["role"].as_str() {
            Some("tool") => reply_text("done"),
            _ => reply_tool("read", serde_json::json!({"path": "note.txt"})),
        }
    }))
    .await;
    let seed: &[(&str, &[u8])] = &[("note.txt", b"hello")];
    let gw = august(&fake, Setup { home: &[("extensions/router/index.ts", ROUTER)], seed, ..Default::default() }).await;
    let mut chat = gw.chat().await;
    chat.ask("read the note", "done").await;
    chat.wait_for("step 0 on fake-model with true").await;

    // The first call went to the cheaper model with one tool; the next one is back to normal.
    let reqs = fake.llm_requests();
    let names = |r: &Value| -> Vec<String> {
        r["tools"].as_array().into_iter().flatten().filter_map(|t| t["function"]["name"].as_str().map(String::from)).collect()
    };
    assert_eq!(reqs[0]["model"], "cheap-model");
    assert_eq!(names(&reqs[0]), ["read"]);
    assert_eq!(reqs[1]["model"], "fake-model");
    assert!(names(&reqs[1]).len() > 1);
}

const CENSOR: &str = r#"
export default function (august) {
  august.on("message_out", ({ text }) =>
    text.includes("DROP ME") ? { block: true } : { text: text.replaceAll("sk-123", "[masked]") });
  august.registerCommand("noise", { description: "Says something dropped", handler: () => "DROP ME" });
}
"#;

#[tokio::test]
async fn message_out_changes_or_drops_what_august_sends() {
    if !have_bun() {
        return;
    }
    let fake = Fake::llm(Box::new(|_| reply_text("the key is sk-123"))).await;
    let gw = august(&fake, Setup { home: &[("extensions/censor/index.ts", CENSOR)], ..Default::default() }).await;
    let mut chat = gw.chat().await;
    chat.ask("what is the key?", "the key is [masked]").await;
    chat.say("/noise").await;
    chat.ask("/help", "/noise").await;
    let all = chat.history().join("\n");
    assert!(!all.contains("sk-123") && !all.contains("DROP ME"), "{all}");
}

const SETTLE: &str = r#"
export default function (august) {
  august.needs("turns");
  let started = false;
  august.on("turn_end", async ({ unattended }, ctx) => {
    if (unattended) return ctx.send("background turn ended");
    if (!started) { started = true; await august.turns.start(ctx.thread, { text: "think more", mode: "fork" }); }
  });
  august.on("turn_settled", (_, ctx) => ctx.send("settled"));
}
"#;

#[tokio::test]
async fn turn_settled_comes_when_nothing_runs_in_the_thread() {
    if !have_bun() {
        return;
    }
    let fake = Fake::llm(Box::new(|req| {
        // The background turn is slow, so it outlives the visible one.
        if last_user_text(req).contains("think more") {
            std::thread::sleep(std::time::Duration::from_millis(1500));
        }
        reply_text("ok")
    }))
    .await;
    let gw = august(&fake, Setup { home: &[("extensions/settle/index.ts", SETTLE)], ..Default::default() }).await;
    let mut chat = gw.chat().await;
    chat.say("hello").await;
    chat.wait_for("settled").await;
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    let texts = chat.texts();
    assert_eq!(texts.iter().filter(|t| *t == "settled").count(), 1, "{texts:?}");
    let (bg, settled) = (texts.iter().position(|t| t == "background turn ended"), texts.iter().position(|t| t == "settled"));
    assert!(bg.is_some() && bg < settled, "{texts:?}");
}

const SENDER: &str = r#"
export default function (august) {
  august.on("message_in", async ({ source }, ctx) => { await ctx.send(`message_in from ${source}`); });
  august.registerCommand("stash", async (_, ctx) => { await ctx.prompt("remember the milk", { deliver: "nextTurn" }); return "stashed"; });
  august.registerCommand("report", async (_, ctx) => { await ctx.prompt("all done", { source: "subagent" }); return "reported"; });
}
"#;

#[tokio::test]
async fn prompts_carry_their_source_and_delivery() {
    if !have_bun() {
        return;
    }
    let fake = Fake::llm(Box::new(|req| reply_text(&format!("got: {}", last_user_text(req).replace('\n', " | "))))).await;
    let gw = august(&fake, Setup { home: &[("extensions/sender/index.ts", SENDER)], ..Default::default() }).await;
    let mut chat = gw.chat().await;

    // A nextTurn message starts nothing; it rides along with the next turn.
    chat.ask("/stash", "stashed").await;
    chat.wait_for("message_in from ext:sender").await;
    chat.ask("hello", "remember the milk | ").await;
    assert_eq!(fake.llm_requests().len(), 1);
    assert!(chat.texts().iter().any(|t| t.contains("milk") && t.ends_with("hello")), "{:?}", chat.texts());

    // Another source is shown to the model and to message_in.
    chat.ask("/report", "reported").await;
    chat.wait_for("[from subagent] all done").await;
    chat.wait_for("message_in from subagent").await;
    chat.wait_for("message_in from user").await;
}

const MODELS: &str = r#"
export default function (august) {
  august.on("model_select", ({ model, previous }) =>
    model === "banned" ? { block: `not after ${previous}` } : model === "fast" ? { model: "fast-model-v2" } : undefined);
}
"#;

#[tokio::test]
async fn model_select_can_redirect_or_refuse_a_model_switch() {
    if !have_bun() {
        return;
    }
    let fake = Fake::llm(Box::new(|_| reply_text("ok"))).await;
    let gw = august(&fake, Setup { home: &[("extensions/models/index.ts", MODELS)], ..Default::default() }).await;
    let mut chat = gw.chat().await;
    chat.ask("/model banned", "blocked by an extension: not after fake-model").await;
    chat.ask("/model fast", "fast-model-v2").await;
    chat.ask("hi", "ok").await;
    assert_eq!(fake.llm_requests().last().unwrap()["model"], "fast-model-v2");
}

const EVENTS: &str = r#"
export default function (august) {
  const seen = [];
  august.on("turn_event", ({ kind, text, tool }) => { seen.push(kind === "text" ? `text:${text}` : kind === "tool" ? `tool:${tool}` : kind); });
  august.registerCommand("events", () => seen.join(",") || "none");
}
"#;

#[tokio::test]
async fn turn_event_streams_what_a_visible_turn_does_in_order() {
    if !have_bun() {
        return;
    }
    let fake = Fake::llm(Box::new(|req| {
        let last = req["messages"].as_array().unwrap().last().unwrap();
        match last["role"].as_str() {
            Some("tool") => reply_text("done"),
            _ => reply_tool("read", serde_json::json!({"path": "note.txt"})),
        }
    }))
    .await;
    let seed: &[(&str, &[u8])] = &[("note.txt", b"hello")];
    let gw = august(&fake, Setup { home: &[("extensions/events/index.ts", EVENTS)], seed, ..Default::default() }).await;
    let mut chat = gw.chat().await;
    chat.ask("read the note", "done").await;
    // Events reach the hook in the background: ask until the last one is in.
    for _ in 0..50 {
        if chat.ask("/events", "").await.contains("text:done") {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert!(chat.texts().iter().any(|t| t == "tool:read,step,text:done"), "{:?}", chat.texts());
}
