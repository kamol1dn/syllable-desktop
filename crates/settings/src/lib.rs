//! Settings window and style presets.
//!
//! The window is an HTML/CSS/JS page (`native/assets/settings/`, compiled into the
//! binary) hosted in WebView2 on this module's own thread. It opens on
//! `Event::OpenSettings`, edits the config through `ConfigStore::update` (so every
//! change is saved and announced on the bus), and redraws on every `ConfigChanged`.
//! Styles (presets) are data in `presets.rs`. See `bridge.rs` for the page protocol.

mod accounts;
mod actions;
mod bridge;
mod placement;
pub mod presets;
mod window;

use syllable_core::Context;

/// Start the module. Returns once it is running: the window lives on its own thread,
/// which stops when it sees `Event::Quit` on the bus.
///
/// Debug builds: `SYLLABLE_OPEN_SETTINGS=1` opens the window at start-up, and
/// `SYLLABLE_OPEN_SETTINGS=<section>` opens it on that section
/// (taskbar | desktop | lock | lyrics | about).
pub fn start(ctx: Context) -> anyhow::Result<()> {
    let events = ctx.bus.subscribe();
    let open_now = if cfg!(debug_assertions) { std::env::var("SYLLABLE_OPEN_SETTINGS").ok() } else { None };
    std::thread::Builder::new()
        .name("settings".into())
        .spawn(move || window::run(ctx, events, open_now))?;
    Ok(())
}

const PAGE: &str = include_str!("../../../assets/settings/index.html");
const STYLE: &str = include_str!("../../../assets/settings/style.css");
const SCRIPT: &str = include_str!("../../../assets/settings/app.js");

/// The page with its stylesheet and script inlined, so it loads from memory with
/// nothing on disk beside the exe.
pub(crate) fn page_html() -> String {
    PAGE.replace(r#"<link rel="stylesheet" href="style.css">"#, &format!("<style>\n{STYLE}\n</style>"))
        .replace(r#"<script src="app.js"></script>"#, &format!("<script>\n{SCRIPT}\n</script>"))
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_page_is_self_contained() {
        let html = super::page_html();
        assert!(!html.contains(r#"href="style.css""#) && !html.contains(r#"src="app.js""#));
        assert!(!super::SCRIPT.contains("</script"), "would end the inlined script early");
        assert!(html.len() < 1_500_000, "NavigateToString takes at most 2 MB");
    }
}
