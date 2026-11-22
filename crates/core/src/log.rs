//! Append-only text log in the app's data folder. Never fails loudly.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::config::data_dir;

static LOCK: Mutex<()> = Mutex::new(());
const MAX_BYTES: u64 = 512 * 1024;

pub fn path() -> std::path::PathBuf {
    data_dir().join("log.txt")
}

pub fn write(msg: &str) {
    let _guard = LOCK.lock();
    let path = path();
    let _ = fs::create_dir_all(data_dir());
    if fs::metadata(&path).map(|m| m.len() > MAX_BYTES).unwrap_or(false) {
        let _ = fs::remove_file(&path);
    }
    // UTC time of day; good enough to order events without a date/time dependency.
    let secs = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let (h, m, s) = ((secs / 3600) % 24, (secs / 60) % 60, secs % 60);
    if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(&path) {
        let _ = writeln!(file, "{h:02}:{m:02}:{s:02}Z {msg}");
    }
    #[cfg(debug_assertions)]
    eprintln!("[syllable] {msg}");
}
