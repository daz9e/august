//! `tools` against a fake core: `bash`, `read`, `write` and `edit` work in the workspace and
//! never outside it.

use super::serve;
use august_ext::FakeAugust;
use august_ext::fake::ctx;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

/// The extension with a fresh workspace (kept alive by the returned dir).
async fn tools() -> (FakeAugust, tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path().join("ws");
    std::fs::create_dir(&ws).unwrap();
    let ws = ws.canonicalize().unwrap();
    let (fake, august) = FakeAugust::new(&[("AUGUST_WORKSPACE", ws.to_str().unwrap())]);
    tokio::spawn(serve(august));
    fake.started().await;
    (fake, dir, ws)
}

async fn run(fake: &FakeAugust, tool: &str, input: Value) -> Result<String, String> {
    fake.tool(tool, input, ctx("1")).await.map_err(|e| e.to_string())
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap()
}

#[tokio::test]
async fn file_tools_stay_inside_the_workspace() {
    let (fake, dir, ws) = tools().await;
    std::fs::write(dir.path().join("secret.txt"), "outside").unwrap();
    std::os::unix::fs::symlink(dir.path(), ws.join("link")).unwrap();

    for path in ["../secret.txt", dir.path().join("secret.txt").to_str().unwrap(), "a/../../secret.txt"] {
        let err = run(&fake, "read", json!({"path": path})).await.unwrap_err();
        assert!(err.contains("outside the workspace"), "{path}: {err}");
    }
    // A symlink out of it doesn't help either, for files that exist or are to be made.
    let err = run(&fake, "read", json!({"path": "link/secret.txt"})).await.unwrap_err();
    assert!(err.contains("resolves outside the workspace"), "{err}");
    let err = run(&fake, "write", json!({"path": "link/new.txt", "content": "x"})).await.unwrap_err();
    assert!(err.contains("resolves outside the workspace"), "{err}");
    assert!(!dir.path().join("new.txt").exists());
    assert_eq!(read(&dir.path().join("secret.txt")), "outside");
}

#[tokio::test]
async fn write_read_and_edit_files() {
    let (fake, _dir, ws) = tools().await;
    // Parent directories are made.
    let out = run(&fake, "write", json!({"path": "notes/a.txt", "content": "keep keep"})).await.unwrap();
    assert!(out.starts_with("wrote 9 bytes to"), "{out}");
    assert_eq!(run(&fake, "read", json!({"path": "./notes/a.txt"})).await.unwrap(), "keep keep");

    // An edit has to match exactly once, unless it replaces all.
    let edit = |old: &str, new: &str, all: bool| json!({"path": "notes/a.txt", "old_string": old, "new_string": new, "replace_all": all});
    let err = run(&fake, "edit", edit("keep", "kept", false)).await.unwrap_err();
    assert!(err.contains("matches 2 places"), "{err}");
    let err = run(&fake, "edit", edit("gone", "x", false)).await.unwrap_err();
    assert!(err.contains("old_string not found"), "{err}");
    assert_eq!(read(&ws.join("notes/a.txt")), "keep keep");
    assert!(run(&fake, "edit", edit("keep", "kept", true)).await.unwrap().starts_with("edited"));
    assert_eq!(read(&ws.join("notes/a.txt")), "kept kept");
    run(&fake, "edit", edit("kept kept", "done", false)).await.unwrap();
    assert_eq!(read(&ws.join("notes/a.txt")), "done");
}

#[tokio::test]
async fn bash_runs_in_the_workspace_and_reports_how_it_went() {
    let (fake, _dir, ws) = tools().await;
    let out = run(&fake, "bash", json!({"command": "pwd; echo oops >&2; exit 3"})).await.unwrap();
    assert_eq!(out, format!("exit code: 3\nstdout:\n{}\n\nstderr:\noops\n", ws.display()));
    let err = run(&fake, "bash", json!({})).await.unwrap_err();
    assert!(err.contains("missing string argument `command`"), "{err}");
}

#[tokio::test]
async fn a_cancelled_command_takes_everything_it_started_with_it() {
    let (fake, _dir, ws) = tools().await;
    // The command puts a process in the background and says its pid.
    let cmd = "sleep 30 & echo $! > bg.pid.tmp; mv bg.pid.tmp bg.pid; wait";
    let (id, _reply) = fake.request_id("tool", json!({"name": "bash", "input": {"command": cmd}, "ctx": ctx("1")}));
    fake.wait_until("the background process to start", |_| ws.join("bg.pid").exists()).await;
    let pid: i32 = std::fs::read_to_string(ws.join("bg.pid")).unwrap().trim().parse().unwrap();
    fake.cancel(id);
    // SAFETY: signal 0 only checks whether the process exists.
    fake.wait_until("the cancelled command's background process to die", |_| unsafe { libc::kill(pid, 0) } != 0).await;
}
