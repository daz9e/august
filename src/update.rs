//! Self-update: install the latest release binary (or, with `update.from_source`, build the
//! latest green commit with cargo), check that it runs, and swap it in for this binary.

use crate::config::{self, Config};
use crate::{agent, llm, service, telegram::Bot};
use anyhow::{Context, Result, anyhow, bail};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

const REPO: &str = "daz9e/august";
const BRANCH: &str = "master";
/// Commit this binary was built from. Releases and `august update` set it; local builds have none.
pub const COMMIT: Option<&str> = option_env!("AUGUST_COMMIT");
/// Service starts allowed with an unconfirmed update before rolling back.
const MAX_STARTS: u32 = 3;

/// A newer version on GitHub and the subjects of what it adds (oldest first; empty for local builds).
pub struct Available {
    pub sha: String,
    /// Release tag; None when building from source.
    pub tag: Option<String>,
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

fn from_source() -> bool {
    Config::load().is_ok_and(|c| c.update.from_source)
}

/// What is new on GitHub; None when this binary is the latest version.
pub async fn check() -> Result<Option<Available>> {
    let (sha, tag) = if from_source() {
        let head = github(&format!("commits/{BRANCH}")).await?;
        let sha = head["sha"].as_str().context("no sha in GitHub's answer")?.to_string();
        if COMMIT != Some(sha.as_str()) && !ci_passed(&sha).await? {
            bail!("{BRANCH} ({}) has not passed CI yet; try again later", &sha[..7]);
        }
        (sha, None)
    } else {
        let release = github("releases/latest").await.context("no release found")?;
        let tag = release["tag_name"].as_str().context("no tag in GitHub's answer")?.to_string();
        let commit = github(&format!("commits/{tag}")).await?;
        (commit["sha"].as_str().context("no sha in GitHub's answer")?.to_string(), Some(tag))
    };
    let Some(current) = COMMIT else {
        return Ok(Some(Available { sha, tag, commits: vec![] }));
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
    Ok(Some(Available { sha, tag, commits }))
}

/// Every check run on the commit finished green (and there is at least one).
async fn ci_passed(sha: &str) -> Result<bool> {
    let runs = github(&format!("commits/{sha}/check-runs")).await?;
    let runs = runs["check_runs"].as_array().cloned().unwrap_or_default();
    Ok(!runs.is_empty() && runs.iter().all(|r| matches!(r["conclusion"].as_str(), Some("success" | "skipped" | "neutral"))))
}

pub fn describe(a: &Available) -> String {
    let name = match &a.tag {
        Some(tag) => format!("{tag} ({})", &a.sha[..7]),
        None => a.sha[..7].to_string(),
    };
    match (COMMIT, a.commits.len()) {
        (None, _) => format!("This is a local build; `update` installs {name} from GitHub."),
        (Some(_), 0) => format!("A new version is available: {name}."),
        (Some(_), n) => {
            let list: Vec<String> = a.commits.iter().rev().take(15).map(|c| format!("- {c}")).collect();
            let more = if n > 15 { format!("\n- … and {} more", n - 15) } else { String::new() };
            format!("A new version is available: {name}, {n} new commit{}:\n{}{more}", if n == 1 { "" } else { "s" }, list.join("\n"))
        }
    }
}

/// Checks, installs and restarts. `show_build` passes cargo's output through (the CLI);
/// otherwise only its error tail is reported. Returns what happened, for the user.
pub async fn update(show_build: bool) -> Result<String> {
    let Some(a) = check().await? else {
        return Ok(format!("Already up to date: {}.", version()));
    };
    if show_build {
        let doing = if a.tag.is_some() { "Downloading…" } else { "Building (a few minutes the first time)…" };
        println!("{}\n\n{doing}", describe(&a));
    }
    install(&a, show_build).await?;
    let mut out = if show_build { String::new() } else { describe(&a) + "\n\n" };
    out += &format!("Installed {}.", a.tag.as_deref().unwrap_or(&a.sha[..7]));
    out += &restart();
    Ok(out)
}

fn root() -> PathBuf {
    config::home().join("build")
}

async fn install(a: &Available, show_build: bool) -> Result<()> {
    let root = root();
    std::fs::create_dir_all(&root)?;
    // Released by the OS when this process ends, so a crash never leaves it stuck.
    let lock = std::fs::File::create(root.join("update.lock"))?;
    lock.try_lock().map_err(|_| anyhow!("another update is already running"))?;
    let new = match &a.tag {
        Some(tag) => download(tag, &root).await?,
        None => build(&a.sha, &root, show_build).await?,
    };
    let out = tokio::process::Command::new(&new).arg("--version").output().await.context("cannot run the new binary")?;
    if !out.status.success() {
        bail!("the new binary does not run; keeping this one\n{}", String::from_utf8_lossy(&out.stderr));
    }
    let exe = std::env::current_exe()?.canonicalize()?;
    std::fs::copy(&exe, root.join("august.old"))?; // for the rollback in `guard_start`
    replace(&exe, &new)?;
    agent::log(&format!("updated to {}", a.sha));
    Ok(())
}

/// Puts `new` in place of `exe` with a rename: the old inode stays valid for running processes.
fn replace(exe: &Path, new: &Path) -> Result<()> {
    let tmp = exe.with_extension("new");
    std::fs::copy(new, &tmp)?;
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755))?;
    std::fs::rename(&tmp, exe).with_context(|| format!("cannot replace {}", exe.display()))
}

/// Release asset name for this machine, as published by .github/workflows/release.yml.
fn asset() -> Result<String> {
    if std::env::consts::OS != "macos" {
        bail!("release binaries are macOS only; set update.from_source to build from source");
    }
    Ok(format!("august-{}-macos", std::env::consts::ARCH))
}

async fn download(tag: &str, root: &Path) -> Result<PathBuf> {
    let name = asset()?;
    let get = |file: String| async move {
        let res = llm::http_client().get(format!("https://github.com/{REPO}/releases/download/{tag}/{file}")).send().await?;
        if !res.status().is_success() {
            bail!("downloading {file} failed: GitHub answered {}", res.status());
        }
        Ok(res.bytes().await?)
    };
    let sums = get(format!("{name}.sha256")).await?;
    let want = String::from_utf8_lossy(&sums).split_whitespace().next().unwrap_or_default().to_lowercase();
    let bin = get(name).await?;
    let got: String = Sha256::digest(&bin).iter().map(|b| format!("{b:02x}")).collect();
    if got != want {
        bail!("the downloaded binary does not match its checksum; not installing it");
    }
    let path = root.join("august.download");
    std::fs::write(&path, &bin)?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))?;
    Ok(path)
}

async fn build(sha: &str, root: &Path, show_build: bool) -> Result<PathBuf> {
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
    let rustup_cargo = config::user_home().join(".cargo/bin/cargo"); // the service's PATH may lack it
    let cargo = if rustup_cargo.exists() { rustup_cargo.into_os_string() } else { "cargo".into() };
    let mut cmd = tokio::process::Command::new(cargo);
    cmd.args(["install", "--locked", "--force", "--path"])
        .arg(&src)
        .arg("--root")
        .arg(root)
        .env("CARGO_TARGET_DIR", root.join("target")) // kept between updates, so rebuilds are incremental
        .env("AUGUST_COMMIT", sha);
    run(&mut cmd, show_build).await?;
    Ok(root.join("bin/august"))
}

/// Run first thing in `august serve`. After an update the service must stay up for a minute
/// (`confirm_start`); if it instead keeps dying, put the previous binary back and exit, so
/// launchd/systemd start that one.
pub fn guard_start() {
    let pending = config::home().join("update-pending");
    let Ok(text) = std::fs::read_to_string(&pending) else { return };
    let starts = text.trim().parse::<u32>().unwrap_or(0) + 1;
    if starts < MAX_STARTS {
        std::fs::write(&pending, starts.to_string()).ok();
        return;
    }
    std::fs::remove_file(&pending).ok();
    let old = root().join("august.old");
    let Ok(exe) = std::env::current_exe().and_then(|e| e.canonicalize()) else { return };
    if !old.exists() || replace(&exe, &old).is_err() {
        agent::log("the updated service keeps failing to start and there is no previous binary to go back to");
        return;
    }
    let note = format!("The update to {} kept failing to start, so August went back to the previous version.", version());
    agent::log(&note);
    std::fs::write(config::home().join("update-rolled-back"), note).ok();
    std::process::exit(1);
}

/// The service has been up long enough after an update: keep it.
pub async fn confirm_start() {
    tokio::time::sleep(Duration::from_secs(60)).await;
    std::fs::remove_file(config::home().join("update-pending")).ok();
}

fn mark_pending() {
    std::fs::write(config::home().join("update-pending"), "0").ok();
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
    // This process is the service: exit shortly (so the reply goes out first); launchd/systemd start the new one.
    if service::is_self() {
        mark_pending();
        tokio::spawn(async {
            tokio::time::sleep(Duration::from_secs(3)).await;
            crate::tools::kill_background();
            std::process::exit(0);
        });
        return " Restarting…".into();
    }
    let mut out = String::new();
    if service::status().is_some() {
        mark_pending();
        out += &match service::restart() {
            Ok(()) => " Restarted the background service.".into(),
            Err(e) => format!(" Could not restart the background service: {e:#}"),
        };
    }
    out + " Restart `august` in the terminal to use it."
}

/// Tells the Telegram owners once about each new version; runs inside `august serve`.
pub async fn notify(bot: Arc<Bot>) {
    let rolled_back = config::home().join("update-rolled-back");
    if let Ok(note) = std::fs::read_to_string(&rolled_back) {
        for user in Config::load().map(|c| c.telegram.allowed).unwrap_or_default() {
            bot.api.send(user, &note).await.ok();
        }
        std::fs::remove_file(&rolled_back).ok();
    }
    if COMMIT.is_none() {
        return; // local builds are always "behind"
    }
    let mark = config::home().join("update-notified");
    loop {
        if let Ok(Some(a)) = check().await
            && std::fs::read_to_string(&mark).ok().as_deref() != Some(a.sha.as_str())
        {
            let text = format!("{}\n\nSend /update to install it.", describe(&a));
            for user in Config::load().map(|c| c.telegram.allowed).unwrap_or_default() {
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
        let a = Available { sha: "abcdef0123".into(), tag: None, commits: vec!["First".into(), "Second".into()] };
        let text = describe(&a);
        match COMMIT {
            None => assert!(text.contains("local build") && text.contains("abcdef0")),
            Some(_) => assert!(text.contains("2 new commits:\n- Second\n- First"), "{text}"),
        }
    }
}
