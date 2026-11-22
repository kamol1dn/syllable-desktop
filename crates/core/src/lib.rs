//! Shared contracts for every module: data models, config, the event bus, the
//! playback clock, the audio-level slot and the log. Module crates depend on this
//! crate only, never on each other; `crates/app` wires them together.

pub mod accounts;
pub mod audio;
pub mod bus;
pub mod config;
pub mod log;
pub mod matching;
pub mod models;
pub mod position;

pub use bus::{Bus, Event};
pub use config::{Config, ConfigStore, WallpaperSettings};
pub use models::{BgVocal, LyricLine, Lyrics, Seg, SyncType, TrackInfo};

/// Everything a module needs to take part: cheap to clone, safe to move to any thread.
#[derive(Clone)]
pub struct Context {
    pub bus: Bus,
    pub config: ConfigStore,
}

impl Context {
    pub fn new() -> Self {
        let bus = Bus::new();
        let config = ConfigStore::load(bus.clone());
        Self { bus, config }
    }
}

impl Default for Context {
    fn default() -> Self {
        Self::new()
    }
}

/// Write a line to the app log: `log!("resolve: {} ok", provider)`.
#[macro_export]
macro_rules! log {
    ($($arg:tt)*) => { $crate::log::write(&format!($($arg)*)) };
}
