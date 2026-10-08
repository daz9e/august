//! Conversations as stored, addressable entities: an extension starts one with its own
//! model, instructions and tools, lists them and switches a thread back to an older one.
//! Needs bun; skipped when it is not installed.

use crate::support::*;
use serde_json::Value;

fn have_bun() -> bool {
    let found = std::env::var_os("PATH").is_some_and(|p| std::env::split_paths(&p).any(|d| d.join("bun").is_file()));
    if !found {
        eprintln!("skipping: bun is not installed");
    }
    found
}

const PERSONAS: &str = r#"
export default function (august) {
  august.needs("sessions");
  august.registerCommand("pirate", async (_, ctx) => {
    const id = await august.sessions.new(ctx.thread, {
      name: "pirate",
      settings: { model: "pirate-model", system: "Talk like a pirate.", tools: ["read_file"] },
    });
    return `pirate session ${id.length > 0}`;
  });
  august.registerCommand("back", async (_, ctx) => {
    const all = await august.sessions.list(ctx.thread);
    const old = all.find((s) => s.name !== "pirate");
    await august.sessions.switch(ctx.thread, old.id);
    return `back to ${old.messages} messages; ${all.length} sessions`;
  });
}
"#;

fn user_texts(req: &Value) -> String {
    req["messages"].as_array().unwrap().iter().filter(|m| m["role"] == "user").map(|m| m["content"].to_string()).collect()
}

#[tokio::test]
async fn sessions_have_their_own_settings_and_can_be_switched() {
    if !have_bun() {
        return;
    }
    let fake = Fake::llm(Box::new(|_| reply_text("ok"))).await;
    let gw = august(&fake, Setup { home: &[("extensions/personas/index.ts", PERSONAS)], ..Default::default() }).await;
    let mut chat = gw.chat().await;
    chat.ask("first words", "ok").await;

    chat.ask("/pirate", "pirate session true").await;
    chat.ask("ahoy", "ok").await;
    let req = fake.llm_requests().last().unwrap().clone();
    assert_eq!(req["model"], "pirate-model");
    assert!(req["messages"][0]["content"].as_str().unwrap().contains("Talk like a pirate."));
    let tools: Vec<&str> = req["tools"].as_array().unwrap().iter().filter_map(|t| t["function"]["name"].as_str()).collect();
    assert_eq!(tools, ["read_file"]);
    assert!(!user_texts(&req).contains("first words"));

    chat.ask("/back", "back to 2 messages; 2 sessions").await;
    chat.ask("again", "ok").await;
    let req = fake.llm_requests().last().unwrap().clone();
    assert_eq!(req["model"], "fake-model");
    assert!(user_texts(&req).contains("first words") && !user_texts(&req).contains("ahoy"));

    // The binding survives a restart.
    let mut gw = gw;
    gw.restart().await;
    let mut chat = gw.chat().await;
    chat.ask("after restart", "ok").await;
    assert!(user_texts(fake.llm_requests().last().unwrap()).contains("first words"));
}

const AUDIT: &str = r#"
export default function (august) {
  august.needs("sessions", "tools");
  august.registerCommand("peek", async (_, ctx) => (await ctx.callTool("read_file", { path: "note.txt" })).output);
  august.registerCommand("mark", async (_, ctx) => { await august.journal.append({ thread: ctx.thread }, "bookmark", { at: "here" }); return "marked"; });
  august.registerCommand("log", async (_, ctx) => {
    const entries = await august.sessions.history({ thread: ctx.thread });
    return entries.map((e) => e.kind === "tool" ? `tool:${e.data.tool}:${e.caller}` : e.kind === "custom" ? `custom:${e.data.type}:${e.caller}` : e.kind).join(",");
  });
}
"#;

#[tokio::test]
async fn the_journal_records_what_happened_in_a_conversation() {
    if !have_bun() {
        return;
    }
    let fake = Fake::llm(Box::new(|req| {
        let last = req["messages"].as_array().unwrap().last().unwrap();
        match last["role"].as_str() {
            Some("tool") => reply_text("done"),
            _ => reply_tool("read_file", serde_json::json!({"path": "note.txt"})),
        }
    }))
    .await;
    let seed: &[(&str, &[u8])] = &[("note.txt", b"hello")];
    let gw = august(&fake, Setup { home: &[("extensions/audit/index.ts", AUDIT)], seed, ..Default::default() }).await;
    let mut chat = gw.chat().await;
    chat.ask("read the note", "done").await;
    chat.ask("/peek", "hello").await;
    chat.ask("/mark", "marked").await;
    chat.ask(
        "/log",
        "turn_start,user_message,assistant,tool:read_file:model,assistant,turn_end,tool:read_file:ext:audit,custom:bookmark:ext:audit",
    )
    .await;
}

const ROUTER: &str = r#"
export default function (august) {
  august.on("session_start", async ({ reason, previous }, ctx) => {
    await ctx.send(`session_start ${reason} after ${previous ? "one" : "none"}`);
    return ctx.thread.messenger === "cli" ? { model: "terminal-model", system: "Be brief." } : undefined;
  });
}
"#;

#[tokio::test]
async fn session_start_sets_up_a_conversation_before_its_first_turn() {
    if !have_bun() {
        return;
    }
    let fake = Fake::llm(Box::new(|_| reply_text("ok"))).await;
    let gw = august(&fake, Setup { home: &[("extensions/router/index.ts", ROUTER)], ..Default::default() }).await;
    let mut chat = gw.chat().await;
    chat.ask("hello", "ok").await;
    chat.wait_for("session_start start after none").await;
    let req = fake.llm_requests().last().unwrap().clone();
    assert_eq!(req["model"], "terminal-model");
    assert!(req["messages"][0]["content"].as_str().unwrap().contains("Be brief."));

    // Only once per conversation; /new starts the next one.
    chat.ask("more", "ok").await;
    chat.ask("/new", "").await;
    chat.ask("fresh", "ok").await;
    chat.wait_for("session_start new after one").await;
    assert_eq!(chat.texts().iter().filter(|t| t.starts_with("session_start")).count(), 2);
}

const GUARD: &str = r#"
export default function (august) {
  august.needs("sessions");
  let locked = true;
  august.on("session_before_new", ({ by }) => (locked ? { block: `locked (asked by ${by})` } : undefined));
  august.on("session_before_switch", ({ to }) => ({ block: `not to ${to}` }));
  august.on("session_changed", ({ reason }, ctx) => ctx.send(`changed: ${reason}`));
  august.registerCommand("unlock", () => { locked = false; return "unlocked"; });
  august.registerCommand("hop", async (_, ctx) => {
    try { await august.sessions.switch(ctx.thread, "nowhere"); return "hopped"; } catch (e) { return `hop failed: ${e.message}`; }
  });
}
"#;

#[tokio::test]
async fn extensions_can_refuse_a_new_or_switched_conversation() {
    if !have_bun() {
        return;
    }
    let fake = Fake::llm(Box::new(|_| reply_text("ok"))).await;
    let gw = august(&fake, Setup { home: &[("extensions/guard/index.ts", GUARD)], ..Default::default() }).await;
    let mut chat = gw.chat().await;
    chat.ask("hello", "ok").await;
    chat.ask("/new", "blocked by an extension: locked (asked by user)").await;
    chat.ask("/hop", "hop failed: blocked by an extension: not to nowhere").await;
    chat.ask("/unlock", "unlocked").await;
    chat.ask("/new", "Started a new conversation").await;
    chat.wait_for("changed: new").await;
}
