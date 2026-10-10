//! The hook pipeline: mutating hooks run as a chain in the user's order, observers run at
//! once; tool, model, message and turn hooks see and change what they are about.

use crate::support::*;
use serde_json::json;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

fn append(tag: &'static str) -> impl Fn(&august_ext::August) + Send + Sync + 'static {
    move |a| a.on("message_in", move |d, _| async move { Ok(Some(json!({"text": format!("{} {tag}", d["text"].as_str().unwrap_or(""))}))) })
}

#[tokio::test]
async fn hooks_chain_in_the_users_order_and_observers_run_at_once() {
    let zeta_ran = Arc::new(AtomicBool::new(false));
    let (seen, set) = (zeta_ran.clone(), zeta_ran.clone());
    let core = core()
        .model(|req| text(&format!("got: {}", last_user_text(req))))
        .ext("alpha", move |a| {
            append("A")(a);
            // Its turn_end only succeeds if zeta's runs meanwhile.
            let seen = seen.clone();
            a.on("turn_end", move |_, ctx| {
                let seen = seen.clone();
                async move {
                    for _ in 0..200 {
                        if seen.load(Ordering::SeqCst) {
                            break;
                        }
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                    ctx.send(if seen.load(Ordering::SeqCst) { "observers ran together" } else { "observers waited" }).await?;
                    Ok(None)
                }
            });
        })
        .ext("zeta", move |a| {
            append("Z")(a);
            let set = set.clone();
            a.on("turn_end", move |_, _| {
                set.store(true, Ordering::SeqCst);
                async { Ok(None) }
            });
        })
        .start()
        .await;
    let chat = core.chat("1");

    // By default the chain runs by name; each handler sees what the previous one returned.
    chat.ask("one", "got: one A Z").await;
    chat.wait_for("observers ran together").await;

    // The user puts zeta first.
    core.call("config_set", json!({"path": "august.hooks.order", "value": ["zeta"]})).await.unwrap();
    chat.ask("two", "got: two Z A").await;
}

#[tokio::test]
async fn tool_hooks_know_the_call_and_who_made_it() {
    let core = core()
        .model(|req| match tool_result(req) {
            Some(out) => text(&format!("Result: {out}")),
            None => tool("read", json!({"path": "note.txt"})),
        })
        .seed("note.txt", b"hello")
        .ext("files", files)
        .ext("watch", |a| {
            a.needs(&["tools"]);
            a.on("tool_call", |d, ctx| async move {
                ctx.send(&format!("call {} {} by {}", d["tool"].as_str().unwrap(), d["id"].as_str().unwrap_or("null"), d["caller"].as_str().unwrap())).await?;
                Ok(None)
            });
            a.on("tool_result", |d, _| async move {
                Ok((d["caller"] == "model").then(|| json!({"output": format!("{} (seen)", d["output"].as_str().unwrap_or(""))})))
            });
            a.register_command("peek", "", |_, ctx| async move {
                let (out, _) = ctx.call_tool("read", json!({"path": "note.txt"})).await?;
                Ok(Some(format!("peek: {out}")))
            });
        })
        .start()
        .await;
    let chat = core.chat("1");

    chat.ask("read the note", "Result: hello (seen)").await;
    chat.wait_for("call read call_1 by model").await;
    chat.ask("/peek", "peek: hello").await;
    chat.wait_for("call read null by ext:watch").await;
}

#[tokio::test]
async fn stop_tells_extensions_which_turns_it_cancels() {
    let core = core()
        .model(|_| tool("slow", json!({})))
        .ext("slow", |a| {
            a.register_tool("slow", "Takes long", json!({"type": "object"}), |_, _| async {
                tokio::time::sleep(Duration::from_secs(60)).await;
                Ok("done".into())
            });
        })
        .ext("stopper", |a| {
            a.on("tool_call", |_, ctx| async move {
                ctx.send(&format!("turn {} runs", ctx.turn.as_ref().unwrap().id)).await?;
                Ok(None)
            });
            a.on("stop", |d, ctx| async move {
                let turns: Vec<String> = d["turns"].as_array().unwrap().iter().map(|t| t.to_string()).collect();
                ctx.send(&format!("stopping turns {}", turns.join(","))).await?;
                Ok(None)
            });
        })
        .start()
        .await;
    let chat = core.chat("1");
    chat.say("start a long job");
    let runs = chat.wait_for(" runs").await.text;
    let id = runs.trim_start_matches("turn ").trim_end_matches(" runs").to_string();

    assert_eq!(core.call_in(&chat.thread, "stop", json!({})).await.unwrap()["cancelled"], 1);
    chat.wait_for(&format!("stopping turns {id}")).await;
}

#[tokio::test]
async fn llm_call_picks_the_model_tools_and_options_of_one_call() {
    let core = core()
        .model(|req| match tool_result(req) {
            Some(_) => text("done"),
            None => tool("read", json!({"path": "note.txt"})),
        })
        .seed("note.txt", b"hello")
        .ext("files", files)
        .ext("router", |a| {
            a.on("llm_call", |d, ctx| async move {
                let tools: Vec<&str> = d["tools"].as_array().unwrap().iter().filter_map(|t| t.as_str()).collect();
                let both = tools.contains(&"read") && tools.contains(&"list");
                ctx.send(&format!("step {} on {} with {both}", d["step"], d["model"].as_str().unwrap())).await?;
                Ok((d["step"] == 0).then(|| json!({"model": "cheap-model", "tools": ["read"], "options": {"temperature": 0}})))
            });
        })
        .start()
        .await;
    let chat = core.chat("1");
    chat.ask("read the note", "done").await;
    chat.wait_for("step 0 on fake-model with true").await;

    // The first call went to the cheaper model with one tool and its own request options;
    // the next one is back to normal.
    let reqs = core.requests();
    assert_eq!(reqs[0].model, "cheap-model");
    assert_eq!(tool_names(&reqs[0]), ["read"]);
    assert_eq!(reqs[0].options, json!({"temperature": 0}));
    assert_eq!(reqs[1].model, MODEL);
    assert!(reqs[1].options.is_null());
    assert_eq!(tool_names(&reqs[1]).len(), 2);
}

#[tokio::test]
async fn message_out_changes_or_drops_what_august_sends() {
    let core = core()
        .model(|_| text("the key is sk-123"))
        .ext("censor", |a| {
            a.on("message_out", |d, _| async move {
                let text = d["text"].as_str().unwrap_or("");
                Ok(Some(if text.contains("DROP ME") { json!({"block": true}) } else { json!({"text": text.replace("sk-123", "[masked]")}) }))
            });
            a.register_command("noise", "Says something dropped", |_, _| async { Ok(Some("DROP ME".into())) });
        })
        .start()
        .await;
    let chat = core.chat("1");
    chat.ask("what is the key?", "the key is [masked]").await;
    let idle = chat.idles();
    chat.say("/noise");
    chat.idle(idle + 1).await;
    let all = chat.history().join("\n");
    assert!(!all.contains("sk-123") && !all.contains("DROP ME"), "{all}");
}

#[tokio::test]
async fn turn_settled_comes_when_nothing_runs_in_the_thread() {
    let core = core()
        .model(|req| {
            // The background turn is slow, so it outlives the visible one.
            let slow = last_user_text(req).contains("think more");
            text("ok").after(Duration::from_millis(if slow { 300 } else { 0 }))
        })
        .ext("settle", |a| {
            a.needs(&["turns"]);
            let me = a.clone();
            let started = Arc::new(AtomicBool::new(false));
            a.on("turn_end", move |d, ctx| {
                let (me, started) = (me.clone(), started.clone());
                async move {
                    if d["unattended"] == true {
                        ctx.send("background turn ended").await?;
                    } else if !started.swap(true, Ordering::SeqCst) {
                        me.start_turn(ctx.thread.as_ref().unwrap(), json!({"text": "think more", "conversation": "copy"})).await?;
                    }
                    Ok(None)
                }
            });
            a.on("turn_settled", |_, ctx| async move {
                ctx.send("settled").await?;
                Ok(None)
            });
        })
        .start()
        .await;
    let chat = core.chat("1");
    chat.say("hello");
    chat.wait_for("settled").await;
    settle(300).await;
    let texts = chat.texts();
    assert_eq!(texts.iter().filter(|t| *t == "settled").count(), 1, "{texts:?}");
    let (bg, settled) = (texts.iter().position(|t| t == "background turn ended"), texts.iter().position(|t| t == "settled"));
    assert!(bg.is_some() && bg < settled, "{texts:?}");
}

#[tokio::test]
async fn prompts_carry_their_source_and_delivery() {
    let core = core()
        .model(|req| text(&format!("got: {}", last_user_text(req).replace('\n', " | "))))
        .ext("sender", |a| {
            a.on("message_in", |d, ctx| async move {
                ctx.send(&format!("message_in from {}", d["source"].as_str().unwrap())).await?;
                Ok(None)
            });
            a.on("message_in", |d, ctx| async move {
                if !d["text"].as_str().unwrap_or("").contains("later:") {
                    return Ok(None);
                }
                ctx.send("held for later").await?;
                Ok(Some(json!({"deliver": "nextTurn"})))
            });
            a.register_command("stash", "", |_, ctx| async move {
                ctx.prompt_with("remember the milk", json!({"deliver": "nextTurn"})).await?;
                Ok(Some("stashed".into()))
            });
            a.register_command("report", "", |_, ctx| async move {
                ctx.prompt_with("all done", json!({"source": "subagent"})).await?;
                Ok(Some("reported".into()))
            });
        })
        .start()
        .await;
    let chat = core.chat("1");

    // A nextTurn message starts nothing; it rides along with the next turn.
    chat.ask("/stash", "stashed").await;
    chat.wait_for("message_in from ext:sender").await;
    let reply = chat.ask("hello", "got: ").await;
    assert!(reply.contains("remember the milk | ") && reply.ends_with("hello"), "{reply}");
    assert_eq!(core.requests().len(), 1);

    // A message_in hook can hold a user's message for the next turn too.
    chat.ask("later: buy bread", "held for later").await;
    chat.ask("and now", "buy bread").await;
    assert_eq!(core.requests().len(), 2);

    // Another source reaches message_in as it is.
    chat.ask("/report", "reported").await;
    chat.wait_for("message_in from subagent").await;
    chat.wait_for("got: all done").await;
    chat.wait_for("message_in from user").await;
}

#[tokio::test]
async fn model_select_can_redirect_or_refuse_a_model_switch() {
    let core = core()
        .ext("models", |a| {
            a.on("model_select", |d, _| async move {
                Ok(match d["model"].as_str() {
                    Some("banned") => Some(json!({"block": format!("not after {}", d["previous"].as_str().unwrap())})),
                    Some("fast") => Some(json!({"model": "fast-model-v2"})),
                    _ => None,
                })
            });
        })
        .start()
        .await;
    let refused = core.call("model_set", json!({"model": "banned"})).await.unwrap_err().to_string();
    assert!(refused.contains("blocked by an extension: not after fake-model"), "{refused}");
    let set = core.call("model_set", json!({"model": "fast"})).await.unwrap();
    assert_eq!(set["model"], "fast-model-v2");
    core.chat("1").ask("hi", "ok").await;
    assert_eq!(core.requests().last().unwrap().model, "fast-model-v2");
}

#[tokio::test]
async fn turn_event_streams_what_a_visible_turn_does_in_order() {
    let seen: Arc<Mutex<Vec<String>>> = Arc::default();
    let log = seen.clone();
    let core = core()
        .model(|req| match tool_result(req) {
            Some(_) => text("done"),
            None => tool("read", json!({"path": "note.txt"})),
        })
        .seed("note.txt", b"hello")
        .ext("files", files)
        .ext("events", move |a| {
            let log = log.clone();
            a.on("turn_event", move |d, _| {
                let entry = match d["kind"].as_str().unwrap() {
                    "text" => format!("text:{}", d["text"].as_str().unwrap()),
                    "tool" => format!("tool:{}", d["tool"].as_str().unwrap()),
                    other => other.to_string(),
                };
                log.lock().unwrap().push(entry);
                async { Ok(None) }
            });
        })
        .start()
        .await;
    core.chat("1").ask("read the note", "done").await;
    // Events reach the hook in the background.
    core.wait_until("the last event", |_| seen.lock().unwrap().iter().any(|e| e == "text:done")).await;
    assert_eq!(*seen.lock().unwrap(), ["tool:read", "step", "text:done"]);
}

/// `meter` declares three events; others only share the declared contract.
fn meter(a: &august_ext::August) {
    a.define_event(
        "status",
        "Context size before a turn; handlers may add a note or block",
        json!({"type": "object", "required": ["tokens"], "properties": {"tokens": {"type": "integer"}}}),
        false,
    );
    a.define_event("done", "A measurement finished", serde_json::Value::Null, true);
    a.define_event("ping", "Re-emits itself", serde_json::Value::Null, false);
    a.on("meter:ping", |d, ctx| async move {
        let n = d["n"].as_i64().unwrap();
        Ok(Some(match ctx.emit("ping", json!({"n": n + 1})).await {
            Ok(v) => v,
            Err(e) => json!({"n": n, "error": e.to_string()}),
        }))
    });
    a.register_command("meter", "", |_, ctx| async move {
        let r = ctx.emit("status", json!({"tokens": 5})).await?;
        ctx.emit("done", json!({"tokens": r["tokens"]})).await?;
        Ok(Some(match r["block"].as_str() {
            Some(b) => format!("blocked: {b}"),
            None => format!("note: {}", r["note"].as_str().unwrap_or("")),
        }))
    });
    let attempt = |r: anyhow::Result<serde_json::Value>| Ok(Some(r.map(|v| v.to_string()).unwrap_or_else(|e| format!("error: {e}"))));
    a.register_command("bad", "", move |_, ctx| async move { attempt(ctx.emit("status", json!({"tokens": "many"})).await) });
    a.register_command("undeclared", "", move |_, ctx| async move { attempt(ctx.emit("nope", json!({})).await) });
    a.register_command("loop", "", move |_, ctx| async move { attempt(ctx.emit("ping", json!({"n": 0})).await) });
}

fn editor(a: &august_ext::August) {
    a.on("meter:status", |d, _| async move {
        let tokens = d["tokens"].as_i64().unwrap_or(0);
        Ok(Some(if tokens > 100 { json!({"block": "too big"}) } else { json!({"note": format!("{tokens} tokens seen")}) }))
    });
    let me = a.clone();
    a.register_command("fake", "", move |_, _| {
        let me = me.clone();
        async move {
            Ok(Some(match me.emit("meter:status", json!({"tokens": 1})).await {
                Ok(_) => "emitted".into(),
                Err(e) => format!("error: {e}"),
            }))
        }
    });
}

#[tokio::test]
async fn extensions_declare_events_others_hook() {
    let core = core()
        .ext("meter", meter)
        .ext("editor", editor)
        .ext("watcher", |a| {
            a.on("meter:done", |d, ctx| async move {
                ctx.send(&format!("watched done: {}", d["tokens"])).await?;
                Ok(None)
            });
        })
        .start()
        .await;
    let chat = core.chat("1");

    // A chain hands back what its handlers changed; an observed event reaches its watchers.
    chat.ask("/meter", "note: 5 tokens seen").await;
    chat.wait_for("watched done: 5").await;

    // The contract holds: the declared schema, declared names only, own namespace only.
    chat.ask("/bad", "error: `meter:status`: `tokens` must be integer").await;
    chat.ask("/undeclared", "error: declare `nope` first").await;
    chat.ask("/fake", "error: events `meter:*` belong to meter").await;

    // Handlers emitting each other end at the depth limit instead of hanging.
    let looped = chat.ask("/loop", "nest deeper than 8").await;
    assert!(looped.contains(r#""n":7"#), "{looped}");

    // Who emits what is visible.
    let events: Vec<String> = core.extension("meter").await["events"].as_array().unwrap().iter().map(|e| e["name"].as_str().unwrap().to_string()).collect();
    assert_eq!(events, ["meter:status", "meter:done", "meter:ping"]);
}

#[tokio::test]
async fn a_stand_in_takes_over_an_extensions_events() {
    let core = core()
        .ext("meter", meter)
        .ext("editor", editor)
        .ext("meter2", |a| {
            a.replaces(&["meter"]);
            a.define_event("status", "Context size before a turn", serde_json::Value::Null, false);
            a.register_command("meter2", "", |_, ctx| async move {
                let r = ctx.emit("meter:status", json!({"tokens": 7})).await?;
                Ok(Some(format!("note: {}", r["note"].as_str().unwrap_or(""))))
            });
        })
        .start()
        .await;
    let chat = core.chat("1");
    chat.ask("/meter2", "note: 7 tokens seen").await;
    chat.ask("/meter", "belong to meter2").await;
}

#[tokio::test]
async fn llm_error_handlers_retry_a_failed_call_on_another_model_or_let_it_fail() {
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let n = calls.clone();
    let seen: Arc<Mutex<Vec<String>>> = Arc::default();
    let log = seen.clone();
    let core = core()
        .model(move |req| match (n.fetch_add(1, Ordering::SeqCst), last_user_text(req).contains("auth")) {
            (_, true) => fail(august_ext::llm::error::ErrorKind::Auth, "bad key"),
            (0, _) => fail(august_ext::llm::error::ErrorKind::Overloaded, "busy"),
            _ => text(&format!("answered by {}", req.model)),
        })
        .ext("retrier", move |a| {
            let log = log.clone();
            a.on("llm_error", move |d, _| {
                log.lock().unwrap().push(format!("{} {} {}", d["error"]["kind"].as_str().unwrap(), d["attempt"], d["model"].as_str().unwrap()));
                let transient = d["error"]["kind"] == "overloaded";
                async move { Ok(Some(if transient { json!({"retry": true, "delayMs": 1, "model": "backup-model"}) } else { json!({}) })) }
            });
        })
        .start()
        .await;
    let chat = core.chat("1");
    chat.ask("hi", "answered by backup-model").await;
    // A failure the handler doesn't retry ends the turn and says why.
    let failed = chat.ask("auth please", "bad key").await;
    assert!(!failed.contains("answered"), "{failed}");
    // The switch lasted only for the turn that failed.
    assert_eq!(*seen.lock().unwrap(), ["overloaded 1 fake-model", "auth 1 fake-model"]);
}

#[tokio::test]
async fn llm_result_counts_every_model_call_in_its_conversation() {
    let seen: Arc<Mutex<Vec<serde_json::Value>>> = Arc::default();
    let log = seen.clone();
    let core = core()
        .model(|_| text("ok").usage(100, 20, 30))
        .ext("meter", move |a| {
            a.needs(&["llm"]);
            let log = log.clone();
            a.on("llm_result", move |d, _| {
                log.lock().unwrap().push(d);
                async { Ok(None) }
            });
            a.register_command("ask", "", |_, ctx| async move { Ok(Some(format!("llm: {}", ctx.llm("hi", None).await?))) });
        })
        .start()
        .await;
    let chat = core.chat("1");
    chat.ask("hello", "ok").await;
    chat.ask("/ask", "llm: ok").await;
    core.wait_until("both results", |_| seen.lock().unwrap().len() == 2).await;
    let seen = seen.lock().unwrap().clone();
    // The turn's call has its step, the single call none; both count in the chat's session.
    assert_eq!((seen[0]["step"].clone(), seen[1]["step"].clone()), (json!(0), serde_json::Value::Null));
    assert!(seen[0]["session"].is_string() && seen[0]["session"] == seen[1]["session"], "{seen:?}");
    assert_eq!((seen[0]["usage"]["inputTokens"].as_u64(), seen[0]["usage"]["cacheReadTokens"].as_u64()), (Some(100), Some(30)));
}
