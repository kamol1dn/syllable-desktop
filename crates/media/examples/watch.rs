//! Live check of the media module (not part of the app).
//!
//!     cargo run -p syllable-media --example watch -- [seconds] [--drive]
//!
//! Starts the module, prints every bus event, and every 250 ms prints the shared
//! clock next to an independent reading of the same SMTC session. Whenever the
//! player publishes a fresh timeline sample (its own statement of where it is),
//! the line is marked FRESH and `d` is how far our clock is from it.
//! Each line carries the wall time in unix ms so it can be lined up with other
//! sources (a browser's `video.currentTime`).
//!
//! `--drive` controls the session through SMTC: play at 1 s, a minute of plain
//! playback, pause at 62 s, resume at 66 s, seek to 60 s at 72 s, seek back to 20 s
//! at 80 s, next track at 88 s, pause again at 100 s (run it for 102 s).
//! `--drive --short`: play at 1 s, next track at 5 s, pause at 12 s.
//! `--spotify`: use the Spotify session for driving and comparing.

use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use syllable_core::{position, Context, Event};
use windows::Media::Control::{
    GlobalSystemMediaTransportControlsSession as Session,
    GlobalSystemMediaTransportControlsSessionManager as Manager,
    GlobalSystemMediaTransportControlsSessionPlaybackStatus as Status,
};

fn wall_ms() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs_f64() * 1000.0
}

fn playing(s: &Session) -> bool {
    s.GetPlaybackInfo().and_then(|p| p.PlaybackStatus()).map(|x| x == Status::Playing).unwrap_or(false)
}

fn pick(mgr: &Manager) -> Option<Session> {
    let cur = mgr.GetCurrentSession().ok();
    if cur.as_ref().is_some_and(playing) {
        return cur;
    }
    let all: Vec<Session> = mgr.GetSessions().ok()?.into_iter().collect();
    let spotify = |s: &Session| s.SourceAppUserModelId().map(|a| a.to_string().to_lowercase().contains("spotify")).unwrap_or(false);
    all.iter().find(|s| playing(s) && spotify(s)).or_else(|| all.iter().find(|s| playing(s))).cloned().or(cur)
}

/// (raw position, last updated unix ms, estimate now)
fn smtc(s: &Session) -> Option<(f64, f64, f64)> {
    let tl = s.GetTimelineProperties().ok()?;
    let pos = (tl.Position().ok()?.Duration - tl.StartTime().ok()?.Duration) as f64 / 10_000.0;
    let upd = (tl.LastUpdatedTime().ok()?.UniversalTime - 116_444_736_000_000_000) as f64 / 10_000.0;
    let est = if playing(s) { pos + (wall_ms() - upd) } else { pos };
    Some((pos, upd, est))
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let secs: u64 = args.iter().skip(1).find_map(|a| a.parse().ok()).unwrap_or(70);
    let drive = args.iter().any(|a| a == "--drive");
    // `--spotify`: drive and compare against the Spotify session, whatever else plays.
    let only_spotify = args.iter().any(|a| a == "--spotify");

    let ctx = Context::new();
    let events = ctx.bus.subscribe();
    syllable_media::start(ctx.clone()).unwrap();
    let mgr = Manager::RequestAsync().unwrap().join().unwrap();

    let t0 = Instant::now();
    let mut last_raw = None;
    let mut fresh_diffs: Vec<f64> = Vec::new();
    let mut done = [false; 7];
    while t0.elapsed() < Duration::from_secs(secs) {
        for ev in events.try_iter() {
            let t = t0.elapsed().as_secs_f64();
            match ev {
                Event::TrackChanged(Some(i)) => println!("{t:7.2} EVENT TrackChanged {} - {} [{}] dur {:.1}s", i.artist, i.title, i.source_app, i.duration_ms / 1000.0),
                Event::TrackChanged(None) => println!("{t:7.2} EVENT TrackChanged none"),
                Event::TrackUpdated(i) => println!("{t:7.2} EVENT TrackUpdated dur {:.1}s album {:?}", i.duration_ms / 1000.0, i.album),
                Event::Artwork(a) => println!("{t:7.2} EVENT Artwork {:?} bytes", a.map(|b| b.len())),
                Event::PlaybackChanged { playing } => println!("{t:7.2} EVENT PlaybackChanged playing={playing}"),
                _ => {}
            }
        }

        let t = t0.elapsed().as_secs_f64();
        let session = if only_spotify {
            mgr.GetSessions().ok().and_then(|v| {
                v.into_iter().find(|s| s.SourceAppUserModelId().map(|a| a.to_string().to_lowercase().contains("spotify")).unwrap_or(false))
            })
        } else {
            pick(&mgr)
        };
        if drive {
            if let Some(s) = &session {
                let full: [(f64, &str); 7] =
                    [(1.0, "play"), (62.0, "pause"), (66.0, "play"), (72.0, "seek 60s"), (80.0, "seek 20s"), (88.0, "next"), (100.0, "pause")];
                // `--short`: just a track change.
                let short: [(f64, &str); 7] =
                    [(1.0, "play"), (5.0, "next"), (12.0, "pause"), (1e9, ""), (1e9, ""), (1e9, ""), (1e9, "")];
                let steps = if args.iter().any(|a| a == "--short") { short } else { full };
                for (i, (at, what)) in steps.iter().enumerate() {
                    if !done[i] && t >= *at {
                        done[i] = true;
                        println!("{t:7.2} DRIVE {what}");
                        let r = match *what {
                            "pause" => s.TryPauseAsync().and_then(|o| o.join()),
                            "play" => s.TryPlayAsync().and_then(|o| o.join()),
                            "seek 60s" => s.TryChangePlaybackPositionAsync(60 * 10_000_000).and_then(|o| o.join()),
                            "seek 20s" => s.TryChangePlaybackPositionAsync(20 * 10_000_000).and_then(|o| o.join()),
                            _ => s.TrySkipNextAsync().and_then(|o| o.join()),
                        };
                        println!("{t:7.2} DRIVE {what} -> {r:?}");
                    }
                }
            }
        }

        let clock = position::now_ms();
        let line = session.as_ref().and_then(smtc).map(|(raw, upd, est)| {
            let fresh = last_raw != Some((raw, upd));
            last_raw = Some((raw, upd));
            let d = clock.map(|c| c - est);
            if fresh {
                if let Some(d) = d {
                    fresh_diffs.push(d);
                }
            }
            format!(
                "smtc={est:9.0} raw={raw:9.0} age={:6.0} d={}{}",
                wall_ms() - upd,
                d.map(|d| format!("{d:+6.0}")).unwrap_or("   n/a".into()),
                if fresh { " FRESH" } else { "" }
            )
        });
        println!(
            "{t:7.2} wall={:.0} clock={} playing={} {}",
            wall_ms(),
            clock.map(|c| format!("{c:9.0}")).unwrap_or("     none".into()),
            position::playing(),
            line.unwrap_or_default()
        );
        std::thread::sleep(Duration::from_millis(250));
    }
    let worst = fresh_diffs.iter().skip(1).fold(0.0f64, |m, d| m.max(d.abs()));
    println!("fresh samples: {}  diffs: {:?}  worst after first: {worst:.0} ms", fresh_diffs.len(), fresh_diffs.iter().map(|d| d.round()).collect::<Vec<_>>());
    ctx.bus.publish(Event::Quit);
    std::thread::sleep(Duration::from_millis(200));
}
