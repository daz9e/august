//! Self-update: build the latest commit from GitHub with cargo and swap it in for this binary.

use crate::config::{self, Config};
use crate::{agent, llm, service, telegram::Bot};
use anyhow::{Context, Result, bail};
use serde_json::Value;
use std::sync::Arc;
use std::time::Duration;

const REPO: &str = "daz9e/august";
const BRANCH: &str = "master";
/// Commit this binary was built from. `august update` sets it; local builds have none.
pub const COMMIT: Option<&str> = option_env!("AUGUST_COMMIT");

/// A newer commit on GitHub and the subjects of what it adds (oldest first; empty for local builds).
pub struct Available {
    pub sha: String,
    pub commits: Vec<String>,
}

pub fn version() -> String {
    match COMMIT {
        Some(c) => format!("{} ({})", env!("CARGO_PKG_VERSION"), &c[..7.min(c.len())]),
        None => format!("{} (local build)", env!("CARGO_PKG_VERSION")),
    }
}

async fn github(path: &str) -> Result<Value> {
    let res = llm::http_client()
        .get(format!("https://api.github.com/repos/{REPO}/{path}"))
        .header("Accept", "application/vnd.github+json")
        .timeout(Duration::from_secs(15))
        .send()
        .await?;
    if !res.status().is_success() {
        bail!("GitHub answered {}", res.status());
    }
    Ok(res.json().await?)
}

/// What is new on GitHub; None when this binary is the latest commit.
pub async fn check() -> Result<Option<Available>> {
    let head = github(&format!("commits/{BRANCH}")).await?;
    let sha = head["sha"].as_str().context("no sha in GitHub's answer")?.to_string();
    let Some(current) = COMMIT else {
        return Ok(Some(Available { sha, commits: vec![] }));
    };
    if current == sha {
        return Ok(None);
    }
    // A failed compare (e.g. history rewritten) still means there is something newer.
    let commits = match github(&format!("compare/{current}...{sha}")).await {
        Ok(cmp) => cmp["commits"]
            .as_array()
            .map(|a| a.iter().filter_map(|c| c["commit"]["message"].as_str()?.lines().next().map(str::to_string)).collect())
            .unwrap_or_default(),
        Err(_) => vec![],
    };
    Ok(Some(Available { sha, commits }))
}

pub fn describe(a: &Available) -> String {
    let short = &a.sha[..7];
    match (COMMIT, a.commits.len()) {
        (None, _) => format!("This is a local build; `update` installs {short} from GitHub."),
        (Some(_), 0) => format!("A new version is available: {short}."),
        (Some(_), n) => {
            let list: Vec<String> = a.commits.iter().rev().take(15).map(|c| format!("- {c}")).collect();
            let more = if n > 15 { format!("\n- … and {} more", n - 15) } else { String::new() };
            format!("A new version is available: {short}, {n} new commit{}:\n{}{more}", if n == 1 { "" } else { "s" }, list.join("\n"))
        }
    }
}

/// Checks, builds and installs. `show_build` passes cargo's output through (the CLI);
/// otherwise only its error tail is reported. Returns what happened, for the user.
pub async fn update(show_build: bool) -> Result<String> {
    let Some(a) = check().await? else {
        return Ok(format!("Already up to date: {}.", version()));
    };
    if show_build {
        println!("{}\n\nBuilding (a few minutes the first time)…", describe(&a));
    }
    install(&a.sha, show_build).await?;
    let mut out = if show_build { String::new() } else { describe(&a) + "\n\n" };
    out += &format!("Installed {}.", &a.sha[..7]);
    out += &restart();
    Ok(out)
}

async fn install(sha: &str, show_build: bool) -> Result<()> {
    let root = config::home().join("build");
    let src = root.join("src");
    // A path install, not `cargo install --git`: cargo never rechecks git packages in a
    // reused target dir, so it would keep the old build.
    if !src.join(".git").exists() {
        std::fs::create_dir_all(&src)?;
        run(tokio::process::Command::new("git").arg("-C").arg(&src).arg("init").arg("-q"), false).await?;
    }
    let url = format!("https://github.com/{REPO}");
    run(tokio::process::Command::new("git").arg("-C").arg(&src).args(["fetch", "-q", "--depth", "1", &url, sha]), false).await?;
    run(tokio::process::Command::new("git").arg("-C").arg(&src).args(["checkout", "-q", "--force", "FETCH_HEAD"]), false).await?;
    let rustup_cargo = config::user_home().join(".cargo/bin/cargo"); // launchd's PATH may lack it
    let cargo = if rustup_cargo.exists() { rustup_cargo.into_os_string() } else { "cargo".into() };
    let mut cmd = tokio::process::Command::new(cargo);
    cmd.args(["install", "--locked", "--force", "--path"])
        .arg(&src)
        .arg("--root")
        .arg(&root)
        .env("CARGO_TARGET_DIR", root.join("target")) // kept between updates, so rebuilds are incremental
        .env("AUGUST_COMMIT", sha);
    run(&mut cmd, show_build).await?;
    // Rename over the running binary: the old inode stays valid for this process.
    let exe = std::env::current_exe()?.canonicalize()?;
    let tmp = exe.with_extension("new");
    std::fs::copy(root.join("bin/august"), &tmp)?;
    std::fs::rename(&tmp, &exe).with_context(|| format!("cannot replace {}", exe.display()))?;
    agent::log(&format!("updated to {sha}"));
    Ok(())
}

/// Runs a build step; on failure the error carries the tail of its output.
async fn run(cmd: &mut tokio::process::Command, show_output: bool) -> Result<()> {
    cmd.stdin(std::process::Stdio::null());
    if !show_output {
        cmd.stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped());
    }
    let program = cmd.as_std().get_program().to_owned();
    let out = cmd.output().await.with_context(|| format!("cannot run {program:?}; updating needs git and cargo (https://rustup.rs)"))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        let tail: Vec<&str> = err.lines().rev().take(15).collect::<Vec<_>>().into_iter().rev().collect();
        bail!("{program:?} failed\n{}", tail.join("\n"));
    }
    Ok(())
}

/// Restarts whatever runs the old binary and says so.
fn restart() -> String {
    // This process is the launchd service: exit shortly (so the reply goes out first); KeepAlive starts the new one.
    if std::env::var("XPC_SERVICE_NAME").is_ok_and(|n| n == service::LABEL) {
        tokio::spawn(async {
            tokio::time::sleep(Duration::from_secs(3)).await;
            crate::tools::kill_background();
            std::process::exit(0);
        });
        return " Restarting…".into();
    }
    let mut out = String::new();
    if service::status().is_some() {
        out += &match service::restart() {
            Ok(()) => " Restarted the background service.".into(),
            Err(e) => format!(" Could not restart the background service: {e:#}"),
        };
    }
    out + " Restart `august` in the terminal to use it."
}

/// Tells the Telegram owners once about each new version; runs inside `august serve`.
pub async fn notify(bot: Arc<Bot>) {
    if COMMIT.is_none() {
        return; // local builds are always "behind"
    }
    let mark = config::home().join("update-notified");
    loop {
        if let Ok(Some(a)) = check().await
            && std::fs::read_to_string(&mark).ok().as_deref() != Some(a.sha.as_str())
        {
            let text = format!("{}\n\nSend /update to install it.", describe(&a));
            for user in Config::load().map(|c| c.telegram_allowed).unwrap_or_default() {
                bot.api.send(user, &text).await.ok();
            }
            std::fs::write(&mark, &a.sha).ok();
        }
        tokio::time::sleep(Duration::from_secs(6 * 3600)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn describes_updates() {
        let a = Available { sha: "abcdef0123".into(), commits: vec!["First".into(), "Second".into()] };
        let text = describe(&a);
        match COMMIT {
            None => assert!(text.contains("local build") && text.contains("abcdef0")),
            Some(_) => assert!(text.contains("2 new commits:\n- Second\n- First"), "{text}"),
        }
    }
}
