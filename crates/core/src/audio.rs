//! The latest audio analysis frame. Written by `syllable-audio` many times a
//! second, read by the visualizer and the wallpaper feed when they draw.

use std::sync::{Arc, Condvar, Mutex, RwLock};

#[derive(Clone, Debug, Default)]
pub struct AudioFrame {
    /// Spectrum magnitudes, low to high frequency, each 0..1.
    pub bands: Vec<f32>,
    /// Bass energy, 0..1.
    pub bass: f32,
    /// Overall level, 0..1.
    pub level: f32,
}

static FRAME: RwLock<Option<Arc<AudioFrame>>> = RwLock::new(None);

pub fn set(frame: AudioFrame) {
    *FRAME.write().unwrap() = Some(Arc::new(frame));
}

/// `None` until the audio module has produced a frame (or when it is off).
pub fn latest() -> Option<Arc<AudioFrame>> {
    FRAME.read().unwrap().clone()
}

/// Forget the latest frame: the audio module calls this when it stops capturing,
/// so readers see `None` ("audio is off") instead of a frozen spectrum.
pub fn clear() {
    *FRAME.write().unwrap() = None;
}

// ---- demand: who needs audio analysis right now ----
//
// Capture costs CPU and keeps the audio device open, so `syllable-audio` only runs
// while at least one consumer has asked for it. A consumer calls
// `audio::demand("visualizer", true)` when it starts drawing and
// `audio::demand("visualizer", false)` when it stops; names are free-form but must
// be stable per consumer. Calls are idempotent.

static DEMAND: Mutex<Vec<&'static str>> = Mutex::new(Vec::new());
static DEMAND_CHANGED: Condvar = Condvar::new();

/// Ask for (or release) audio analysis on behalf of `who`.
pub fn demand(who: &'static str, on: bool) {
    let mut list = DEMAND.lock().unwrap();
    let had = list.contains(&who);
    if on && !had {
        list.push(who);
    } else if !on && had {
        list.retain(|w| *w != who);
    } else {
        return;
    }
    DEMAND_CHANGED.notify_all();
}

/// True while any consumer has asked for audio.
pub fn demanded() -> bool {
    !DEMAND.lock().unwrap().is_empty()
}

/// Block until `demanded()` differs from `current` or `timeout` passes; returns the
/// new state. Used by the audio module to sleep at zero CPU while nobody listens.
pub fn wait_demand(current: bool, timeout: std::time::Duration) -> bool {
    let list = DEMAND.lock().unwrap();
    let (list, _) = DEMAND_CHANGED
        .wait_timeout_while(list, timeout, |l| !l.is_empty() == current)
        .unwrap();
    !list.is_empty()
}
