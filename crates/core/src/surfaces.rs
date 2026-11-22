//! Whether the lock-screen page is showing, for Settings.
//!
//! The lock screen is drawn by a third-party host (Aura Wallpaper) that loads our page
//! from the local feed, so Settings needs evidence that it is actually connected.
//! `syllable-wallpaper`'s feed calls `lock_page_opened` / `lock_page_closed` and then
//! publishes `Event::SurfacesChanged`; `syllable-settings` reads `lock_status`.
//!
//! "Last seen" is kept in `<data_dir>\lock-seen.txt` (Unix seconds) so it survives restarts.

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static OPEN: AtomicUsize = AtomicUsize::new(0);
/// Unix seconds; 0 = not loaded from disk yet / never seen.
static LAST_SEEN: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LockStatus {
    /// Lock-screen pages connected right now.
    pub open: usize,
    /// When a lock-screen page was last connected (Unix seconds), if ever.
    pub last_seen: Option<u64>,
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn file() -> std::path::PathBuf {
    crate::config::data_dir().join("lock-seen.txt")
}

fn touch() {
    let t = now();
    LAST_SEEN.store(t, Ordering::SeqCst);
    let _ = std::fs::write(file(), t.to_string());
}

pub fn lock_page_opened() {
    OPEN.fetch_add(1, Ordering::SeqCst);
    touch();
}

pub fn lock_page_closed() {
    let mut cur = OPEN.load(Ordering::SeqCst);
    while cur > 0 {
        match OPEN.compare_exchange_weak(cur, cur - 1, Ordering::SeqCst, Ordering::SeqCst) {
            Ok(_) => break,
            Err(now) => cur = now,
        }
    }
    touch();
}

pub fn lock_status() -> LockStatus {
    let mut seen = LAST_SEEN.load(Ordering::SeqCst);
    if seen == 0 {
        seen = std::fs::read_to_string(file()).ok().and_then(|s| s.trim().parse().ok()).unwrap_or(0);
        if seen != 0 {
            let _ = LAST_SEEN.compare_exchange(0, seen, Ordering::SeqCst, Ordering::SeqCst);
        }
    }
    LockStatus { open: OPEN.load(Ordering::SeqCst), last_seen: (seen != 0).then_some(seen) }
}
