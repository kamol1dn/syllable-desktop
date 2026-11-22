//! Diagnostics, off unless asked for by environment variable:
//!
//! - `SYLLABLE_TASKBAR_STATS=1`: every 5 s, log loop wake-ups, frames drawn, time per
//!   frame (drawing and the layered-window update) and what woke the loop.
//! - `SYLLABLE_TASKBAR_DUMP=<dir>`: save the lyric strip's own bitmap (premultiplied
//!   BGRA, `lyrics-NNNN.bmp`) every 250 ms, and keep drawing while a full-screen app
//!   covers the taskbar. Lets the rendering be checked without screen captures.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering::Relaxed};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use syllable_core::log;

pub fn stats_on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("SYLLABLE_TASKBAR_STATS").is_some())
}

pub fn dump_dir() -> Option<&'static PathBuf> {
    static DIR: OnceLock<Option<PathBuf>> = OnceLock::new();
    DIR.get_or_init(|| std::env::var_os("SYLLABLE_TASKBAR_DUMP").map(PathBuf::from)).as_ref()
}

static FRAMES: AtomicU32 = AtomicU32::new(0);
static DRAW_US: AtomicU32 = AtomicU32::new(0);
static PRESENT_US: AtomicU32 = AtomicU32::new(0);
static TICKS: AtomicU32 = AtomicU32::new(0);
static RELAYOUTS: AtomicU32 = AtomicU32::new(0);
static WAKES: [AtomicU32; 5] = [const { AtomicU32::new(0) }; 5];

pub fn frame(draw: Duration, present: Duration) {
    if stats_on() {
        FRAMES.fetch_add(1, Relaxed);
        DRAW_US.fetch_add(draw.as_micros() as u32, Relaxed);
        PRESENT_US.fetch_add(present.as_micros() as u32, Relaxed);
    }
}

pub fn tick(relayout: bool) {
    if stats_on() {
        TICKS.fetch_add(1, Relaxed);
        if relayout {
            RELAYOUTS.fetch_add(1, Relaxed);
        }
    }
}

/// `kind`: 0 taskbar created, 1 taskbar moved, 2 foreground, 3 display, 4 wake message.
pub fn signal(kind: usize) {
    if stats_on() {
        WAKES[kind.min(4)].fetch_add(1, Relaxed);
    }
}

/// Counts loop iterations and reports every 5 s.
pub struct Stats {
    since: Instant,
    loops: u32,
}

impl Stats {
    pub fn new() -> Self {
        Self { since: Instant::now(), loops: 0 }
    }

    pub fn looped(&mut self) {
        if !stats_on() {
            return;
        }
        self.loops += 1;
        let secs = self.since.elapsed().as_secs_f32();
        if secs < 5.0 {
            return;
        }
        let frames = FRAMES.swap(0, Relaxed).max(1) as f32;
        let wakes: Vec<u32> = WAKES.iter().map(|c| c.swap(0, Relaxed)).collect();
        log!(
            "taskbar: stats: {:.0} wake-ups/s, {:.0} frame ticks/s ({} layout passes), {:.0} frames drawn/s, \
             {:.2} ms drawing + {:.2} ms presenting per frame; signals created/moved/foreground/display/wake {:?}",
            self.loops as f32 / secs,
            TICKS.swap(0, Relaxed) as f32 / secs,
            RELAYOUTS.swap(0, Relaxed),
            frames / secs,
            DRAW_US.swap(0, Relaxed) as f32 / 1000.0 / frames,
            PRESENT_US.swap(0, Relaxed) as f32 / 1000.0 / frames,
            wakes
        );
        *self = Self::new();
    }
}
