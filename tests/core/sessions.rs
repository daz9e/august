//! Conversations as stored, addressable entities: started with their own model,
//! instructions and tools, listed, switched, journaled, guarded by hooks, and reachable
//! without a chat.

use crate::support::*;
use august_ext::August;
use serde_json::{Value, json};

/// Registers `/name` running `op` with `params(args)` in the command's thread; replies with
/// `reply(result)` or the error.
fn op_command(a: &August, name: &str, op: &'static str, params: fn(&str) -> Value, reply: fn(Value) -> String) {
    a.register_command(name, "", move |args, ctx| async move {
        Ok(Some(match ctx.call(op, params(&args)).await {
            Ok(v) => reply(v),
            Err(e) => format!("{op} failed: {e}"),
        }))
    });
}

#[tokio::test]
async fn sessions_have_their_own_settings_and_can_be_switched() {
    let mut core = core()
        .ext("files", files)
        .ext("personas", |a| {
            a.needs(&["sessions"]);
            op_command(a, "pirate", "session_new", |_| json!({"name": "pirate", "settings": {"model": "pirate-model", "system": "Talk like a pirate.", "tools": ["read"]}}), |id| format!("pirate session {}", !id.as_str().unwrap().is_empty()));
            a.register_command("back", "", |_, ctx| async move {
                let all = ctx.call("sessions", json!({})).await?;
                let all = all.as_array().unwrap();
                let old = all.iter().find(|s| s["name"] != "pirate").unwrap();
                ctx.call("session_switch", json!({"session": old["id"]})).await?;
                Ok(Some(format!("back to {} messages; {} sessions", old["messages"], all.len())))
            });
        })
        .start()
        .await;
    let chat = core.chat("1");
    chat.ask("first words", "ok").await;

    chat.ask("/pirate", "pirate session true").await;
    chat.ask("ahoy", "ok").await;
    let req = core.requests().last().unwrap().clone();
    assert_eq!(req.model, "pirate-model");
    assert!(req.system.contains("Talk like a pirate."));
    assert_eq!(tool_names(&req), ["read"]);
    assert!(!all_text(&req).contains("first words"));

    chat.ask("/back", "back to 2 messages; 2 sessions").await;
    chat.ask("again", "ok").await;
    let req = core.requests().last().unwrap().clone();
    assert_eq!(req.model, MODEL);
    assert!(all_text(&req).contains("first words") && !all_text(&req).contains("ahoy"));

    // The binding survives a restart.
    core.restart().await;
    core.chat("1").ask("after restart", "ok").await;
    assert!(all_text(core.requests().last().unwrap()).contains("first words"));
}

#[tokio::test]
async fn the_journal_records_what_happened_in_a_conversation() {
    let core = core()
        .model(|req| if tool_result(req).is_some() { text("done") } else { tool("read", json!({"path": "note.txt"})) })
        .seed("note.txt", b"hello")
        .ext("files", files)
        .ext("audit", |a| {
            a.needs(&["sessions", "tools"]);
            a.register_command("peek", "", |_, ctx| async move { Ok(Some(ctx.call_tool("read", json!({"path": "note.txt"})).await?.0)) });
            op_command(a, "mark", "journal_append", |_| json!({"type": "bookmark", "data": {"at": "here"}}), |_| "marked".into());
            op_command(a, "log", "history", |_| json!({}), |entries| {
                let kinds: Vec<String> = entries.as_array().unwrap().iter().map(|e| match e["kind"].as_str().unwrap() {
                    "tool" => format!("tool:{}:{}", e["data"]["tool"].as_str().unwrap(), e["caller"].as_str().unwrap()),
                    "custom" => format!("custom:{}:{}", e["data"]["type"].as_str().unwrap(), e["caller"].as_str().unwrap()),
                    k => k.to_string(),
                }).collect();
                kinds.join(",")
            });
        })
        .start()
        .await;
    let chat = core.chat("1");
    chat.ask("read the note", "done").await;
    chat.ask("/peek", "hello").await;
    chat.ask("/mark", "marked").await;
    chat.ask("/log", "turn_start,user_message,assistant,tool:read:model,assistant,turn_end,tool:read:ext:audit,custom:bookmark:ext:audit").await;
}

#[tokio::test]
async fn session_start_sets_up_a_conversation_before_its_first_turn() {
    let core = core()
        .ext("router", |a| {
            a.on("session_start", |d, ctx| async move {
                ctx.send(&format!("session_start {} after {}", d["reason"].as_str().unwrap(), if d["previous"].is_null() { "none" } else { "one" })).await?;
                Ok((ctx.thread.unwrap().messenger == MESSENGER).then(|| json!({"model": "terminal-model", "system": "Be brief."})))
            });
        })
        .start()
        .await;
    let chat = core.chat("1");
    chat.ask("hello", "ok").await;
    chat.wait_for("session_start start after none").await;
    let req = core.requests().last().unwrap().clone();
    assert_eq!(req.model, "terminal-model");
    assert!(req.system.contains("Be brief."));

    // Only once per conversation; a new one starts the next.
    chat.ask("more", "ok").await;
    core.call_in("1", "session_new", json!({"as_user": true})).await.unwrap();
    chat.ask("fresh", "ok").await;
    chat.wait_for("session_start new after one").await;
    assert_eq!(chat.texts().iter().filter(|t| t.starts_with("session_start")).count(), 2);
}

#[tokio::test]
async fn extensions_can_refuse_a_new_or_switched_conversation() {
    let locked = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
    let lock = locked.clone();
    let core = core()
        .ext("guard", move |a| {
            let lock = lock.clone();
            a.on("session_before_new", move |d, _| {
                let locked = lock.load(std::sync::atomic::Ordering::SeqCst);
                async move { Ok(locked.then(|| json!({"block": format!("locked (asked by {})", d["by"].as_str().unwrap())}))) }
            });
            a.on("session_before_switch", |d, _| async move { Ok(Some(json!({"block": format!("not to {}", d["to"].as_str().unwrap())}))) });
            a.on("session_changed", |d, ctx| async move {
                ctx.send(&format!("changed: {}", d["reason"].as_str().unwrap())).await?;
                Ok(None)
            });
        })
        .start()
        .await;
    let chat = core.chat("1");
    chat.ask("hello", "ok").await;
    let new = || core.call_in("1", "session_new", json!({"as_user": true}));
    let refused = new().await.unwrap_err().to_string();
    assert!(refused.contains("blocked by an extension: locked (asked by user)"), "{refused}");
    let refused = core.call_in("1", "session_switch", json!({"session": "nowhere"})).await.unwrap_err().to_string();
    assert!(refused.contains("blocked by an extension: not to nowhere"), "{refused}");
    locked.store(false, std::sync::atomic::Ordering::SeqCst);
    new().await.unwrap();
    chat.wait_for("changed: new").await;
}

#[tokio::test]
async fn an_extension_talks_to_a_conversation_of_no_chat() {
    let core = core()
        .model(|req| {
            let all = all_text(req);
            text(if last_user_text(req).contains("which number") && all.contains("keep 42") { "it was 42" } else { "kept" })
        })
        .ext("notebook", |a| {
            a.needs(&["sessions", "turns"]);
            let (me, book) = (a.clone(), std::sync::Arc::new(tokio::sync::Mutex::new(None::<String>)));
            a.register_command("note", "", move |args, _| {
                let (me, book) = (me.clone(), book.clone());
                async move {
                    let mut book = book.lock().await;
                    if book.is_none() {
                        *book = Some(me.call("session_new", json!({"name": "notebook"})).await?.as_str().unwrap().to_string());
                    }
                    let thread = august_ext::Thread { messenger: "session".into(), id: book.clone().unwrap() };
                    let id = me.start_turn(&thread, json!({"text": args})).await?;
                    let out = me.wait_turn(id, std::time::Duration::from_secs(10)).await?;
                    let messages = me.call("messages", json!({"thread": thread})).await?;
                    Ok(Some(format!("{}: {} ({} messages)", out["status"].as_str().unwrap(), out["reply"].as_str().unwrap(), messages["messages"].as_array().unwrap().len())))
                }
            });
        })
        .start()
        .await;
    let chat = core.chat("1");
    chat.ask("/note keep 42", "ok: kept (2 messages)").await;
    chat.ask("/note which number?", "ok: it was 42 (4 messages)").await;

    // The chat's own conversation never saw it.
    chat.ask("which number?", "kept").await;
    assert!(!all_text(core.requests().last().unwrap()).contains("keep 42"));
}

#[tokio::test]
async fn a_conversation_sees_only_the_prompt_sections_it_names() {
    let core = core()
        .ext("memory", |a| a.register_prompt_section("facts", "## Facts\nLIKES-TEA"))
        .ext("code", |a| {
            a.register_prompt_section("project", "## Project\nUSES-RUST");
            a.on("session_start", |_, _| async { Ok(Some(json!({"sections": ["project"]}))) });
        })
        .start()
        .await;
    core.chat("1").ask("hi", "ok").await;
    let system = core.requests().last().unwrap().system.clone();
    assert!(system.contains("USES-RUST") && !system.contains("LIKES-TEA"), "{system}");
}
