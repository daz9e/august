//! Each extension's log, kept by the core in `logs/extensions/<name>.log` (not in the
//! extension's folder: that is the extension's own, and a log it can rewrite tells nothing).
//! Its stderr, and the core's notes about it marked `[august]`; rotated at 1 MB, one old file kept.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

const MAX_BYTES: u64 = 1024 * 1024;

pub struct Log {
    path: PathBuf,
    file: Mutex<Option<File>>,
}

fn path(home: &Path, name: &str) -> PathBuf {
    home.join("logs/extensions").join(format!("{name}.log"))
}

impl Log {
    /// The log of extension `name` of the August at `home`.
    pub fn new(home: &Path, name: &str) -> Self {
        Self { path: path(home, name), file: Mutex::new(None) }
    }

    /// A line the extension wrote.
    pub fn line(&self, text: &str) {
        let stamp = chrono::Local::now().format("%Y-%m-%d %H:%M:%S");
        let mut file = self.file.lock().unwrap();
        if file.is_none() || std::fs::metadata(&self.path).is_ok_and(|m| m.len() > MAX_BYTES) {
            if std::fs::metadata(&self.path).is_ok_and(|m| m.len() > MAX_BYTES) {
                std::fs::rename(&self.path, self.path.with_extension("log.1")).ok();
            }
            std::fs::create_dir_all(self.path.parent().unwrap()).ok();
            *file = OpenOptions::new().create(true).append(true).open(&self.path).ok();
        }
        if let Some(f) = file.as_mut() {
            writeln!(f, "{stamp} {text}").ok();
        }
    }

    /// A note of the core about the extension.
    pub fn note(&self, text: &str) {
        self.line(&format!("[august] {text}"));
    }
}

/// The last `lines` lines of `name`'s log (reaching into the rotated file if needed).
pub fn tail(home: &Path, name: &str, lines: usize) -> String {
    let p = path(home, name);
    let read = |p: &PathBuf| std::fs::read_to_string(p).unwrap_or_default();
    let mut all: Vec<String> = read(&p.with_extension("log.1")).lines().chain(read(&p).lines()).map(String::from).collect();
    let skip = all.len().saturating_sub(lines);
    all.drain(..skip);
    all.join("\n")
}
