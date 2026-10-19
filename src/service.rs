//! Autostart: a launchd agent running `august serve` (macOS).

use crate::config;
use anyhow::{Result, bail};
use std::path::PathBuf;
use std::process::Command;

const LABEL: &str = "com.august.agent";

fn plist() -> PathBuf {
    config::user_home().join("Library/LaunchAgents").join(format!("{LABEL}.plist"))
}

fn target() -> Result<String> {
    let out = Command::new("id").arg("-u").output()?;
    Ok(format!("gui/{}", String::from_utf8_lossy(&out.stdout).trim()))
}

fn xml(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

fn ensure_macos() -> Result<()> {
    if !cfg!(target_os = "macos") {
        bail!("autostart is only set up for macOS for now; run `august serve` from your init system (e.g. a systemd user unit)");
    }
    Ok(())
}

pub fn install() -> Result<()> {
    ensure_macos()?;
    let exe = std::env::current_exe()?.canonicalize()?;
    let home = config::home();
    std::fs::create_dir_all(&home)?;
    let mut env = format!(
        "<key>PATH</key><string>{}</string><key>HOME</key><string>{}</string>",
        xml(&std::env::var("PATH").unwrap_or_default()),
        xml(&config::user_home().display().to_string())
    );
    if std::env::var_os("AUGUST_HOME").is_some() {
        env += &format!("<key>AUGUST_HOME</key><string>{}</string>", xml(&home.display().to_string()));
    }
    let out = xml(&home.join("serve.out").display().to_string());
    let content = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>{LABEL}</string>
  <key>ProgramArguments</key><array><string>{}</string><string>serve</string></array>
  <key>EnvironmentVariables</key><dict>{env}</dict>
  <key>WorkingDirectory</key><string>{}</string>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>ThrottleInterval</key><integer>10</integer>
  <key>StandardOutPath</key><string>{out}</string>
  <key>StandardErrorPath</key><string>{out}</string>
</dict>
</plist>
"#,
        xml(&exe.display().to_string()),
        xml(&home.display().to_string()),
    );
    let path = plist();
    std::fs::create_dir_all(path.parent().unwrap())?;
    std::fs::write(&path, content)?;
    let target = target()?;
    // Replace a running copy (e.g. after rebuilding the binary).
    Command::new("launchctl").args(["bootout", &format!("{target}/{LABEL}")]).output().ok();
    let out = Command::new("launchctl").args(["bootstrap", &target]).arg(&path).output()?;
    if !out.status.success() {
        bail!("launchctl bootstrap failed: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(())
}

pub fn uninstall() -> Result<bool> {
    ensure_macos()?;
    let path = plist();
    if !path.exists() {
        return Ok(false);
    }
    Command::new("launchctl").args(["bootout", &format!("{}/{LABEL}", target()?)]).output().ok();
    std::fs::remove_file(path)?;
    Ok(true)
}

pub fn restart() -> Result<()> {
    ensure_macos()?;
    let out = Command::new("launchctl").args(["kickstart", "-k", &format!("{}/{LABEL}", target()?)]).output()?;
    if !out.status.success() {
        bail!("the service is not installed, run `august service install`");
    }
    Ok(())
}

/// "running (pid 123)", "installed, not running", or None when not installed.
pub fn status() -> Option<String> {
    if !cfg!(target_os = "macos") || !plist().exists() {
        return None;
    }
    let out = Command::new("launchctl").args(["print", &format!("{}/{LABEL}", target().ok()?)]).output().ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let pid = text.lines().find_map(|l| l.trim().strip_prefix("pid = ").map(str::to_string));
    Some(match pid {
        Some(pid) => format!("running (pid {pid})"),
        None => "installed, not running (see `august logs`)".into(),
    })
}
