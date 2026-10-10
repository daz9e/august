//! How extensions live: started from `extension.json` in any language (Python here), watched
//! by the core (crashes, hangs, their own health), restarted by a policy hooks can change,
//! with a log of their own, and hooks on launching them. Skipped without python3.

use crate::support::*;
use serde_json::{Value, json};

fn have_python() -> bool {
    let found = std::env::var_os("PATH").is_some_and(|p| std::env::split_paths(&p).any(|d| d.join("python3").is_file()));
    if !found {
        eprintln!("skipping: python3 is not installed");
    }
    found
}

/// A tiny Python SDK: the extension protocol over stdin/stdout, calls into August included.
const SDK: &str = r#"
import json, sys, threading

class August:
    def __init__(self, summary):
        self.summary, self.tools, self.commands, self.hooks, self.needs, self.health = summary, {}, {}, {}, [], None
        self.lock, self.next, self.pending = threading.Lock(), 1_000_000, {}

    def write(self, msg):
        with self.lock:
            sys.stdout.write(json.dumps(msg) + "\n")
            sys.stdout.flush()

    def call(self, method, params):
        with self.lock:
            self.next += 1
            i = self.next
        done = threading.Event()
        self.pending[i] = [done, None]
        self.write({"id": i, "method": method, "params": params})
        done.wait(30)
        return self.pending.pop(i)[1]

    def on(self, event, handler):
        self.hooks.setdefault(event, []).append(handler)

    def handle(self, msg):
        m, p = msg["method"], msg.get("params", {})
        try:
            if m == "tool":
                r = self.tools[p["name"]](p.get("input", {}))
            elif m == "command":
                r = self.commands[p["name"]](p.get("args", ""))
            elif m == "event":
                r = p.get("data", {})
                for h in self.hooks.get(p["name"], []):
                    r.update(h(r) or {})
            elif m == "health" and self.health:
                r = self.health()
            else:
                raise Exception("unknown method " + m)
            self.write({"id": msg["id"], "result": r})
        except Exception as e:
            self.write({"id": msg["id"], "error": {"message": str(e)}})

    def run(self):
        tools = [{"name": n, "description": n, "parameters": {"type": "object", "properties": {"city": {"type": "string"}}}} for n in self.tools]
        commands = [{"name": n, "description": n} for n in self.commands]
        self.write({"method": "ready", "params": {"protocol": 2, "summary": self.summary, "tools": tools,
                    "commands": commands, "events": list(self.hooks), "needs": self.needs}})
        for line in sys.stdin:
            msg = json.loads(line)
            if msg.get("method") == "cancel":
                continue
            if "method" in msg:
                threading.Thread(target=self.handle, args=(msg,), daemon=True).start()
            elif msg.get("id") in self.pending:
                slot = self.pending[msg["id"]]
                slot[1] = msg.get("result", msg.get("error"))
                slot[0].set()
"#;

/// `weather`: a tool, commands that crash or freeze it, and a health check read from
/// `health.txt` in its folder (`status|detail`).
const WEATHER: &str = r#"
import os, signal, sys
from sdk import August

print("hello from python", file=sys.stderr, flush=True)
a = August("Weather in Python")
a.tools["weather"] = lambda i: f"{i.get('city')}: sunny, {os.environ.get('GREETING', 'no greeting')}"
a.commands["die"] = lambda _: os._exit(3)
a.commands["freeze"] = lambda _: os.kill(os.getpid(), signal.SIGSTOP)

def health():
    try:
        status, detail = open("health.txt").read().strip().split("|")
        return {"status": status, "detail": detail}
    except FileNotFoundError:
        return {"status": "ok"}
a.health = health
a.run()
"#;

/// Reports every state change of `weather` home, and offers `/logs` and `/health`.
const MONITOR: &str = r#"
from sdk import August

a = August("Watches weather")
a.needs = ["messaging", "admin"]
def state(d):
    if d["name"] == "weather":
        a.call("send", {"thread": "home", "message": f"monitor: {d['state']} ({d['reason']}) restarts={d['restarts']} final={d['final']}"})
a.on("extension_state", state)
a.commands["logs"] = lambda name: a.call("extension_logs", {"name": name, "lines": 50})
a.commands["health"] = lambda name: str(a.call("extension_health", {"name": name}))
a.run()
"#;

fn ext(cmd: &str) -> String {
    json!({"command": ["python3", cmd], "env": {"GREETING": "hi"}}).to_string()
}

/// Calls `weather` when asked about the weather; answers with what the tool returned.
fn llm() -> Llm {
    Box::new(|req: &Value| {
        let last = req["messages"].as_array().unwrap().last().unwrap();
        if last["role"] == "tool" {
            reply_text(&format!("Result: {}", last["content"].as_str().unwrap_or("")))
        } else {
            reply_tool("weather", json!({"city": "Paris"}))
        }
    })
}

/// Fast checks and a short leash, so the tests don't wait.
const SUPERVISE: &str = r#"{"supervise": {"ping_interval_ms": 200, "ping_timeout_ms": 300, "ping_misses": 2, "max_restarts": 2}}"#;
const EARLY: &str = r#"{"early": true}"#;

#[tokio::test]
async fn an_extension_in_any_language_runs_from_extension_json_and_has_its_own_log() {
    if !have_python() {
        return;
    }
    let fake = Fake::llm(llm()).await;
    let weather_json = ext("main.py");
    let ghost = r#"{"command": ["no-such-program-xyz"]}"#;
    let home = [
        ("extensions/weather/extension.json", weather_json.as_str()),
        ("extensions/weather/main.py", WEATHER),
        ("extensions/weather/sdk.py", SDK),
        ("extensions/ghost/extension.json", ghost),
        ("extensions/monitor/extension.json", r#"{"command": ["python3", "main.py"]}"#),
        ("extensions/monitor/main.py", MONITOR),
        ("extensions/monitor/sdk.py", SDK),
    ];
    let gw = august(&fake, Setup { home: &home, ..Default::default() }).await;
    let mut chat = gw.chat().await;

    // The Python extension's tool works, with the environment its extension.json gives it.
    chat.ask("what's the weather?", "Result: Paris: sunny, hi").await;

    // A missing program fails only that extension, saying what is missing.
    let status = chat.ask("/extensions", "ghost").await;
    assert!(status.contains("`no-such-program-xyz` was not found"), "{status}");
    assert!(status.contains("✅ weather — tools: weather"), "{status}");

    // Its stderr and the core's notes about it are in its log, kept outside its folder.
    let log = chat.ask("/logs weather", "hello from python").await;
    assert!(log.contains("[august] launching (install)") && log.contains("[august] started, pid"), "{log}");
    assert!(!gw.home.join("extensions/weather/weather.log").exists());
    assert!(gw.home.join("logs/extensions/weather.log").is_file());

    // It answers a health check on demand.
    chat.ask("/health weather", "'status': 'ok'").await;
}

#[tokio::test]
async fn crashed_extensions_restart_until_the_policy_gives_up() {
    if !have_python() {
        return;
    }
    let fake = Fake::llm(llm()).await;
    let weather_json = ext("main.py");
    let home = [
        ("config/august.json", SUPERVISE),
        ("config/extensions/monitor.json", EARLY),
        ("extensions/weather/extension.json", weather_json.as_str()),
        ("extensions/weather/main.py", WEATHER),
        ("extensions/weather/sdk.py", SDK),
        ("extensions/monitor/extension.json", r#"{"command": ["python3", "main.py"]}"#),
        ("extensions/monitor/main.py", MONITOR),
        ("extensions/monitor/sdk.py", SDK),
    ];
    let gw = august(&fake, Setup { home: &home, ..Default::default() }).await;
    let mut chat = gw.chat().await;
    chat.ask("/home", "home thread").await;

    // A crash: reported with its reason, then the restart.
    chat.say("/die").await;
    chat.wait_for("monitor: failed (exit) restarts=0 final=False").await;
    chat.wait_for("monitor: running (restart) restarts=1").await;
    chat.ask("what's the weather?", "Result: Paris: sunny").await;

    // A frozen process answers no health check: killed as hung, and restarted.
    chat.say("/freeze").await;
    chat.wait_for("monitor: failed (hang) restarts=1").await;
    chat.wait_for("monitor: running (restart) restarts=2").await;
    let log = chat.ask("/logs weather", "no reply to 2 health checks").await;
    assert!(log.contains("[august] exited with code 3"), "{log}");

    // Past `max_restarts` it stays down, and says so.
    chat.say("/die").await;
    chat.wait_for("monitor: failed (exit) restarts=2 final=True").await;
    chat.ask("/extensions", "❌ weather — crashed").await;
}

#[tokio::test]
async fn extensions_report_their_own_health() {
    if !have_python() {
        return;
    }
    let fake = Fake::llm(llm()).await;
    let weather_json = ext("main.py");
    let home = [
        ("config/august.json", SUPERVISE),
        ("config/extensions/monitor.json", EARLY),
        ("extensions/weather/extension.json", weather_json.as_str()),
        ("extensions/weather/main.py", WEATHER),
        ("extensions/weather/sdk.py", SDK),
        ("extensions/monitor/extension.json", r#"{"command": ["python3", "main.py"]}"#),
        ("extensions/monitor/main.py", MONITOR),
        ("extensions/monitor/sdk.py", SDK),
    ];
    let gw = august(&fake, Setup { home: &home, ..Default::default() }).await;
    let mut chat = gw.chat().await;
    chat.ask("/home", "home thread").await;
    let health = gw.home.join("extensions/weather/health.txt");

    // Degraded: shown, not restarted — something outside is wrong.
    std::fs::write(&health, "degraded|no API key").unwrap();
    chat.wait_for("monitor: degraded (unhealthy)").await;
    chat.ask("/extensions", "⚠️ weather — degraded: no API key").await;
    chat.ask("what's the weather?", "Result: Paris: sunny").await;
    std::fs::remove_file(&health).unwrap();
    chat.wait_for("monitor: running (recovered)").await;

    // Failed: broken inside, so it is restarted.
    std::fs::write(&health, "failed|poller stalled").unwrap();
    chat.wait_for("monitor: failed (unhealthy)").await;
    std::fs::remove_file(&health).unwrap();
    chat.wait_for("monitor: running (restart) restarts=1").await;
    chat.ask("/extensions", "✅ weather").await;
}

/// Hooks on launching: blocks one extension, changes another's environment, takes `admin`
/// from it, remembers why each one started and what it wrote (`/seen`), and keeps it down
/// once it crashes.
const GUARD: &str = r#"
from sdk import August

a = August("Guards launches")
a.needs = ["admin"]
seen = []
a.commands["seen"] = lambda _: "seen: " + "; ".join(seen)
def launch(d):
    seen.append(f"{d['name']} ({d['reason']}, {len(d['changed'])} changed)")
    if d["name"] == "evil":
        return {"block": "not on my watch"}
    if d["name"] == "weather":
        return {"env": {**d["env"], "GREETING": "from the guard"}}
def ready(d):
    needs = d["manifest"]["needs"]
    if d["name"] == "weather" and "admin" in needs:
        return {"needs": [n for n in needs if n != "admin"]}
def output(d):
    if d["name"] == "weather":
        seen.append("said " + d["line"])
a.on("extension_launch", launch)
a.on("extension_ready", ready)
a.on("extension_output", output)
a.on("extension_exit", lambda d: {"restart": False} if d["name"] == "weather" else None)
a.run()
"#;

/// Asks for `admin` and tries to use it.
const GREEDY: &str = r#"
import os, sys
from sdk import August

print("greedy says hi", file=sys.stderr, flush=True)
a = August("Greedy weather")
a.needs = ["admin"]
a.tools["weather"] = lambda i: f"{i.get('city')}: sunny, {__import__('os').environ.get('GREETING')}"
a.commands["coup"] = lambda _: str(a.call("extensions", {}))
a.commands["die"] = lambda _: os._exit(1)
a.run()
"#;

#[tokio::test]
async fn hooks_watch_extensions_launch_and_can_block_change_or_limit_them() {
    if !have_python() {
        return;
    }
    let fake = Fake::llm(llm()).await;
    let weather_json = ext("main.py");
    let home = [
        ("config/extensions/guard.json", EARLY),
        ("extensions/guard/extension.json", r#"{"command": ["python3", "main.py"]}"#),
        ("extensions/guard/main.py", GUARD),
        ("extensions/guard/sdk.py", SDK),
        ("extensions/weather/extension.json", weather_json.as_str()),
        ("extensions/weather/main.py", GREEDY),
        ("extensions/weather/sdk.py", SDK),
        ("extensions/evil/extension.json", r#"{"command": ["python3", "-c", "print('should never run')"]}"#),
    ];
    let mut gw = august(&fake, Setup { home: &home, ..Default::default() }).await;
    let mut chat = gw.chat().await;

    // The guard ran before the others started: one blocked, one changed and limited.
    let status = chat.ask("/extensions", "evil").await;
    assert!(status.contains("❌ evil — launch blocked: not on my watch"), "{status}");
    chat.ask("what's the weather?", "Result: Paris: sunny, from the guard").await;
    let refused = chat.ask("/coup", "admin").await;
    assert!(refused.contains("needs the `admin` permission"), "{refused}");

    // It saw why each one started: new ones, then a plain start, then a change.
    chat.ask("/seen", "weather (install, 3 changed)").await;
    gw.restart().await;
    let mut chat = gw.chat().await;
    chat.ask("/seen", "weather (start, 0 changed)").await;
    std::fs::write(gw.home.join("extensions/weather/main.py"), GREEDY.replace("sunny", "rainy")).unwrap();
    chat.ask("/reload", "weather").await;
    chat.ask("/seen", "weather (update, 1 changed)").await;
    chat.ask("what's the weather?", "Result: Paris: rainy").await;
    chat.ask("/seen", "said greedy says hi").await;

    // Its exit hook keeps the crashed one down: no restart, however long we wait.
    chat.say("/die").await;
    chat.wait_for("/die failed").await;
    tokio::time::sleep(std::time::Duration::from_millis(1_500)).await;
    chat.ask("/extensions", "❌ weather — crashed").await;
}

/// Reads what its setup built.
const BUILT: &str = r#"
from sdk import August

a = August("Built weather")
a.tools["weather"] = lambda i: "built: " + open("built.txt").read()
a.run()
"#;

/// Judges setup steps (`setup:step`): no `rm`, whatever the extension asks.
const JUDGE: &str = r#"
from sdk import August

a = August("Judges setup steps")
def step(d):
    if d["command"][0] == "rm":
        return {"block": f"no rm in setup of {d['extension']} (step {d['index']}/{d['total']})"}
a.on("setup:step", step)
a.run()
"#;

#[tokio::test]
async fn setup_steps_build_an_extension_before_it_starts_and_hooks_judge_each_step() {
    if !have_python() {
        return;
    }
    let fake = Fake::llm(llm()).await;
    let build = r#"{"command": ["python3", "main.py"],
        "setup": [["python3", "-c", "open('built.txt', 'a').write('x')"], ["python3", "-c", "print('setup ran')"]]}"#;
    let risky = r#"{"command": ["python3", "main.py"], "setup": [["rm", "-rf", "nothing-here"]]}"#;
    let broken = r#"{"command": ["python3", "main.py"], "setup": [["python3", "-c", "import sys; print('bad build'); sys.exit(4)"]]}"#;
    let home = [
        ("config/extensions/judge.json", EARLY),
        ("extensions/judge/extension.json", r#"{"command": ["python3", "main.py"]}"#),
        ("extensions/judge/main.py", JUDGE),
        ("extensions/judge/sdk.py", SDK),
        ("extensions/weather/extension.json", build),
        ("extensions/weather/main.py", BUILT),
        ("extensions/weather/sdk.py", SDK),
        ("extensions/risky/extension.json", risky),
        ("extensions/broken/extension.json", broken),
    ];
    let mut gw = august(&fake, Setup { home: &home, ..Default::default() }).await;
    let mut chat = gw.chat().await;

    // Built on install, then started; the build's output is in setup's log.
    chat.ask("what's the weather?", "Result: built: x").await;
    let status = chat.ask("/extensions", "risky").await;
    assert!(status.contains("❌ risky — launch blocked: setup step 1 blocked: no rm in setup of risky (step 1/1)"), "{status}");
    assert!(status.contains("❌ broken — launch blocked: setup step 1 (python3 -c"), "{status}");
    assert!(status.contains("failed with code 4: bad build"), "{status}");
    let log = std::fs::read_to_string(gw.home.join("logs/extensions/setup.log")).unwrap();
    assert!(log.contains("weather: setup 2/2: python3 -c") && log.contains("weather: setup ran"), "{log}");

    // A plain restart doesn't build again; asking does.
    gw.restart().await;
    let mut chat = gw.chat().await;
    chat.ask("what's the weather?", "Result: built: x").await;
    chat.ask("/setup weather", "✅ weather").await;
    chat.ask("what's the weather?", "Result: built: xx").await;
}

#[tokio::test]
async fn the_agent_installs_an_extension_in_any_language_and_august_sets_it_up() {
    if !have_python() {
        return;
    }
    let pick: fn(&str) -> Value = |text| {
        let files = |setup: &str| json!({
            "extension.json": format!(r#"{{"command": ["python3", "main.py"], "setup": [{setup}]}}"#),
            "main.py": BUILT,
            "sdk.py": SDK,
        });
        if text.contains("broken") {
            let noisy = r#"["python3", "-c", "import sys; print('error: the first error'); print('help: a hint\\n' * 400); sys.exit(2)"]"#;
            reply_tool("save_extension", json!({"name": "weather", "files": files(noisy)}))
        } else if text.contains("install") {
            reply_tool("save_extension", json!({"name": "weather", "files": files(r#"["python3", "-c", "open('built.txt', 'w').write('by setup')"]"#)}))
        } else if text.contains("escape") {
            reply_tool("save_extension", json!({"name": "weather", "files": {"extension.json": "{}", "../evil.py": "x"}}))
        } else {
            reply_tool("weather", json!({"city": "Paris"}))
        }
    };
    let fake = Fake::llm(Box::new(move |req: &Value| {
        let msgs = req["messages"].as_array().unwrap();
        let last = msgs.last().unwrap();
        if last["role"] == "tool" {
            return reply_text(&format!("Result: {}", last["content"].as_str().unwrap_or("")));
        }
        let text = last["content"].as_str().map(String::from).unwrap_or_else(|| last["content"].to_string());
        pick(&text)
    }))
    .await;
    let gw = august(&fake, Setup::default()).await;
    let mut chat = gw.chat().await;

    // Saved, set up by August (not by hand), started, usable right away.
    chat.say("install a weather extension in python").await;
    chat.press(&chat.question().await.button("Allow")).await;
    let saved = chat.wait_for("Result: saved").await;
    assert!(saved.text.contains("extension.json, main.py, sdk.py") && saved.text.contains("✅ weather — tools: weather"), "{}", saved.text);
    chat.ask("what's the weather?", "Result: built: by setup").await;

    // A setup that fails comes back with the reason, to fix and save again.
    chat.say("make it broken").await;
    chat.press(&chat.question().await.button("Allow")).await;
    let failed = chat.wait_for("failed to start").await;
    // A long build output keeps its start, where compilers put the first error, and says
    // where all of it and the extension's own log are.
    assert!(failed.text.contains("setup step 1") && failed.text.contains("error: the first error"), "{}", failed.text);
    assert!(failed.text.contains("characters …]") && failed.text.contains("logs/extensions/setup.log"), "{}", failed.text);
    assert!(failed.text.contains("logs/extensions/weather.log"), "{}", failed.text);

    // Files stay inside the extension's folder.
    chat.say("escape the folder").await;
    chat.press(&chat.question().await.button("Allow")).await;
    chat.wait_for("stay inside the extension's folder").await;
    assert!(!gw.home.join("extensions/evil.py").exists());
}
