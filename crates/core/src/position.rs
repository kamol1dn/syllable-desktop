//! The playback clock: the latest known position, interpolated between updates.
//! Written by `syllable-media`, read by anything that draws in time with the music.

use std::sync::RwLock;
use std::time::Instant;

#[derive(Clone, Copy)]
struct Snap {
    base_ms: f64,
    at: Instant,
    playing: bool,
    rate: f64,
}

static SNAP: RwLock<Option<Snap>> = RwLock::new(None);

pub fn set(position_ms: f64, playing: bool, rate: f64) {
    let rate = if rate <= 0.0 { 1.0 } else { rate };
    *SNAP.write().unwrap() = Some(Snap { base_ms: position_ms, at: Instant::now(), playing, rate });
}

pub fn clear() {
    *SNAP.write().unwrap() = None;
}

pub fn playing() -> bool {
    SNAP.read().unwrap().map(|s| s.playing).unwrap_or(false)
}

/// The rate the clock is running at: the player's rate, briefly slowed while the
/// clock absorbs a small backward correction. 1.0 when nothing is playing.
pub fn rate() -> f64 {
    SNAP.read().unwrap().map(|s| s.rate).unwrap_or(1.0)
}

/// Current position in ms, or `None` when nothing is playing.
pub fn now_ms() -> Option<f64> {
    let s = (*SNAP.read().unwrap())?;
    if !s.playing {
        return Some(s.base_ms);
    }
    Some(s.base_ms + s.at.elapsed().as_secs_f64() * 1000.0 * s.rate)
}
