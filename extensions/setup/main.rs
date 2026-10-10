//! `setup`: runs the `setup` steps of an extension's `extension.json` before it starts —
//! build it, fetch it, install its dependencies — when it is new, when its files changed, or
//! when asked (`/setup <name>`). Each step is a command run in the extension's folder; the
//! `setup:step` hooks see it first and may change or block it, `setup:done` tells how it went.
//! The core knows nothing of this: it is an `extension_launch` hook.

use august_ext::August;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Output kept to explain a failed step.
const TAIL: usize = 2_000;

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let august = August::new();
    august.describe(
        "Runs extensions' setup steps (build, download, install) before they start",
        "An extension's `extension.json` may list `setup`: commands (argv lists) run in its folder \
         before it starts — on install, when its files changed, or on /setup <name>. A failed step \
         keeps it from starting, with the step's output as the reason. `setup:step` hooks may \
         change or block each step; `setup:done` reports the outcome. Step output is in this \
         extension's log.",
    );
    august.needs(&["admin"]);
    august.settings_schema(json!({
        "type": "object",
        "properties": {
            "step_timeout_s": {"type": "integer", "default": 600, "description": "How long one setup step may run"},
        },
    }));
    august.define_event(
        "step",
        "Before a setup step runs: return {command} to change it or {block} to stop the setup",
        json!({"type": "object", "properties": {"extension": {"type": "string"}, "dir": {"type": "string"},
               "index": {"type": "integer"}, "total": {"type": "integer"}, "command": {"type": "array"}},
               "required": ["extension", "dir", "index", "total", "command"]}),
        false,
    );
    august.define_event(
        "done",
        "After an extension's setup: {extension, ok, error, duration_ms}",
        json!({"type": "object", "properties": {"extension": {"type": "string"}, "ok": {"type": "boolean"}}, "required": ["extension", "ok"]}),
        true,
    );
    august.hook_timeout("extension_launch", Duration::from_secs(1800));

    let me = august.clone();
    august.on("extension_launch", move |data, ctx| {
        let august = me.clone();
        async move {
            let name = data["name"].as_str().unwrap_or_default().to_string();
            let dir = PathBuf::from(data["dir"].as_str().unwrap_or_default());
            let steps = steps(&dir);
            let forced = august.get(&format!("force:{name}")).await?.is_some();
            let due = forced || matches!(data["reason"].as_str(), Some("install" | "update"));
            if steps.is_empty() || !due {
                return Ok(None);
            }
            august.set(&format!("force:{name}"), Value::Null).await?;
            let timeout = Duration::from_secs(august.settings().await?["step_timeout_s"].as_u64().unwrap_or(600));
            let started = Instant::now();
            let outcome = run(&ctx, &name, &dir, &steps, timeout).await;
            let error = outcome.as_ref().err().cloned();
            let done = json!({"extension": name, "ok": error.is_none(), "error": error, "duration_ms": started.elapsed().as_millis() as u64});
            ctx.emit("done", done).await.ok();
            Ok(error.map(|e| json!({"block": e})))
        }
    });

    let me = august.clone();
    august.register_command("setup", "Run an extension's setup steps again and restart it: /setup <name>", move |args, _| {
        let august = me.clone();
        async move {
            let name = args.trim().to_string();
            if name.is_empty() {
                return Ok(Some("Usage: /setup <extension>".into()));
            }
            august.set(&format!("force:{name}"), json!(true)).await?;
            let line = august.call("extension_enable", json!({"name": name})).await;
            Ok(Some(match line {
                Ok(v) => v.as_str().unwrap_or_default().to_string(),
                Err(e) => format!("{e:#}"),
            }))
        }
    });
    august.run().await;
}

/// The `setup` steps of the extension in `dir`: argv lists.
fn steps(dir: &Path) -> Vec<Vec<String>> {
    let spec: Value = std::fs::read_to_string(dir.join("extension.json")).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default();
    spec["setup"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|step| step.as_array()?.iter().map(|a| a.as_str().map(String::from)).collect::<Option<Vec<_>>>())
        .filter(|argv| !argv.is_empty())
        .collect()
}

/// Runs the steps in order, each through the `setup:step` hooks; the first failure stops them.
async fn run(ctx: &august_ext::Ctx, name: &str, dir: &Path, steps: &[Vec<String>], timeout: Duration) -> Result<(), String> {
    let total = steps.len();
    for (i, step) in steps.iter().enumerate() {
        let index = i + 1;
        let data = json!({"extension": name, "dir": dir, "index": index, "total": total, "command": step});
        let data = ctx.emit("step", data).await.map_err(|e| format!("setup step {index}: {e:#}"))?;
        if let Some(why) = blocked(&data) {
            return Err(format!("setup step {index} blocked: {why}"));
        }
        let argv: Vec<String> = data["command"].as_array().into_iter().flatten().filter_map(|a| a.as_str().map(String::from)).collect();
        if argv.is_empty() {
            return Err(format!("setup step {index}: a setup:step hook left no command"));
        }
        eprintln!("{name}: setup {index}/{total}: {}", argv.join(" "));
        let out = exec(&argv, dir, timeout).await.map_err(|e| format!("setup step {index} ({}): {e}", argv.join(" ")))?;
        let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
        for line in text.lines() {
            eprintln!("{name}: {line}");
        }
        if !out.status.success() {
            let tail: String = text.chars().rev().take(TAIL).collect::<Vec<_>>().into_iter().rev().collect();
            let code = out.status.code().map_or("a signal".into(), |c| format!("code {c}"));
            return Err(format!("setup step {index} ({}) failed with {code}: {}", argv.join(" "), tail.trim()));
        }
    }
    Ok(())
}

fn blocked(data: &Value) -> Option<String> {
    match &data["block"] {
        Value::String(s) if !s.is_empty() => Some(s.clone()),
        Value::Bool(true) => Some("by a hook".into()),
        _ => None,
    }
}

/// Runs `argv` in `dir` in a process group of its own, killed whole on timeout.
async fn exec(argv: &[String], dir: &Path, timeout: Duration) -> Result<std::process::Output, String> {
    // A relative program (`./build.sh`) is the extension's own, in its folder.
    let program = Path::new(&argv[0]);
    let program = if program.components().count() > 1 && program.is_relative() { dir.join(program) } else { program.to_path_buf() };
    let child = tokio::process::Command::new(&program)
        .args(&argv[1..])
        .current_dir(dir)
        .process_group(0)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => format!("`{}` was not found: is it installed and on PATH?", argv[0]),
            _ => e.to_string(),
        })?;
    let pid = child.id().map(|p| p as i32);
    match tokio::time::timeout(timeout, child.wait_with_output()).await {
        Ok(out) => out.map_err(|e| e.to_string()),
        Err(_) => {
            if let Some(pid) = pid {
                // SAFETY: plain syscall on the step's own process group.
                unsafe { libc::killpg(pid, libc::SIGKILL) };
            }
            Err(format!("timed out after {} s", timeout.as_secs()))
        }
    }
}
