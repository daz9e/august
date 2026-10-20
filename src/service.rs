//! Autostart for `august serve`: a launchd agent on macOS, a systemd user unit on Linux.
//! Both restart it whenever it exits, which self-update and its rollback rely on.

use crate::config;
use anyhow::{Result, bail};
use std::path::PathBuf;
use std::process::Command;

pub const LABEL: &str = "com.august.agent";
const UNIT: &str = "august.service";

/// Whether this process is the service (set in its environment by `install`).
pub fn is_self() -> bool {
    std::env::var_os("AUGUST_SERVICE").is_some() || std::env::var("XPC_SERVICE_NAME").is_ok_and(|n| n == LABEL)
}

fn ensure_supported() -> Result<()> {
    if !cfg!(any(target_os = "macos", target_os = "linux")) {
        bail!("autostart is set up for macOS (launchd) and Linux (systemd) only; run `august serve` from your init system");
    }
    Ok(())
}

/// What the service needs in its environment.
fn env() -> Vec<(&'static str, String)> {
    let mut env = vec![
        ("PATH", std::env::var("PATH").unwrap_or_default()),
        ("HOME", config::user_home().display().to_string()),
        ("AUGUST_SERVICE", "1".into()),
    ];
    if std::env::var_os("AUGUST_HOME").is_some() {
        env.push(("AUGUST_HOME", config::home().display().to_string()));
    }
    env
}

/// Installs and starts the service; returns a warning for the user, if any.
pub fn install() -> Result<Option<String>> {
    ensure_supported()?;
    let exe = std::env::current_exe()?.canonicalize()?;
    std::fs::create_dir_all(config::home())?;
    if cfg!(target_os = "macos") {
        launchd::install(&exe).map(|()| None)
    } else {
        systemd::install(&exe)
    }
}

pub fn uninstall() -> Result<bool> {
    ensure_supported()?;
    if cfg!(target_os = "macos") { launchd::uninstall() } else { systemd::uninstall() }
}

pub fn restart() -> Result<()> {
    ensure_supported()?;
    if cfg!(target_os = "macos") { launchd::restart() } else { systemd::restart() }
}

/// "running (pid 123)", "installed, not running", or None when not installed.
pub fn status() -> Option<String> {
    let pid = if cfg!(target_os = "macos") {
        launchd::pid()?
    } else if cfg!(target_os = "linux") {
        systemd::pid()?
    } else {
        return None;
    };
    Some(match pid {
        Some(pid) => format!("running (pid {pid})"),
        None => "installed, not running (see `august logs`)".into(),
    })
}

fn check(out: std::process::Output, what: &str) -> Result<()> {
    if !out.status.success() {
        bail!("{what} failed: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(())
}

mod launchd {
    use super::*;
    use std::path::Path;

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

    pub fn install(exe: &Path) -> Result<()> {
        let home = config::home();
        let env: String = env().iter().map(|(k, v)| format!("<key>{k}</key><string>{}</string>", xml(v))).collect();
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
        check(Command::new("launchctl").args(["bootstrap", &target]).arg(&path).output()?, "launchctl bootstrap")
    }

    pub fn uninstall() -> Result<bool> {
        let path = plist();
        if !path.exists() {
            return Ok(false);
        }
        Command::new("launchctl").args(["bootout", &format!("{}/{LABEL}", target()?)]).output().ok();
        std::fs::remove_file(path)?;
        Ok(true)
    }

    pub fn restart() -> Result<()> {
        let out = Command::new("launchctl").args(["kickstart", "-k", &format!("{}/{LABEL}", target()?)]).output()?;
        if !out.status.success() {
            bail!("the service is not installed, run `august service install`");
        }
        Ok(())
    }

    /// None when not installed; Some(None) when installed but not running.
    pub fn pid() -> Option<Option<String>> {
        if !plist().exists() {
            return None;
        }
        let out = Command::new("launchctl").args(["print", &format!("{}/{LABEL}", target().ok()?)]).output().ok()?;
        let text = String::from_utf8_lossy(&out.stdout);
        Some(text.lines().find_map(|l| l.trim().strip_prefix("pid = ").map(str::to_string)))
    }
}

mod systemd {
    use super::*;
    use std::path::Path;

    fn unit() -> PathBuf {
        let config = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from).unwrap_or_else(|| config::user_home().join(".config"));
        config.join("systemd/user").join(UNIT)
    }

    fn systemctl(args: &[&str]) -> Result<std::process::Output> {
        Command::new("systemctl").arg("--user").args(args).output().map_err(|e| anyhow::anyhow!("cannot run systemctl ({e}); autostart on Linux needs systemd"))
    }

    /// systemd unit files take C-style quoted strings; `%` starts a specifier.
    fn quote(s: &str) -> String {
        format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\"").replace('%', "%%"))
    }

    pub fn install(exe: &Path) -> Result<Option<String>> {
        let home = config::home();
        let out = home.join("serve.out").display().to_string().replace('%', "%%");
        let env: String = env().iter().map(|(k, v)| format!("Environment={}\n", quote(&format!("{k}={v}")))).collect();
        // Restart=always with a pause: the service exits to update itself, and the pause keeps
        // systemd's start limit from giving up during a rollback.
        let content = format!(
            "[Unit]\nDescription=August agent\nWants=network-online.target\nAfter=network-online.target\n\n\
             [Service]\nExecStart={} serve\nWorkingDirectory={}\n{env}Restart=always\nRestartSec=10\n\
             StandardOutput=append:{out}\nStandardError=append:{out}\n\n\
             [Install]\nWantedBy=default.target\n",
            quote(&exe.display().to_string()),
            home.display().to_string().replace('%', "%%"),
        );
        let path = unit();
        std::fs::create_dir_all(path.parent().unwrap())?;
        std::fs::write(&path, content)?;
        check(systemctl(&["daemon-reload"])?, "systemctl --user daemon-reload")?;
        check(systemctl(&["enable", UNIT])?, "systemctl --user enable")?;
        // Restart, not start, so a running copy picks up the new unit.
        check(systemctl(&["restart", UNIT])?, "systemctl --user restart")?;
        // Without lingering, user services stop at logout and only start at login.
        let user = std::env::var("USER").unwrap_or_default();
        if Path::new("/var/lib/systemd/linger").join(&user).exists() {
            return Ok(None);
        }
        let linger = Command::new("loginctl").args(["enable-linger", &user]).output();
        Ok(match linger {
            Ok(o) if o.status.success() => None,
            _ => Some(format!("Could not enable lingering, so the service stops when you log out. Run: sudo loginctl enable-linger {user}")),
        })
    }

    pub fn uninstall() -> Result<bool> {
        let path = unit();
        if !path.exists() {
            return Ok(false);
        }
        systemctl(&["disable", "--now", UNIT]).ok();
        std::fs::remove_file(path)?;
        systemctl(&["daemon-reload"]).ok();
        Ok(true)
    }

    pub fn restart() -> Result<()> {
        if !unit().exists() {
            bail!("the service is not installed, run `august service install`");
        }
        check(systemctl(&["restart", UNIT])?, "systemctl --user restart")
    }

    /// None when not installed; Some(None) when installed but not running.
    pub fn pid() -> Option<Option<String>> {
        if !unit().exists() {
            return None;
        }
        let out = systemctl(&["show", UNIT, "--property=MainPID", "--value"]).ok()?;
        let pid = String::from_utf8_lossy(&out.stdout).trim().to_string();
        Some((!pid.is_empty() && pid != "0").then_some(pid))
    }
}
