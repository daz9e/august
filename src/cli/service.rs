//! Runs the gateway as a background service that starts at login and is
//! restarted when it dies: a launchd agent on macOS, a systemd user unit on Linux.

use crate::config;
use anyhow::{Context, Result, bail};
use std::path::PathBuf;
use std::process::Command;

const LABEL: &str = "dev.august.agent";
const UNIT: &str = "august.service";

pub fn log_path() -> PathBuf {
    config::home().join("logs").join("august.log")
}

fn run(cmd: &mut Command) -> Result<()> {
    let out = cmd.output().with_context(|| format!("failed to run {:?}", cmd.get_program()))?;
    if !out.status.success() {
        bail!(
            "{:?} failed: {}",
            cmd.get_program(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

fn home_dir() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into()))
}

fn plist_path() -> PathBuf {
    home_dir().join("Library/LaunchAgents").join(format!("{LABEL}.plist"))
}

fn unit_path() -> PathBuf {
    home_dir().join(".config/systemd/user").join(UNIT)
}

fn xml(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

/// Installs the service for the current binary and working directory (where
/// `.env` and a relative `AUGUST_WORKSPACE` are resolved) and starts it.
pub fn start() -> Result<()> {
    let exe = std::env::current_exe()?.canonicalize()?;
    let dir = std::env::current_dir()?;
    let log = log_path();
    std::fs::create_dir_all(log.parent().unwrap())?;
    let path = std::env::var("PATH").unwrap_or_default();

    if cfg!(target_os = "macos") {
        let plist = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>{LABEL}</string>
  <key>ProgramArguments</key><array><string>{exe}</string><string>gateway</string></array>
  <key>WorkingDirectory</key><string>{dir}</string>
  <key>EnvironmentVariables</key><dict><key>PATH</key><string>{path}</string></dict>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>ThrottleInterval</key><integer>5</integer>
  <key>StandardOutPath</key><string>{log}</string>
  <key>StandardErrorPath</key><string>{log}</string>
</dict>
</plist>
"#,
            exe = xml(&exe.to_string_lossy()),
            dir = xml(&dir.to_string_lossy()),
            path = xml(&path),
            log = xml(&log.to_string_lossy()),
        );
        let file = plist_path();
        std::fs::create_dir_all(file.parent().unwrap())?;
        std::fs::write(&file, plist)?;
        let domain = format!("gui/{}", uid()?);
        // Reload if it is already installed.
        Command::new("launchctl").args(["bootout", &format!("{domain}/{LABEL}")]).output().ok();
        run(Command::new("launchctl").args(["bootstrap", &domain]).arg(&file))?;
        run(Command::new("launchctl").args(["kickstart", &format!("{domain}/{LABEL}")]))?;
    } else {
        let unit = format!(
            "[Unit]\nDescription=August agent\nAfter=network-online.target\nWants=network-online.target\n\n\
             [Service]\nExecStart=\"{exe}\" gateway\nWorkingDirectory={dir}\nEnvironment=\"PATH={path}\"\n\
             Restart=always\nRestartSec=5\nStandardOutput=append:{log}\nStandardError=append:{log}\n\n\
             [Install]\nWantedBy=default.target\n",
            exe = exe.display(),
            dir = dir.display(),
            log = log.display(),
        );
        let file = unit_path();
        std::fs::create_dir_all(file.parent().unwrap())?;
        std::fs::write(&file, unit)?;
        run(Command::new("systemctl").args(["--user", "daemon-reload"]))?;
        run(Command::new("systemctl").args(["--user", "enable", "--now", UNIT]))?;
        run(Command::new("systemctl").args(["--user", "restart", UNIT]))?;
        println!("to keep it running after logout: loginctl enable-linger $USER");
    }
    println!("august is running in the background (starts at login, restarts on crash)");
    println!("logs: {}   (august logs)\nstop: august stop", log.display());
    Ok(())
}

/// Stops the service and removes it from autostart.
pub fn stop() -> Result<()> {
    if cfg!(target_os = "macos") {
        let file = plist_path();
        if !file.exists() {
            bail!("service is not installed");
        }
        run(Command::new("launchctl").args(["bootout", &format!("gui/{}/{LABEL}", uid()?)]))?;
        std::fs::remove_file(file)?;
    } else {
        let file = unit_path();
        if !file.exists() {
            bail!("service is not installed");
        }
        run(Command::new("systemctl").args(["--user", "disable", "--now", UNIT]))?;
        std::fs::remove_file(file)?;
        run(Command::new("systemctl").args(["--user", "daemon-reload"]))?;
    }
    println!("august stopped and removed from autostart");
    Ok(())
}

/// Follows the service log.
pub fn logs() -> Result<()> {
    let log = log_path();
    if !log.exists() {
        bail!("no log yet: {}", log.display());
    }
    Command::new("tail").args(["-n", "100", "-f"]).arg(log).status()?;
    Ok(())
}

fn uid() -> Result<String> {
    let out = Command::new("id").arg("-u").output()?;
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}
