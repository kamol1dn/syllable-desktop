//! Run the wallpaper module on its own (feed + desktop host), for testing while the
//! rest of the app is being built. Stops after the given number of seconds.
//! With `next` as a second argument it publishes `Event::NextWallpaper` every 5 s.
//!
//!     set SYLLABLE_FAKE_MEDIA=1
//!     cargo run -p syllable-wallpaper --example run -- 60 [next]

use std::time::{Duration, Instant};

use syllable_core::{Context, Event};

fn main() {
    let mut args = std::env::args().skip(1);
    let secs: u64 = args.next().and_then(|a| a.parse().ok()).unwrap_or(60);
    let next = args.next().is_some_and(|a| a == "next");
    let ctx = Context::new();
    syllable_wallpaper::start(ctx.clone()).expect("start");
    let end = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < end {
        std::thread::sleep(Duration::from_secs(5).min(end.saturating_duration_since(Instant::now())));
        if next && Instant::now() < end {
            ctx.bus.publish(Event::NextWallpaper);
        }
    }
    ctx.bus.publish(Event::Quit);
    std::thread::sleep(Duration::from_millis(500));
}
