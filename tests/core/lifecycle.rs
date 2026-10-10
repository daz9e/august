//! How extensions live: started from `extension.json` in any language, watched by the core
//! (crashes, hangs, their own health), restarted by a policy hooks can change, with a log of
//! their own, and hooks on launching them.

use crate::support::*;
use august_ext::August;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

/// A tiny Python SDK: the extension protocol over stdin/stdout.
const SDK: &str = r#"
import json, os, sys, threading

def write(msg):
    sys.stdout.write(json.dumps(msg) + "\n")
    sys.stdout.flush()

def handle(msg, tools):
    p = msg.get("params", {})
    if msg["method"] == "tool":
        write({"id": msg["id"], "result": tools[p["name"]](p.get("input", {}))})
    elif msg["method"] == "health":
        write({"id": msg["id"], "result": {"status": "ok"}})
    else:
        write({"id": msg["id"], "error": {"message": "unknown method " + msg["method"]}})

def run(summary, tools):
    specs = [{"name": n, "description": n, "parameters": {"type": "object"}} for n in tools]
    write({"method": "ready", "params": {"protocol": 2, "summary": summary, "tools": specs}})
    for line in sys.stdin:
        msg = json.loads(line)
        if "method" in msg and "id" in msg:
            threading.Thread(target=handle, args=(msg, tools), daemon=True).start()
"#;

const WEATHER: &str = r#"
import os, sys
from sdk import run

print("hello from python", file=sys.stderr, flush=True)
run("Weather in Python", {"weather": lambda i: f"{i.get('city')}: sunny, {os.environ.get('GREETING', 'no greeting')}"})
"#;

#[tokio::test]
async fn an_extension_in_any_language_runs_from_extension_json_and_has_its_own_log() {
    let core = core()
        .model(|req| match tool_result(req) {
            Some(out) => text(&format!("Result: {out}")),
            None => tool("weather", json!({"city": "Paris"})),
        })
        .home("extensions/weather/extension.json", &json!({"command": ["python3", "main.py"], "env": {"GREETING": "hi"}}).to_string())
        .home("extensions/weather/main.py", WEATHER)
        .home("extensions/weather/sdk.py", SDK)
        .home("extensions/ghost/extension.json", r#"{"command": ["no-such-program-xyz"]}"#)
        .start()
        .await;

    // The Python extension's tool works, with the environment its extension.json gives it.
    core.chat("1").ask("what's the weather?", "Result: Paris: sunny, hi").await;
    assert_eq!(core.extension("weather").await["summary"], "Weather in Python");

    // A missing program fails only that extension, saying what is missing.
    let ghost = core.extension("ghost").await;
    assert!(ghost["error"].as_str().unwrap().contains("`no-such-program-xyz` was not found"), "{ghost}");

    // Its stderr and the core's notes about it are in its log, kept outside its folder.
    let log = core.call("extension_logs", json!({"name": "weather", "lines": 50})).await.unwrap();
    let log = log.as_str().unwrap();
    assert!(log.contains("hello from python") && log.contains("[august] launching (install)") && log.contains("[august] started, pid"), "{log}");
    assert!(core.path("logs/extensions/weather.log").is_file());
    assert!(!core.path("extensions/weather/weather.log").exists());

    // It answers a health check on demand.
    assert_eq!(core.call("extension_health", json!({"name": "weather"})).await.unwrap()["status"], "ok");
}

fn alive(pid: &str) -> bool {
    std::process::Command::new("kill").args(["-0", pid]).status().is_ok_and(|s| s.success())
}

#[tokio::test]
async fn processes_an_extension_started_die_with_it() {
    let script = format!("sleep 600 & echo $! > child.txt; {}", ready_script(json!({})));
    let core = core().sh("parent", &script).start().await;
    let pid = std::fs::read_to_string(core.path("extensions/parent/child.txt")).unwrap();
    assert!(alive(pid.trim()), "the helper is running");

    // The extension dies on its own: its helper does not outlive it.
    let parent = core.extension("parent").await["pid"].to_string();
    std::process::Command::new("kill").args(["-9", &parent]).status().unwrap();
    for _ in 0..100 {
        if !alive(pid.trim()) {
            return;
        }
        settle(50).await;
    }
    panic!("helper {pid} outlived its extension");
}

/// Fast checks and a short leash.
const SUPERVISE: &str = r#"{"provider": "fake", "model": "fake-model", "supervise": {"ping_interval_ms": 200, "ping_timeout_ms": 300, "ping_misses": 2, "max_restarts": 2}}"#;

/// Reports every state change of `weather` to the test.
fn monitor(log: Arc<Mutex<Vec<String>>>) -> impl Fn(&August) + Send + Sync + 'static {
    move |a| {
        a.needs(&["admin"]);
        let log = log.clone();
        a.on("extension_state", move |d, _| {
            if d["name"] == "weather" {
                log.lock().unwrap().push(format!("{} ({}) restarts={} final={}", d["state"].as_str().unwrap(), d["reason"].as_str().unwrap(), d["restarts"], d["final"]));
            }
            async { Ok(None) }
        });
    }
}

async fn saw(core: &Core, log: &Arc<Mutex<Vec<String>>>, entry: &str) {
    core.wait_until(entry, |_| log.lock().unwrap().iter().any(|e| e.contains(entry))).await;
}

/// `weather` answers its health checks with what `health` holds; `null` never answers.
fn weather(health: Arc<Mutex<Value>>) -> impl Fn(&August) + Send + Sync + 'static {
    move |a| {
        a.register_tool("weather", "", json!({"type": "object"}), |_, _| async { Ok("sunny".into()) });
        let health = health.clone();
        a.health(move || {
            let now = health.lock().unwrap().clone();
            async move {
                if now.is_null() {
                    std::future::pending::<()>().await;
                }
                Ok(now)
            }
        });
    }
}

#[tokio::test(start_paused = true)]
async fn crashed_extensions_restart_until_the_policy_gives_up() {
    let (log, health): (Arc<Mutex<Vec<String>>>, _) = (Arc::default(), Arc::new(Mutex::new(json!({"status": "ok"}))));
    let core = core().home("config/august.json", SUPERVISE).ext("monitor", monitor(log.clone())).ext("weather", weather(health.clone())).start().await;

    // A crash: reported with its reason, then the restart.
    core.crash("weather");
    saw(&core, &log, "failed (exit) restarts=0 final=false").await;
    saw(&core, &log, "running (restart) restarts=1").await;

    // One that answers no health check is stopped as hung, and restarted.
    *health.lock().unwrap() = Value::Null;
    saw(&core, &log, "failed (hang) restarts=1").await;
    *health.lock().unwrap() = json!({"status": "ok"});
    saw(&core, &log, "running (restart) restarts=2").await;
    let notes = core.call("extension_logs", json!({"name": "weather"})).await.unwrap();
    assert!(notes.as_str().unwrap().contains("no reply to 2 health checks"), "{notes}");

    // Past `max_restarts` it stays down, and says so.
    core.crash("weather");
    saw(&core, &log, "failed (exit) restarts=2 final=true").await;
    let state = core.extension("weather").await;
    assert!(state["state"] == "failed" && state["error"].as_str().unwrap().starts_with("crashed"), "{state}");
}

#[tokio::test(start_paused = true)]
async fn extensions_report_their_own_health() {
    let (log, health): (Arc<Mutex<Vec<String>>>, _) = (Arc::default(), Arc::new(Mutex::new(json!({"status": "ok"}))));
    let core = core().home("config/august.json", SUPERVISE).ext("monitor", monitor(log.clone())).ext("weather", weather(health.clone())).start().await;

    // Degraded: shown, not restarted — something outside is wrong.
    *health.lock().unwrap() = json!({"status": "degraded", "detail": "no API key"});
    saw(&core, &log, "degraded (unhealthy)").await;
    let state = core.extension("weather").await;
    assert_eq!((state["state"].as_str(), state["health"]["detail"].as_str()), (Some("degraded"), Some("no API key")));
    *health.lock().unwrap() = json!({"status": "ok"});
    saw(&core, &log, "running (recovered)").await;

    // Failed: broken inside, so it is restarted.
    *health.lock().unwrap() = json!({"status": "failed", "detail": "poller stalled"});
    saw(&core, &log, "failed (unhealthy)").await;
    *health.lock().unwrap() = json!({"status": "ok"});
    saw(&core, &log, "running (restart) restarts=1").await;
}

/// Watches launches: blocks `evil`, changes `weather`'s environment, takes `admin` from it,
/// remembers why each one started and what it wrote, and keeps it down once it crashes.
fn guard(seen: Arc<Mutex<Vec<String>>>) -> impl Fn(&August) + Send + Sync + 'static {
    move |a| {
        a.needs(&["admin"]);
        let s = seen.clone();
        a.on("extension_launch", move |d, _| {
            s.lock().unwrap().push(format!("{} ({}, {} changed)", d["name"].as_str().unwrap(), d["reason"].as_str().unwrap(), d["changed"].as_array().unwrap().len()));
            async move {
                Ok(match d["name"].as_str() {
                    Some("evil") => Some(json!({"block": "not on my watch"})),
                    Some("weather") => {
                        let mut env = d["env"].clone();
                        env["GREETING"] = json!("from the guard");
                        Some(json!({"env": env}))
                    }
                    _ => None,
                })
            }
        });
        a.on("extension_ready", |d, _| async move {
            let needs: Vec<Value> = d["manifest"]["needs"].as_array().unwrap().iter().filter(|n| *n != "admin").cloned().collect();
            Ok((d["name"] == "weather").then(|| json!({"needs": needs})))
        });
        let s = seen.clone();
        a.on("extension_output", move |d, _| {
            if d["name"] == "weather" {
                s.lock().unwrap().push(format!("said {}", d["line"].as_str().unwrap()));
            }
            async { Ok(None) }
        });
        a.on("extension_exit", |d, _| async move { Ok((d["name"] == "weather").then(|| json!({"restart": false}))) });
    }
}

#[tokio::test]
async fn hooks_watch_extensions_launch_and_can_block_change_or_limit_them() {
    let seen: Arc<Mutex<Vec<String>>> = Arc::default();
    let greedy = format!("echo greedy says hi >&2; echo \"$GREETING\" > \"$AUGUST_WORKSPACE/greeting.txt\"; {}", ready_script(json!({"needs": ["admin"]})));
    let mut core = core().ext("guard", guard(seen.clone())).sh("weather", &greedy).sh("evil", "echo should never run > ran.txt").start().await;
    let has = |what: &str| seen.lock().unwrap().iter().any(|e| e.contains(what));

    // The guard ran before the others started: one blocked, one changed and limited.
    let evil = core.extension("evil").await;
    assert_eq!(evil["error"], "launch blocked: not on my watch");
    assert!(!core.path("extensions/evil/ran.txt").exists());
    assert_eq!(std::fs::read_to_string(core.workspace.join("greeting.txt")).unwrap().trim(), "from the guard");
    assert_eq!(core.extension("weather").await["needs"], json!([]));

    // It saw why each one started: new ones, then a plain start, then a change.
    assert!(has("weather (install, 1 changed)"), "{:?}", seen.lock().unwrap());
    core.wait_until("its stderr", |_| has("said greedy says hi")).await;
    core.restart().await;
    core.wait_until("a plain start", |_| has("weather (start, 0 changed)")).await;
    std::fs::write(core.path("extensions/weather/notes.txt"), "new").unwrap();
    core.call("extensions_reload", json!({})).await.unwrap();
    core.wait_until("an update", |_| has("weather (update, 1 changed)")).await;

    // Its exit hook keeps the crashed one down: no restart, however long we wait.
    let pid = core.extension("weather").await["pid"].to_string();
    std::process::Command::new("kill").args(["-9", &pid]).status().unwrap();
    settle(1_500).await;
    let state = core.extension("weather").await;
    assert!(state["state"] == "failed" && state["error"].as_str().unwrap().starts_with("crashed"), "{state}");
}
