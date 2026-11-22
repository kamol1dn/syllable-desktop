//! Resolve lyrics for a song through the real module, without the media module:
//!
//!     cargo run -p syllable-lyrics --example resolve -- "Blinding Lights" "The Weeknd" 200040
//!
//! Uses a temporary data folder (cache, secrets) unless `SYLLABLE_LYRICS_DIR` is set,
//! so nothing configured: this exercises the LRCLIB path. Also checks that a second
//! request is a cache hit and that skipping cancels the earlier lookup.

use std::sync::Arc;
use std::time::{Duration, Instant};

use syllable_core::{ConfigStore, Context, Event, TrackInfo};

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let title = args.first().cloned().unwrap_or_else(|| "Blinding Lights".into());
    let artist = args.get(1).cloned().unwrap_or_else(|| "The Weeknd".into());
    let duration_ms: f64 = args.get(2).and_then(|d| d.parse().ok()).unwrap_or(200_040.0);

    let dir = std::env::var_os("SYLLABLE_LYRICS_DIR")
        .map(Into::into)
        .unwrap_or_else(|| std::env::temp_dir().join(format!("syllable-lyrics-example-{}", std::process::id())));
    let bus = syllable_core::Bus::new();
    let ctx = Context { bus: bus.clone(), config: ConfigStore::load_from(dir.join("config.json"), bus.clone()) };
    let events = bus.subscribe();
    syllable_lyrics::start_in(ctx, dir.clone())?;

    let track = |t: &str, a: &str, d: f64| {
        Arc::new(TrackInfo { title: t.into(), artist: a.into(), duration_ms: d, source_app: "example".into(), ..Default::default() })
    };
    let wait = |label: &str| -> Option<Arc<syllable_core::Lyrics>> {
        let start = Instant::now();
        loop {
            match events.recv_timeout(Duration::from_secs(40)) {
                Ok(Event::Lyrics(l)) => {
                    println!(
                        "{label}: {} after {} ms",
                        l.as_ref().map(|l| format!("{} lines from {}", l.lines.len(), l.source)).unwrap_or("none".into()),
                        start.elapsed().as_millis()
                    );
                    return l;
                }
                Ok(_) => {}
                Err(_) => {
                    println!("{label}: timed out");
                    return None;
                }
            }
        }
    };

    // Rapid skip: the first lookup must be cancelled; only the second song's lyrics arrive.
    bus.publish(Event::TrackChanged(Some(track("Some Skipped Song", "Nobody In Particular", 180_000.0))));
    std::thread::sleep(Duration::from_millis(100));
    bus.publish(Event::TrackChanged(Some(track(&title, &artist, duration_ms))));
    let first = wait("first lookup");
    if let Some(l) = &first {
        for line in l.lines.iter().take(3) {
            println!("  [{:>7.0} ms] {}", line.start, line.text);
        }
    }

    bus.publish(Event::TrackChanged(Some(track(&title, &artist, duration_ms))));
    let second = wait("second lookup (cache)");
    assert_eq!(first, second, "the cache returns what was resolved");

    bus.publish(Event::TrackChanged(None));
    assert!(wait("nothing playing").is_none());
    bus.publish(Event::Quit);
    std::thread::sleep(Duration::from_millis(200));
    if std::env::var_os("SYLLABLE_LYRICS_DIR").is_none() {
        let _ = std::fs::remove_dir_all(&dir);
    }
    println!("log: {}", syllable_core::log::path().display());
    Ok(())
}
