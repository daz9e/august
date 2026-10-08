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
