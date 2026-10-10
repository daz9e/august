//! Small helpers shared across layers.

/// Random UUID v4, e.g. for session and host ids.
pub fn new_uuid() -> String {
    let mut b = [0u8; 16];
    getrandom::fill(&mut b).expect("os rng");
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!("{}-{}-{}-{}-{}", &h[0..8], &h[8..12], &h[12..16], &h[16..20], &h[20..32])
}

/// Env var value, or `default` when unset or empty.
pub fn env_or(key: &str, default: &str) -> String {
    std::env::var(key)
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| default.to_string())
}

/// PATH for the gateway: its own, then what the user's login shell adds. A background service
/// starts with launchd's short PATH; this way what runs in the user's terminal runs here too.
pub fn login_path() -> String {
    let own = std::env::var("PATH").unwrap_or_default();
    let Ok(shell) = std::env::var("SHELL") else { return own };
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let out = std::process::Command::new(shell).args(["-lc", "printf %s \"$PATH\""]).stdin(std::process::Stdio::null()).output();
        tx.send(out.ok().filter(|o| o.status.success()).map(|o| String::from_utf8_lossy(&o.stdout).into_owned())).ok();
    });
    // A profile that hangs must not keep August from starting.
    let Ok(Some(theirs)) = rx.recv_timeout(std::time::Duration::from_secs(3)) else { return own };
    let mut dirs: Vec<&str> = own.split(':').filter(|d| !d.is_empty()).collect();
    for d in theirs.split(':') {
        if !d.is_empty() && !dirs.contains(&d) {
            dirs.push(d);
        }
    }
    dirs.join(":")
}
