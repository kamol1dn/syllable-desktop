//! Whether the built-in lyrics wallpaper is in view on the main screen right now:
//! the host is running with its page loaded there, and the desktop shows (Show desktop,
//! or no window covering a real part of the screen). `syllable-wallpaper`'s host
//! writes it and publishes `Event::WallpaperInView`; the taskbar reads it to fade its
//! lyrics out when the wallpaper already shows them (`LyricsHideOnDesktop`).

use std::sync::atomic::{AtomicBool, Ordering};

static IN_VIEW: AtomicBool = AtomicBool::new(false);

/// Record the new state; returns whether it changed.
pub fn set(in_view: bool) -> bool {
    IN_VIEW.swap(in_view, Ordering::SeqCst) != in_view
}

pub fn in_view() -> bool {
    IN_VIEW.load(Ordering::SeqCst)
}
