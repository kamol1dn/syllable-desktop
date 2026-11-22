//! Wallpaper: the local feed the HTML wallpaper connects to (`feed`), and the WebView2
//! host that shows that page on every monitor behind the desktop icons (`host`).
//!
//! The page itself is `assets/wallpaper/` in the workspace; `build.rs` copies it to
//! `assets/wallpaper/` beside the built exe, which is where the host loads it from.
//! `WallpaperHostEnabled` switches the desktop windows on and off live; the feed always
//! runs, so a browser tab or another host can show the page too.
//!
//! Debug builds only:
//! - `SYLLABLE_FAKE_MEDIA=1` publishes a made-up song (track, cover, word-timed lyrics,
//!   a running position and a bass level) so the page can be checked while the media,
//!   lyrics and audio modules are not there yet.
//! - `SYLLABLE_WEBVIEW_DEBUG_PORT=9223` opens Chromium's remote-debugging port on the
//!   wallpaper pages (and enables DevTools), for inspection with `examples/cdp.rs`.

mod feed;
mod host;
pub mod images;
pub mod protocol;

#[cfg(debug_assertions)]
mod fake;

use std::path::PathBuf;

use syllable_core::Context;

pub use images::categories;

/// Start the module. Returns once it is running: long-lived work goes on the
/// module's own thread(s), which stop when they see `Event::Quit` on the bus.
pub fn start(ctx: Context) -> anyhow::Result<()> {
    feed::start(ctx.clone());
    host::start(ctx.clone());
    #[cfg(debug_assertions)]
    if std::env::var_os("SYLLABLE_FAKE_MEDIA").is_some_and(|v| v != "0") {
        fake::start(ctx);
    }
    Ok(())
}

/// `assets\wallpaper\index.html` beside the exe; in debug builds also the workspace copy.
pub(crate) fn page_path() -> Option<PathBuf> {
    let beside = std::env::current_exe().ok()?.parent()?.join("assets").join("wallpaper").join("index.html");
    if beside.is_file() {
        return Some(beside);
    }
    #[cfg(debug_assertions)]
    {
        let dev = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..\\..\\assets\\wallpaper\\index.html");
        if let Ok(dev) = dev.canonicalize() {
            let dev = PathBuf::from(dev.to_string_lossy().trim_start_matches(r"\\?\"));
            return Some(dev);
        }
    }
    Some(beside)
}

pub(crate) fn debug_port() -> Option<u16> {
    #[cfg(debug_assertions)]
    {
        std::env::var("SYLLABLE_WEBVIEW_DEBUG_PORT").ok().and_then(|p| p.parse().ok())
    }
    #[cfg(not(debug_assertions))]
    {
        None
    }
}
