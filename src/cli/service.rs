//! Runs August as a background service that starts at login and is restarted when it
//! crashes (not when it stops cleanly: `august stop`, `restart`): a launchd agent on macOS,
//! a systemd user unit on Linux. Or, without the service, on its own in the background.

use crate::config;
use anyhow::{Context, Result, bail};
use std::path::PathBuf;
use std::os::unix::process::CommandExt;
use std::process::Command;

const LABEL: &str = "dev.august.agent";

/// launchd label; `AUGUST_SERVICE_LABEL` lets tests install a separate service.
fn label() -> String {
    crate::util::env_or("AUGUST_SERVICE_LABEL", LABEL)
}
const UNIT: &str = "august.service";

/// Whether `august serve` installed the background service.
/// Whether `august serve` installed the background service for this `AUGUST_HOME` (one
/// installed for another home is not this August's to stop or restart).
pub fn installed() -> bool {
    let file = if cfg!(target_os = "macos") { plist_path() } else { unit_path() };
    let home = config::home().display().to_string();
    std::fs::read_to_string(file).is_ok_and(|f| f.contains(&format!("AUGUST_HOME</key><string>{}<", xml(&home))) || f.contains(&format!("AUGUST_HOME={home}\"")))
}

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
    home_dir().join("Library/LaunchAgents").join(format!("{}.plist", label()))
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
  <key>Label</key><string>{label}</string>
  <key>ProgramArguments</key><array><string>{exe}</string><string>gateway</string></array>
  <key>WorkingDirectory</key><string>{dir}</string>
  <key>EnvironmentVariables</key><dict><key>PATH</key><string>{path}</string><key>AUGUST_HOME</key><string>{home}</string></dict>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><dict><key>SuccessfulExit</key><false/></dict>
  <key>ThrottleInterval</key><integer>5</integer>
  <key>StandardOutPath</key><string>{log}</string>
  <key>StandardErrorPath</key><string>{log}</string>
</dict>
</plist>
"#,
            label = xml(&label()),
            exe = xml(&exe.to_string_lossy()),
            dir = xml(&dir.to_string_lossy()),
            path = xml(&path),
            log = xml(&log.to_string_lossy()),
            home = xml(&config::home().to_string_lossy()),
        );
        let file = plist_path();
        std::fs::create_dir_all(file.parent().unwrap())?;
        std::fs::write(&file, plist)?;
        let domain = format!("gui/{}", uid()?);
        // Reload if it is already installed. `bootout` returns before the old job is
        // gone, and `bootstrap` fails with "5: Input/output error" until it is.
        let target = format!("{domain}/{}", label());
        Command::new("launchctl").args(["bootout", &target]).output().ok();
        for _ in 0..50 {
            let loaded = Command::new("launchctl").args(["print", &target]).output().is_ok_and(|o| o.status.success());
            if !loaded {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(200));
        }
        run(Command::new("launchctl").args(["bootstrap", &domain]).arg(&file))?;
        run(Command::new("launchctl").args(["kickstart", &target]))?;
    } else {
        let unit = format!(
            "[Unit]\nDescription=August agent\nAfter=network-online.target\nWants=network-online.target\n\n\
             [Service]\nExecStart=\"{exe}\" gateway\nWorkingDirectory={dir}\nEnvironment=\"PATH={path}\"\nEnvironment=\"AUGUST_HOME={home}\"\n\
             Restart=on-failure\nRestartSec=5\nStandardOutput=append:{log}\nStandardError=append:{log}\n\n\
             [Install]\nWantedBy=default.target\n",
            exe = exe.display(),
            dir = dir.display(),
            log = log.display(),
            home = config::home().display(),
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

/// Starts the installed service again.
pub fn kick() -> Result<()> {
    if cfg!(target_os = "macos") {
        run(Command::new("launchctl").args(["kickstart", &format!("gui/{}/{}", uid()?, label())]))
    } else {
        run(Command::new("systemctl").args(["--user", "restart", UNIT]))
    }
}

/// Starts August on its own in the background (`august serve` keeps it running at login).
pub fn spawn() -> Result<()> {
    let log = log_path();
    std::fs::create_dir_all(log.parent().unwrap_or(&log))?;
    let out = std::fs::OpenOptions::new().create(true).append(true).open(&log)?;
    Command::new(std::env::current_exe()?)
        .arg("gateway")
        .stdin(std::process::Stdio::null())
        .stdout(out.try_clone()?)
        .stderr(out)
        // Its own process group, so closing this terminal doesn't stop it.
        .process_group(0)
        .spawn()
        .context("could not start August")?;
    Ok(())
}

/// Stops the service (if still running) and removes it from autostart.
pub fn uninstall() -> Result<()> {
    if cfg!(target_os = "macos") {
        let file = plist_path();
        if !file.exists() {
            bail!("service is not installed");
        }
        // Already stopped (`august stop` shut it down first) is fine.
        Command::new("launchctl").args(["bootout", &format!("gui/{}/{}", uid()?, label())]).output().ok();
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
