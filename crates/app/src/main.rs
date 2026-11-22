//! The executable: loads the config, starts every module, owns the tray icon and
//! the main thread's message loop.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

#[cfg(feature = "guard-heap")]
mod guard_heap;
mod tray;

// Only in builds made with `cargo build --features guard-heap`, and then only with
// `SYLLABLE_GUARD_HEAP=1` (see guard_heap.rs). Shipped builds use the system allocator.
#[cfg(feature = "guard-heap")]
#[global_allocator]
static ALLOC: guard_heap::GuardHeap = guard_heap::GuardHeap;

use syllable_core::{log, Context, Event};

fn main() {
    let Some(_instance) = tray::single_instance() else {
        return; // already running
    };

    log!("---- starting {} ----", env!("CARGO_PKG_VERSION"));
    let ctx = Context::new();

    // Each module is independent: one failing to start must not take the rest down.
    type Start = fn(Context) -> anyhow::Result<()>;
    let modules: &[(&str, Start)] = &[
        ("media", syllable_media::start),
        ("audio", syllable_audio::start),
        ("lyrics", syllable_lyrics::start),
        ("wallpaper", syllable_wallpaper::start),
        ("taskbar", syllable_taskbar::start),
        ("visualizer", syllable_visualizer::start),
        ("settings", syllable_settings::start),
    ];
    // Debug aid: `SYLLABLE_MODULES=media,taskbar` starts only the named modules,
    // `SYLLABLE_MODULES=-wallpaper` starts all but the named ones.
    let only = std::env::var("SYLLABLE_MODULES").unwrap_or_default();
    let names: Vec<&str> = only.split(',').map(str::trim).filter(|s| !s.is_empty()).collect();
    let wanted = |name: &str| {
        let excluded = names.iter().any(|n| n.strip_prefix('-') == Some(name));
        let included = names.iter().filter(|n| !n.starts_with('-')).count() == 0 || names.contains(&name);
        included && !excluded
    };
    for (name, start) in modules.iter().filter(|(name, _)| wanted(name)) {
        if let Err(e) = start(ctx.clone()) {
            log!("{name}: failed to start: {e:#}");
        }
    }

    if let Err(e) = tray::run(&ctx) {
        log!("tray: {e:#}");
    }

    ctx.bus.publish(Event::Quit);
    // Give module threads a moment to tear their windows down.
    std::thread::sleep(std::time::Duration::from_millis(200));
    log!("---- exited ----");
}
