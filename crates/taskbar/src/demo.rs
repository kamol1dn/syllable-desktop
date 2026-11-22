//! Debug-only test source: publishes a fake track, word-timed lyrics (with a
//! background vocal, a long line and an instrumental gap) and runs the playback
//! clock, looping. Enabled with `SYLLABLE_DEMO=1` in a debug build, so the real exe
//! (with its manifest) is what gets tested while media and lyrics are stubs.
//! `SYLLABLE_DEMO=pause` publishes the same but leaves playback paused at 6.8 s;
//! `SYLLABLE_DEMO=line` publishes line-timed lyrics (no syllables) instead.

use std::sync::Arc;
use std::time::Duration;

use syllable_core::{log, position, BgVocal, Context, Event, LyricLine, Lyrics, Seg, SyncType, TrackInfo};

/// Word-timed line from (text, duration ms) words, starting at `start`.
fn line(start: f64, words: &[(&str, f64)]) -> LyricLine {
    let mut t = start;
    let mut sylls = Vec::new();
    for (i, (w, d)) in words.iter().enumerate() {
        // Split longer words into two syllables, to exercise part_of_word.
        let last = i + 1 == words.len();
        let text = if last { w.to_string() } else { format!("{w} ") };
        if w.chars().count() >= 6 {
            let cut = w.char_indices().nth(w.chars().count() / 2).map(|(b, _)| b).unwrap();
            let (a, b) = text.split_at(cut);
            sylls.push(Seg { start: t, end: t + d / 2.0, text: a.into(), part_of_word: true });
            sylls.push(Seg { start: t + d / 2.0, end: t + d, text: b.into(), part_of_word: false });
        } else {
            sylls.push(Seg { start: t, end: t + d, text, part_of_word: false });
        }
        t += d + 40.0;
    }
    let text = sylls.iter().map(|s| s.text.as_str()).collect::<String>();
    LyricLine { start, end: t, text, sylls: Some(sylls), opposite: false }
}

pub fn lyrics() -> Lyrics {
    let lines = vec![
        line(3000.0, &[("Streetlights", 700.0), ("hum", 300.0), ("a", 150.0), ("borrowed", 500.0), ("tune", 600.0)]),
        line(6000.0, &[("I", 200.0), ("count", 350.0), ("the", 200.0), ("windows", 500.0), ("one", 300.0), ("by", 250.0), ("one", 600.0)]),
        line(9200.0, &[("Every", 400.0), ("echo", 450.0), ("folds", 400.0), ("into", 350.0), ("the", 200.0), ("night", 900.0)]),
        // Long gap after this one: interlude dots.
        line(12500.0, &[("Hold", 400.0), ("on", 300.0), ("hold", 400.0), ("on", 900.0)]),
        line(19000.0, &[
            ("This", 250.0), ("is", 150.0), ("a", 120.0), ("deliberately", 700.0), ("long", 300.0),
            ("line", 300.0), ("that", 200.0), ("has", 200.0), ("to", 150.0), ("shrink", 400.0),
            ("or", 150.0), ("scroll", 400.0), ("to", 150.0), ("fit", 250.0), ("inside", 400.0),
            ("the", 150.0), ("narrow", 400.0), ("taskbar", 500.0), ("strip", 600.0),
        ]),
        line(26500.0, &[("Paper", 400.0), ("moons", 450.0), ("and", 200.0), ("quiet", 450.0), ("rooms", 800.0)]),
        line(29800.0, &[("Ünïcödé", 600.0), ("and", 200.0), ("emoji", 400.0), ("♪", 500.0)]),
    ];
    let bg = vec![BgVocal {
        start: 13600.0,
        end: 15200.0,
        text: "(hold on)".into(),
        sylls: Some(vec![
            Seg { start: 13600.0, end: 14300.0, text: "(hold ".into(), part_of_word: false },
            Seg { start: 14300.0, end: 15200.0, text: "on)".into(), part_of_word: false },
        ]),
        opposite: false,
    }];
    Lyrics { sync: SyncType::Syllable, start_time: 3000.0, lines, bg, source: "demo".into() }
}

/// The same song with line timing only (what LRC-style providers give).
pub fn line_lyrics() -> Lyrics {
    let mut l = lyrics();
    for line in &mut l.lines {
        line.sylls = None;
        line.text = line.text.trim().to_string();
    }
    for b in &mut l.bg {
        b.sylls = None;
    }
    l.sync = SyncType::Line;
    l
}

const SONG_MS: f64 = 36_000.0;

pub fn spawn(ctx: Context) {
    let mode = std::env::var("SYLLABLE_DEMO").unwrap_or_default();
    let paused = mode == "pause";
    std::thread::Builder::new()
        .name("taskbar-demo".into())
        .spawn(move || {
            log!("taskbar: demo source running ({})", if mode.is_empty() { "1" } else { &mode });
            let rx = ctx.bus.subscribe();
            let track = Arc::new(TrackInfo {
                title: "Borrowed Tune".into(),
                artist: "The Demo Source".into(),
                album: "Debug Builds".into(),
                duration_ms: SONG_MS,
                source_app: "syllable-demo".into(),
                ..Default::default()
            });
            let ly = Arc::new(if mode == "line" { line_lyrics() } else { lyrics() });
            loop {
                ctx.bus.publish(Event::TrackChanged(Some(track.clone())));
                ctx.bus.publish(Event::Lyrics(Some(ly.clone())));
                if paused {
                    position::set(6800.0, false, 1.0);
                    ctx.bus.publish(Event::PlaybackChanged { playing: false });
                } else {
                    position::set(0.0, true, 1.0);
                    ctx.bus.publish(Event::PlaybackChanged { playing: true });
                }
                // Sleep through the song, but stop promptly on Quit.
                let mut left = SONG_MS;
                while left > 0.0 {
                    match rx.recv_timeout(Duration::from_millis(200)) {
                        Ok(Event::Quit) | Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return,
                        _ => {}
                    }
                    if !paused {
                        left -= 200.0;
                    }
                }
            }
        })
        .expect("spawn demo");
}
