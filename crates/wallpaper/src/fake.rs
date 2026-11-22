//! Debug-only stand-in for the media, lyrics and audio modules (`SYLLABLE_FAKE_MEDIA=1`):
//! a 40-second made-up song on a loop, published on the bus exactly as the real modules
//! will, so live data can be followed all the way into the wallpaper page.

use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use syllable_core::{audio, log, position, Context, Event, LyricLine, Lyrics, Seg, SyncType, TrackInfo};

const SONG_MS: f64 = 40_000.0;

const WORDS: [&str; 6] = [
    "Neon rivers run beneath the station",
    "Every sliding bass line pulls me under",
    "Hold on to the static in the wires",
    "I can hear the city count the seconds",
    "Lights come on along the empty platform",
    "And the night keeps singing back to me",
];

fn lyrics() -> Lyrics {
    let mut lines = Vec::new();
    let mut t = 3000.0;
    for (i, text) in WORDS.iter().enumerate() {
        let words: Vec<&str> = text.split(' ').collect();
        let start = t;
        let mut sylls = Vec::new();
        for (j, w) in words.iter().enumerate() {
            let len = 260.0 + 40.0 * w.len() as f64 + if j == words.len() - 1 { 500.0 } else { 0.0 };
            sylls.push(Seg { start: t, end: t + len, text: format!("{w} "), part_of_word: false });
            t += len;
        }
        lines.push(LyricLine { start, end: t, text: text.to_string(), sylls: Some(sylls), opposite: i % 4 == 3 });
        t += 900.0;
    }
    Lyrics { sync: SyncType::Syllable, start_time: 3000.0, lines, bg: vec![], source: "fake".into() }
}

pub fn start(ctx: Context) {
    log!("wallpaper: SYLLABLE_FAKE_MEDIA is set, publishing a made-up song");
    thread::Builder::new()
        .name("wallpaper-fake-media".into())
        .spawn(move || {
            let events = ctx.bus.subscribe();
            // Let the other modules subscribe first.
            thread::sleep(Duration::from_millis(500));
            let track = Arc::new(TrackInfo {
                title: "Neon Rivers".into(),
                artist: "Fake Source".into(),
                album: "Debug Sessions".into(),
                duration_ms: SONG_MS,
                source_app: "syllable-fake".into(),
                ..Default::default()
            });
            ctx.bus.publish(Event::TrackChanged(Some(track)));
            let cover = crate::page_path()
                .and_then(|p| std::fs::read(p.parent()?.join("demo").join("cover.png")).ok());
            ctx.bus.publish(Event::Artwork(cover.map(Arc::new)));
            thread::sleep(Duration::from_millis(300)); // as if a provider answered
            ctx.bus.publish(Event::Lyrics(Some(Arc::new(lyrics()))));
            ctx.bus.publish(Event::PlaybackChanged { playing: true });

            let began = Instant::now();
            let mut last_loop = 0u64;
            loop {
                if matches!(events.try_recv(), Ok(Event::Quit)) {
                    position::clear();
                    break;
                }
                let ms = began.elapsed().as_secs_f64() * 1000.0;
                let n = (ms / SONG_MS) as u64;
                if n != last_loop {
                    // Loop the song: a jump back the page has to settle on.
                    last_loop = n;
                    position::set(0.0, true, 1.0);
                }
                position::set(ms % SONG_MS, true, 1.0);
                let beat = ((ms / 1000.0 * std::f64::consts::PI * 2.0).sin() * 0.5 + 0.5).powi(3);
                audio::set(audio::AudioFrame { bands: vec![], bass: beat as f32, level: beat as f32 });
                thread::sleep(Duration::from_millis(33));
            }
        })
        .ok();
}
