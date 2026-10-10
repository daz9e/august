//! Extensions as the core sees them: what they register reaches the agent and the user,
//! they only get what they declare, the user turns them on and off, and they drive the core
//! through its operations.

use crate::support::*;
use august_ext::August;
use serde_json::{Value, json};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// After a tool ran, answers with its output; otherwise `pick` chooses what to do.
fn agent(pick: impl Fn(&str) -> Reply + Send + Sync + 'static) -> impl Fn(&august_ext::llm::Request) -> Reply + Send + Sync + 'static {
    move |req| match tool_result(req) {
        Some(out) => text(&format!("Result: {out}")),
        None => pick(&last_user_text(req)),
    }
}

fn demo(a: &August) {
    a.describe("Shouts text and guards bash", "`shout` uppercases text; blocks bash commands with `forbidden`.");
    a.register_tool("shout", "Uppercase some text", json!({"type": "object", "properties": {"text": {"type": "string"}}}), |input, ctx| async move {
        Ok(format!("{} via {}", input["text"].as_str().unwrap().to_uppercase(), ctx.thread.unwrap().messenger))
    });
    a.register_tool("bash", "Run a command", json!({"type": "object"}), |_, _| async { Ok("ran".into()) });
    a.on("message_in", |d, _| async move {
        let text = d["text"].as_str().unwrap_or("");
        Ok(Some(if text == "secret" { json!({"handled": true, "reply": "intercepted"}) } else { json!({"text": text.replace("colour", "color")}) }))
    });
    a.on("before_turn", |d, _| async move { Ok(Some(json!({"system": format!("{}\nEXTENSION-CONTEXT", d["system"].as_str().unwrap_or(""))}))) });
    a.on("tool_call", |d, _| async move {
        let forbidden = d["tool"] == "bash" && d["input"]["command"].as_str().unwrap_or("").contains("forbidden");
        Ok(forbidden.then(|| json!({"block": "no forbidden commands"})))
    });
    a.on("turn_end", |d, ctx| async move {
        ctx.send(&format!("turn ended: {}", d["reply"].as_str().unwrap_or(""))).await?;
        Ok(None)
    });
    a.register_command("ping", "Replies pong", |args, _| async move { Ok(Some(format!("pong {args}"))) });
}

#[tokio::test]
async fn extensions_add_tools_commands_and_hooks() {
    let core = core()
        .model(agent(|t| if t.contains("forbidden") { tool("bash", json!({"command": "echo forbidden"})) } else { tool("shout", json!({"text": "hi"})) }))
        .ext("demo", demo)
        .sh("broken", "echo boom at load >&2; exit 1")
        .start()
        .await;
    let chat = core.chat("1");

    // One at a time: a message sent while a turn runs would join that turn.
    chat.ask("hello colour", "turn ended: Result: HI via test").await;
    chat.ask("run the forbidden thing", "turn ended: Result: blocked by an extension: no forbidden commands").await;
    chat.ask("secret", "intercepted").await;
    chat.ask("/ping x", "pong x").await;

    // The extension's tool was offered and the before_turn hook extended the prompt.
    let reqs = core.requests();
    let first = reqs.iter().find(|r| last_user_text(r).contains("hello")).unwrap();
    assert!(tool_names(first).contains(&"shout".to_string()), "{:?}", tool_names(first));
    assert!(first.system.contains("EXTENSION-CONTEXT"), "{}", first.system);
    // message_in rewrote one message and swallowed another before the model saw them.
    assert_eq!(last_user_text(first), "hello color");
    assert!(reqs.iter().all(|r| !last_user_text(r).contains("secret")));

    // The extensions list has the working one and the error of the broken one.
    let demo = core.extension("demo").await;
    assert_eq!((demo["state"].as_str(), demo["tools"].clone(), demo["commands"].clone()), (Some("running"), json!(["shout", "bash"]), json!(["ping"])));
    let broken = core.extension("broken").await;
    assert_eq!(broken["state"], "failed");
    assert!(broken["error"].as_str().unwrap().contains("boom at load"), "{broken}");
    // The command reaches the messenger's menu.
    assert!(core.messenger.menus().last().unwrap().contains(&"ping".to_string()));
}

#[tokio::test]
async fn an_extension_installed_by_an_extension_is_the_agents_and_runs_at_once() {
    let greeter = sh_extension("greeter", &ready_script(json!({"summary": "Greets", "tools": [{"name": "greet", "description": "Greets", "parameters": {"type": "object"}}]})));
    let core = core().model(|_| text("ok")).start().await;
    let (path, text_) = greeter;
    let file = core.path(&path);
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    std::fs::write(file, text_).unwrap();

    // An extension (say, the agent's `save_extension`) starts it: the core marks it as the
    // agent's, and its tool is offered on the next model call.
    let line = core.call("extension_enable", json!({"name": "greeter"})).await.unwrap();
    assert!(line.as_str().unwrap().contains("greeter — tools: greet"), "{line}");
    let unit = std::fs::read_to_string(core.path("config/extensions/greeter.json")).unwrap();
    assert!(unit.contains(r#""origin": "agent""#), "{unit}");
    core.chat("1").ask("hi", "ok").await;
    assert!(tool_names(&core.requests()[0]).contains(&"greet".to_string()));
}

#[tokio::test(start_paused = true)]
async fn crashed_extension_is_restarted() {
    let core = core()
        .model(agent(|_| tool("alive", json!({}))))
        .ext("fragile", |a| a.register_tool("alive", "Answers if running", json!({"type": "object"}), |_, _| async { Ok("still here".into()) }))
        .start()
        .await;
    core.crash("fragile");
    let mut state = Value::Null;
    for _ in 0..100 {
        state = core.extension("fragile").await;
        if state["state"] == "running" && state["restarts"] == 1 {
            break;
        }
        settle(100).await;
    }
    assert_eq!((state["state"].as_str(), state["restarts"].as_u64()), (Some("running"), Some(1)), "{state}");
    // The gateway kept serving, and the restarted extension answers.
    core.chat("1").ask("are you there?", "Result: still here").await;
}

#[tokio::test]
async fn extensions_hook_model_calls_call_into_august_and_can_be_disabled() {
    let core = core()
        .model(agent(|t| if t.contains("read the note") { tool("read", json!({"path": "note.txt"})) } else { text("forty-two") }))
        .seed("note.txt", b"note body")
        .ext("files", files)
        .ext("probe2", |a| {
            a.needs(&["tools", "llm"]);
            a.on("llm_call", |d, _| async move { Ok((d["step"] == 1).then(|| json!({"system": format!("{}\nSTEP-ONE", d["system"].as_str().unwrap())}))) });
            a.on("llm_result", |d, ctx| async move {
                let calls: Vec<&str> = d["toolCalls"].as_array().unwrap().iter().filter_map(|c| c["name"].as_str()).collect();
                let tokens = if d["usage"]["inputTokens"].is_number() { "number" } else { "?" };
                ctx.send(&format!("llm_result {}: {}|{}|{tokens}", d["step"], calls.join(","), d["text"].as_str().unwrap_or(""))).await?;
                Ok(None)
            });
            a.register_command("peek", "", |args, ctx| async move {
                let (out, err) = ctx.call_tool("read", json!({"path": "note.txt"})).await?;
                Ok(Some(format!("peek {args}: {out} (error: {err})")))
            });
            a.register_command("ask", "", |args, ctx| async move { Ok(Some(format!("llm says: {}", ctx.llm(&args, Some("SYS-X")).await?))) });
        })
        .start()
        .await;
    let chat = core.chat("1");
    chat.say("read the note");

    // llm_result fires after every model call of the turn.
    chat.wait_for("llm_result 0: read||number").await;
    chat.wait_for("llm_result 1: |Result: note body").await;
    // llm_call changed the system prompt of the second call only.
    let reqs = core.requests();
    assert!(reqs[1].system.ends_with("\nSTEP-ONE"));
    assert_eq!(reqs.iter().filter(|r| r.system.contains("STEP-ONE")).count(), 1);

    // callTool runs a tool in the chat; llm asks the model without tools.
    chat.ask("/peek one", "peek one: note body (error: false)").await;
    chat.ask("/ask what is six times seven", "llm says: forty-two").await;
    let ask = core.requests().into_iter().find(|r| last_user_text(r).contains("six times seven")).unwrap();
    assert_eq!(ask.system, "SYS-X");
    assert!(ask.tools.is_empty());

    // Disabled: listed as such, not running, remembered on disk; enabling restores it.
    let settings = || std::fs::read_to_string(core.path("config/extensions/probe2.json")).unwrap_or_default();
    core.call("extension_disable", json!({"name": "probe2"})).await.unwrap();
    assert_eq!(core.extension("probe2").await["state"], "disabled");
    assert!(settings().contains(r#""enabled": false"#), "{}", settings());
    chat.ask("/peek two", "Unknown command /peek.").await;
    core.call("extension_enable", json!({"name": "probe2"})).await.unwrap();
    assert!(!settings().contains("enabled"), "{}", settings());
    chat.ask("/peek three", "peek three: note body").await;
}

#[tokio::test]
async fn context_hook_changes_what_the_model_sees_but_not_the_history() {
    let core = core()
        .model(|req| text(&format!("seen {}", req.messages.len())))
        .ext("recall", |a| {
            a.on("context", |d, _| async move {
                let mut messages = vec![json!({"role": "user", "content": [{"type": "text", "text": "RECALLED: the user likes tea"}]})];
                messages.extend(d["messages"].as_array().unwrap().iter().cloned());
                Ok(Some(json!({"messages": messages})))
            });
        })
        .start()
        .await;
    let chat = core.chat("1");
    chat.ask("hi", "seen 2").await;
    chat.ask("again", "seen 4").await;
    for req in core.requests() {
        let recalled = req.messages.iter().filter(|m| m.text().contains("RECALLED")).count();
        assert_eq!(recalled, 1);
    }
}

#[tokio::test]
async fn the_users_extensions_replace_default_tools() {
    let edit = ready_script(json!({"tools": [{"name": "edit", "description": "Edit a file", "parameters": {"type": "object"}}]}));
    let core = core()
        .model(agent(|_| tool("edit", json!({"path": "notes.txt"}))))
        .default_ext("tools", &edit)
        .ext("replacer", |a| a.register_tool("edit", "Edit a file", json!({"type": "object"}), |_, _| async { Ok("custom edit".into()) }))
        .start()
        .await;
    core.chat("1").ask("edit", "Result: custom edit").await;
    let tools = core.call("tools", json!({})).await.unwrap();
    let edit = tools.as_array().unwrap().iter().find(|t| t["name"] == "edit").unwrap().clone();
    assert_eq!(edit["owner"], "replacer");
    assert_eq!(core.extension("tools").await["origin"], "default");
}

#[tokio::test]
async fn extension_changes_its_tools_after_startup() {
    let core = core()
        .model(agent(|_| tool("late", json!({}))))
        .ext("late", |a| {
            a.register_tool("early", "Gone after /arm", json!({"type": "object"}), |_, _| async { Ok("early ran".into()) });
            let me = a.clone();
            a.register_command("arm", "", move |_, _| {
                me.unregister_tool("early");
                me.register_tool("late", "Added by /arm", json!({"type": "object"}), |_, _| async { Ok("late ran".into()) });
                async { Ok(Some("armed".into())) }
            });
        })
        .start()
        .await;
    let chat = core.chat("1");
    chat.ask("/arm", "armed").await;
    chat.ask("go", "Result: late ran").await;
    let tools = tool_names(&core.requests()[0]);
    assert!(tools.contains(&"late".to_string()) && !tools.contains(&"early".to_string()), "{tools:?}");
}

#[tokio::test]
async fn extensions_add_prompt_sections_fixed_per_conversation() {
    let core = core()
        .ext("hints", |a| {
            a.register_prompt_section("hints", "## Hints\nHINT-ONE");
            let me = a.clone();
            a.register_command("hint", "", move |text, _| {
                me.register_prompt_section("hints", &format!("## Hints\n{text}"));
                async { Ok(Some("hinted".into())) }
            });
        })
        .start()
        .await;
    let chat = core.chat("1");
    let system = |i: usize| core.requests()[i].system.clone();

    chat.ask("first", "ok").await;
    assert!(system(0).contains("## Hints\nHINT-ONE"), "{}", system(0));
    // A changed section waits for the next conversation, so the prompt stays cacheable.
    chat.ask("/hint HINT-TWO", "hinted").await;
    chat.ask("second", "ok").await;
    assert_eq!(system(1), system(0));
    core.call_in("1", "session_new", json!({})).await.unwrap();
    chat.ask("third", "ok").await;
    assert!(system(2).contains("HINT-TWO") && !system(2).contains("HINT-ONE"), "{}", system(2));
}

#[tokio::test]
async fn extensions_keep_state_in_the_store_across_reloads() {
    let core = core()
        .ext("counter", |a| {
            let me = a.clone();
            a.register_command("count", "", move |_, _| {
                let me = me.clone();
                async move {
                    let n = me.get("count").await?.and_then(|v| v.as_u64()).unwrap_or(0) + 1;
                    me.set("count", json!(n)).await?;
                    me.set(&format!("seen:{n}"), json!({"n": n})).await?;
                    Ok(Some(format!("count {n}, seen {}", me.list("seen:").await?.len())))
                }
            });
        })
        .start()
        .await;
    let chat = core.chat("1");
    chat.ask("/count", "count 1, seen 1").await;
    core.call("extensions_reload", json!({})).await.unwrap();
    chat.ask("/count", "count 2, seen 2").await;
}

#[tokio::test]
async fn extensions_only_get_what_they_declare() {
    let core = core()
        .ext("nosy", |a| {
            let me = a.clone();
            a.register_command("peek", "", move |_, _| {
                let me = me.clone();
                async move { Ok(Some(format!("{} messengers", me.messengers().await?))) }
            });
            a.register_command("here", "", |_, ctx| async move {
                ctx.send("answering here is fine").await?;
                Ok(None)
            });
            a.register_command("act", "", |_, ctx| async move {
                ctx.call("config_set", json!({"path": "august.model", "value": "x", "as_user": true})).await?;
                Ok(Some("acted".into()))
            });
        })
        .start()
        .await;
    let chat = core.chat("1");
    let refused = chat.ask("/peek", "failed").await;
    assert!(refused.contains("needs the `messaging` permission"), "{refused}");
    // Answering in the thread of the call in progress needs nothing.
    chat.ask("/here", "answering here is fine").await;
    // Acting for the user needs `user`.
    let refused = chat.ask("/act", "failed").await;
    assert!(refused.contains("needs the `user` permission"), "{refused}");
}

#[tokio::test]
async fn extensions_hear_cancels_and_shutdowns_and_set_hook_timeouts() {
    let (aborted, shut) = (Arc::new(AtomicBool::new(false)), Arc::new(AtomicBool::new(false)));
    let (a1, s1) = (aborted.clone(), shut.clone());
    let core = core()
        .model(|req| match (last_user_text(req), tool_result(req)) {
            (t, None) if t.contains("run slow") => tool("slow", json!({})),
            (t, _) => text(&format!("got: {t}")),
        })
        .ext("life", move |a| {
            // A long tool that stops when August stops waiting for it.
            let aborted = a1.clone();
            a.register_tool("slow", "Takes long", json!({"type": "object"}), move |_, _| {
                let guard = SetOnDrop(aborted.clone());
                async move {
                    tokio::time::sleep(Duration::from_secs(60)).await;
                    drop(guard);
                    Ok("late".into())
                }
            });
            // A hook slower than its own timeout is skipped; the turn goes on without it.
            a.hook_timeout("before_turn", Duration::from_millis(300));
            a.on("before_turn", |d, _| async move {
                let text = d["text"].as_str().unwrap_or("").to_string();
                if text.contains("hang") {
                    tokio::time::sleep(Duration::from_secs(5)).await;
                }
                Ok(Some(json!({"text": format!("{text} (seen by the hook)")})))
            });
            let shut = s1.clone();
            a.on("shutdown", move |_, _| {
                shut.store(true, Ordering::SeqCst);
                async { Ok(None) }
            });
        })
        .start()
        .await;
    let chat = core.chat("1");

    // /stop while the extension's tool runs: the extension is told to stop.
    chat.say("run slow");
    core.wait_until("the tool to run", |c| c.requests().len() == 1).await;
    settle(50).await;
    chat.ask("/stop", "Stopped 1 turn(s).").await;
    core.wait_until("the tool aborted", |_| aborted.load(Ordering::SeqCst)).await;

    // Its own timeout: the hanging hook is skipped, the quick one applies.
    let hung = chat.ask("hang please", "got: hang please").await;
    assert!(!hung.contains("seen by the hook"), "{hung}");
    chat.ask("quick", "got: quick (seen by the hook)").await;

    // A reload tells it to clean up first.
    core.call("extensions_reload", json!({})).await.unwrap();
    assert!(shut.load(Ordering::SeqCst));
}

/// Sets its flag when dropped before it is defused (a cancelled future).
struct SetOnDrop(Arc<AtomicBool>);

impl Drop for SetOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

#[tokio::test]
async fn extensions_drive_the_core_through_its_operations() {
    let core = core()
        .ext("files", files)
        .ext("control", |a| {
            a.needs(&["admin", "models"]);
            a.register_tool("dial", "A dial", json!({"type": "object"}), |_, _| async { Ok("turned".into()) });
            let me = a.clone();
            a.register_command("owners", "", move |_, _| {
                let me = me.clone();
                async move {
                    let tools = me.call("tools", json!({})).await?;
                    let owner = |name: &str| tools.as_array().unwrap().iter().find(|t| t["name"] == name).map(|t| t["owner"].as_str().unwrap().to_string());
                    let ops = me.call("ops", json!({})).await?;
                    let need = ops.as_array().unwrap().iter().find(|o| o["name"] == "model_set").unwrap()["permission"].clone();
                    Ok(Some(format!("dial: {}, read: {}, model_set needs {}", owner("dial").unwrap(), owner("read").unwrap(), need.as_str().unwrap())))
                }
            });
            let me = a.clone();
            a.register_command("swap", "", move |model, _| {
                let me = me.clone();
                async move { Ok(Some(format!("swapped to {}", me.call("model_set", json!({"model": model})).await?["model"].as_str().unwrap()))) }
            });
            let me = a.clone();
            a.register_command("off", "", move |name, _| {
                let me = me.clone();
                async move {
                    me.call("extension_disable", json!({"name": name})).await?;
                    let list = me.call("extensions", json!({})).await?;
                    let state = list.as_array().unwrap().iter().find(|e| e["name"] == name.as_str()).unwrap()["state"].clone();
                    Ok(Some(format!("{name} is {}", state.as_str().unwrap())))
                }
            });
        })
        .ext("meek", |a| {
            let me = a.clone();
            a.register_command("coup", "", move |_, _| {
                let me = me.clone();
                async move {
                    me.call("extension_disable", json!({"name": "control"})).await?;
                    Ok(Some("done".into()))
                }
            });
        })
        .start()
        .await;
    let chat = core.chat("1");

    // The tables of tools and operations, with who offers what and what it needs.
    chat.ask("/owners", "dial: control, read: files, model_set needs models").await;

    // Switching the model: the next call uses it, and it is what `status` says.
    chat.ask("/swap other-model", "swapped to other-model").await;
    chat.ask("hi", "ok").await;
    assert_eq!(core.requests().last().unwrap().model, "other-model");
    assert_eq!(core.call("status", json!({})).await.unwrap()["model"], "other-model");

    // Without `admin`, an extension can't touch others; with it, it can.
    let refused = chat.ask("/coup", "failed").await;
    assert!(refused.contains("needs the `admin` permission"), "{refused}");
    chat.ask("/off meek", "meek is disabled").await;
}

#[tokio::test]
async fn extensions_have_settings_the_user_sets_and_secrets_stay_hidden() {
    let core = core()
        .ext("weather", |a| {
            a.needs(&["messaging"]);
            a.settings_schema(json!({"type": "object", "properties": {"city": {"type": "string", "default": "Berlin"}, "api_key": {"type": "string", "secret": true}}}));
            let me = a.clone();
            a.register_command("weather", "", move |_, _| {
                let me = me.clone();
                async move {
                    let s = me.settings().await?;
                    Ok(Some(format!("weather for {} with {}", s["city"].as_str().unwrap(), s["api_key"].as_str().unwrap_or("no key"))))
                }
            });
            let me = a.clone();
            a.on("config_changed", move |d, _| {
                let me = me.clone();
                async move {
                    let path = d["path"].as_str().unwrap_or("").to_string();
                    if path.starts_with("extensions.weather") {
                        me.call("send", json!({"thread": "home", "message": format!("noticed {path}")})).await?;
                    }
                    Ok(None)
                }
            });
        })
        .ext("snoop", |a| {
            a.needs(&["config"]);
            let me = a.clone();
            a.register_command("snoop", "", move |_, _| {
                let me = me.clone();
                async move { Ok(Some(me.call("config_get", json!({"path": "extensions.weather.settings"})).await?.to_string())) }
            });
            let me = a.clone();
            a.register_command("silence", "", move |_, _| {
                let me = me.clone();
                async move { Ok(Some(me.call("config_set", json!({"path": "extensions.weather.enabled", "value": false})).await.map(|_| "done".to_string())?)) }
            });
        })
        .start()
        .await;
    let chat = core.chat("1");
    let user = |path: &str, value: Value| core.call("config_set", json!({"path": path, "value": value, "as_user": true}));

    // Defaults from the schema until the user sets something.
    chat.ask("/weather", "weather for Berlin with no key").await;
    user("august.home", json!("test:1")).await.unwrap();

    // The user sets a secret; others see it masked, and the extension hears of the change.
    user("extensions.weather.settings.api_key", json!("abc123")).await.unwrap();
    chat.wait_for("noticed extensions.weather.settings.api_key").await;
    let shown = core.call("config_get", json!({"path": "extensions.weather.settings.api_key"})).await.unwrap();
    assert_eq!(shown, "••••");
    user("extensions.weather.settings.city", json!("Paris")).await.unwrap();
    chat.ask("/weather", "weather for Paris with abc123").await;
    let file = std::fs::read_to_string(core.path("config/extensions/weather.json")).unwrap();
    assert!(file.contains("abc123") && file.contains("Paris"), "{file}");

    // Another extension with `config` reads settings with secrets masked, and can't turn
    // extensions off.
    let seen = chat.ask("/snoop", "Paris").await;
    assert!(seen.contains("••••") && !seen.contains("abc123"), "{seen}");
    chat.ask("/silence", "only the user turns extensions on and off").await;
    chat.ask("/weather", "weather for Paris").await;
}

#[tokio::test(start_paused = true)]
async fn a_monitor_reports_crashed_extensions_to_the_home_thread() {
    let core = core()
        .ext("fragile", |a| a.register_tool("alive", "", json!({"type": "object"}), |_, _| async { Ok("still here".into()) }))
        .ext("monitor", |a| {
            a.needs(&["messaging"]);
            let me = a.clone();
            a.on("extension_state", move |d, _| {
                let me = me.clone();
                async move {
                    if d["name"] == "fragile" {
                        let error = d["error"].as_str().map(|e| format!(" ({})", e.split(':').next().unwrap())).unwrap_or_default();
                        me.call("send", json!({"thread": "home", "message": format!("monitor: fragile is {}{error}", d["state"].as_str().unwrap())})).await?;
                    }
                    Ok(None)
                }
            });
        })
        .start()
        .await;
    core.call("config_set", json!({"path": "august.home", "value": "test:home", "as_user": true})).await.unwrap();
    let home = core.chat("home");
    core.crash("fragile");
    home.wait_for("monitor: fragile is failed (crashed)").await;
    home.wait_for("monitor: fragile is running").await;
}
