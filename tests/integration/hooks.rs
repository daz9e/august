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
  august.registerCommand("peek", async (_, ctx) => `peek: ${(await ctx.callTool("read_file", { path: "note.txt" })).output}`);
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
            _ => reply_tool("read_file", serde_json::json!({"path": "note.txt"})),
        }
    }))
    .await;
    let seed: &[(&str, &[u8])] = &[("note.txt", b"hello")];
    let gw = august(&fake, Setup { home: &[("extensions/watch/index.ts", WATCH)], seed, ..Default::default() }).await;
    let mut chat = gw.chat().await;

    chat.ask("read the note", "Result: hello (seen)").await;
    chat.wait_for("call read_file call_1 by model").await;
    chat.ask("/peek", "peek: hello").await;
    chat.wait_for("call read_file null by ext:watch").await;
}
