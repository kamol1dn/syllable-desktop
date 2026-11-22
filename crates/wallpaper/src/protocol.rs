//! The messages the wallpaper page speaks (`assets/wallpaper/js/app.js`, `Bridge`).
//!
//! Server -> page, one JSON object per text frame, told apart by `type`:
//!
//! | type             | fields                                                       |
//! |------------------|--------------------------------------------------------------|
//! | `hello`          | `app`, `caps`                                                |
//! | `settings`       | `folder`, `desktop`, `lockscreen` (lower-case setting keys)  |
//! | `track`          | `track`: `{title, artist, album, cover, durationMs, spotify}` or `null` |
//! | `lyrics`         | `state`: `ok` / `loading` / `none`, `lyrics` (see below) or `null` |
//! | `art`            | `dataUrl`: the media session's cover as a `data:` URL       |
//! | `pos`            | `has`, `ms` (sync offset already added), `playing`, `rate`   |
//! | `wallpapers`     | `dir`, `files` (absolute paths), `error` or `null`           |
//! | `audio`          | `bass` 0..1                                                  |
//! | `next-wallpaper` | none                                                         |
//!
//! Page -> server: `{type:"list-wallpapers", dir}` and `{type:"audio", on}`.
//!
//! Lyrics keep the prototype's PascalCase shape, all times in ms:
//! `{Type: "Syllable"|"Line", StartTime, Lines: [Line], Bg: [Line], Source}` with
//! `Line = {Start, End, Text, Sylls: [{Start, End, Text, PartOfWord}] | null, Opposite}`.

use serde_json::{json, Value};
use syllable_core::{BgVocal, Config, LyricLine, Lyrics, Seg, SyncType, TrackInfo};

pub fn hello() -> Value {
    // No `controls`: the desktop host sits under the icons and never gets clicks.
    json!({ "type": "hello", "app": "Syllable", "caps": { "controls": false } })
}

pub fn settings(config: &Config) -> Value {
    json!({
        "type": "settings",
        "folder": config.wallpaper_folder,
        "desktop": config.wallpaper_desktop.to_message(),
        "lockscreen": config.wallpaper_lock.to_message(),
    })
}

pub fn track(info: Option<&TrackInfo>) -> Value {
    let track = info.map(|t| {
        json!({
            "title": t.title,
            "artist": t.artist,
            "album": t.album,
            "cover": t.cover_url,
            "durationMs": t.duration_ms,
            "spotify": t.is_spotify,
        })
    });
    json!({ "type": "track", "track": track })
}

/// What to show while the lyrics module is still looking.
pub fn lyrics_loading() -> Value {
    json!({ "type": "lyrics", "state": "loading", "lyrics": null })
}

/// `None` or an empty set both mean "this song has no (timed) lyrics".
pub fn lyrics(lyrics: Option<&Lyrics>) -> Value {
    match lyrics {
        Some(l) if !l.is_empty() => json!({ "type": "lyrics", "state": "ok", "lyrics": lyrics_body(l) }),
        _ => json!({ "type": "lyrics", "state": "none", "lyrics": null }),
    }
}

fn lyrics_body(l: &Lyrics) -> Value {
    json!({
        "Type": match l.sync { SyncType::Syllable => "Syllable", SyncType::Line => "Line" },
        "StartTime": l.start_time,
        "Lines": l.lines.iter().map(line).collect::<Vec<_>>(),
        "Bg": l.bg.iter().map(bg_line).collect::<Vec<_>>(),
        "Source": l.source,
    })
}

fn line(l: &LyricLine) -> Value {
    line_value(l.start, l.end, &l.text, l.sylls.as_deref(), l.opposite)
}

fn bg_line(l: &BgVocal) -> Value {
    line_value(l.start, l.end, &l.text, l.sylls.as_deref(), l.opposite)
}

fn line_value(start: f64, end: f64, text: &str, sylls: Option<&[Seg]>, opposite: bool) -> Value {
    let sylls = sylls.filter(|s| !s.is_empty()).map(|s| {
        s.iter()
            .map(|g| json!({ "Start": g.start, "End": g.end, "Text": g.text, "PartOfWord": g.part_of_word }))
            .collect::<Vec<_>>()
    });
    json!({ "Start": start, "End": end, "Text": text, "Sylls": sylls, "Opposite": opposite })
}

/// Cover bytes as the page's `art` message; `None` for empty input.
pub fn art(bytes: &[u8]) -> Option<Value> {
    if bytes.is_empty() {
        return None;
    }
    let mime = if bytes[0] == 0x89 { "image/png" } else { "image/jpeg" };
    Some(json!({ "type": "art", "dataUrl": format!("data:{mime};base64,{}", base64(bytes)) }))
}

/// `now_ms` is the playback clock (`None` when nothing plays); the sync offset is added here,
/// the same nudge the taskbar applies, so both show the same line. `rate` is the rate the
/// clock runs at, which the page extrapolates with: while the clock slews off a small
/// backward correction it runs slower than 1, and a page assuming 1 would run ahead and
/// be pulled back on every message, so the sweep would stutter backwards.
pub fn position(now_ms: Option<f64>, playing: bool, rate: f64, offset_ms: i32) -> Value {
    let rate = if rate.is_finite() && rate > 0.0 { rate } else { 1.0 };
    json!({
        "type": "pos",
        "has": now_ms.is_some(),
        "ms": now_ms.unwrap_or(0.0) + offset_ms as f64,
        "playing": playing,
        "rate": rate,
    })
}

pub fn audio(bass: f32) -> Value {
    let bass = (bass.clamp(0.0, 1.0) as f64 * 1000.0).round() / 1000.0;
    json!({ "type": "audio", "bass": bass })
}

pub fn wallpapers(dir: &str, files: &[String]) -> Value {
    let error = if files.is_empty() { Some("no images found") } else { None };
    json!({ "type": "wallpapers", "dir": dir, "files": files, "error": error })
}

pub fn next_wallpaper() -> Value {
    json!({ "type": "next-wallpaper" })
}

/// A message from the page.
#[derive(Debug, PartialEq)]
pub enum Request {
    ListWallpapers { dir: String },
    Audio { on: bool },
}

pub fn parse_request(text: &str) -> Option<Request> {
    let v: Value = serde_json::from_str(text).ok()?;
    match v.get("type")?.as_str()? {
        "list-wallpapers" => Some(Request::ListWallpapers { dir: v.get("dir").and_then(Value::as_str).unwrap_or("").to_string() }),
        "audio" => Some(Request::Audio { on: v.get("on").and_then(Value::as_bool).unwrap_or(false) }),
        _ => None,
    }
}

fn base64(bytes: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for c in bytes.chunks(3) {
        let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if c.len() > 1 { T[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if c.len() > 2 { T[n as usize & 63] as char } else { '=' });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seg(start: f64, end: f64, text: &str, part_of_word: bool) -> Seg {
        Seg { start, end, text: text.into(), part_of_word }
    }

    #[test]
    fn syllable_lyrics_use_the_page_shape() {
        let l = Lyrics {
            sync: SyncType::Syllable,
            start_time: 3600.0,
            lines: vec![LyricLine {
                start: 3600.0,
                end: 4600.0,
                text: "Every sliding".into(),
                sylls: Some(vec![seg(3600.0, 3980.0, "Every ", false), seg(3980.0, 4280.0, "sli", true), seg(4280.0, 4600.0, "ding ", false)]),
                opposite: true,
            }],
            bg: vec![BgVocal { start: 4000.0, end: 4500.0, text: "(oh)".into(), sylls: None, opposite: false }],
            source: "demo".into(),
        };
        let m = lyrics(Some(&l));
        assert_eq!(
            m,
            json!({
                "type": "lyrics", "state": "ok",
                "lyrics": {
                    "Type": "Syllable", "StartTime": 3600.0, "Source": "demo",
                    "Lines": [{
                        "Start": 3600.0, "End": 4600.0, "Text": "Every sliding", "Opposite": true,
                        "Sylls": [
                            { "Start": 3600.0, "End": 3980.0, "Text": "Every ", "PartOfWord": false },
                            { "Start": 3980.0, "End": 4280.0, "Text": "sli", "PartOfWord": true },
                            { "Start": 4280.0, "End": 4600.0, "Text": "ding ", "PartOfWord": false }
                        ]
                    }],
                    "Bg": [{ "Start": 4000.0, "End": 4500.0, "Text": "(oh)", "Sylls": null, "Opposite": false }]
                }
            })
        );
    }

    #[test]
    fn line_lyrics_have_null_sylls_and_empty_sets_mean_none() {
        let l = Lyrics {
            sync: SyncType::Line,
            lines: vec![LyricLine { start: 1.0, end: 2.0, text: "a".into(), sylls: Some(vec![]), opposite: false }],
            ..Default::default()
        };
        let m = lyrics(Some(&l));
        assert_eq!(m["lyrics"]["Type"], "Line");
        assert_eq!(m["lyrics"]["Lines"][0]["Sylls"], Value::Null);
        assert_eq!(m["lyrics"]["Bg"], json!([]));

        let none = json!({ "type": "lyrics", "state": "none", "lyrics": null });
        assert_eq!(lyrics(None), none);
        assert_eq!(lyrics(Some(&Lyrics::default())), none);
        assert_eq!(lyrics_loading()["state"], "loading");
    }

    #[test]
    fn track_and_no_track() {
        let t = TrackInfo {
            title: "Neon Rivers".into(),
            artist: "Demo Artist".into(),
            album: "Sessions".into(),
            cover_url: Some("https://example.invalid/c.jpg".into()),
            duration_ms: 215000.0,
            is_spotify: true,
            ..Default::default()
        };
        assert_eq!(
            track(Some(&t)),
            json!({ "type": "track", "track": {
                "title": "Neon Rivers", "artist": "Demo Artist", "album": "Sessions",
                "cover": "https://example.invalid/c.jpg", "durationMs": 215000.0, "spotify": true } })
        );
        assert_eq!(track(None), json!({ "type": "track", "track": null }));
    }

    #[test]
    fn position_adds_the_offset_and_says_when_nothing_plays() {
        assert_eq!(position(Some(1000.0), true, 1.0, 250), json!({ "type": "pos", "has": true, "ms": 1250.0, "playing": true, "rate": 1.0 }));
        let p = position(None, false, 1.0, 0);
        assert_eq!(p["has"], false);
        assert_eq!(p["ms"], 0.0);
    }

    #[test]
    fn position_carries_the_clock_rate() {
        assert_eq!(position(Some(0.0), true, 0.9, 0)["rate"], 0.9);
        assert_eq!(position(Some(0.0), true, 0.0, 0)["rate"], 1.0);
        assert_eq!(position(Some(0.0), true, f64::NAN, 0)["rate"], 1.0);
    }

    #[test]
    fn art_is_a_data_url_with_the_right_mime() {
        let png = art(&[0x89, b'P', b'N', b'G']).unwrap();
        assert_eq!(png, json!({ "type": "art", "dataUrl": "data:image/png;base64,iVBORw==" }));
        let jpg = art(&[0xFF, 0xD8, 0xFF]).unwrap();
        assert_eq!(jpg["dataUrl"], "data:image/jpeg;base64,/9j/");
        assert!(art(&[]).is_none());
    }

    #[test]
    fn base64_matches_the_rfc_vectors() {
        for (i, o) in [("", ""), ("f", "Zg=="), ("fo", "Zm8="), ("foo", "Zm9v"), ("foob", "Zm9vYg=="), ("fooba", "Zm9vYmE="), ("foobar", "Zm9vYmFy")] {
            assert_eq!(base64(i.as_bytes()), o);
        }
    }

    #[test]
    fn settings_carry_both_surfaces_and_the_folder() {
        let mut c = Config::default();
        c.wallpaper_folder = r"C:\Pics".into();
        c.wallpaper_desktop.layout = "karaoke".into();
        let m = settings(&c);
        assert_eq!(m["type"], "settings");
        assert_eq!(m["folder"], r"C:\Pics");
        assert_eq!(m["desktop"]["layout"], "karaoke");
        assert_eq!(m["lockscreen"]["layout"], "lock");
        assert_eq!(m["desktop"]["lyricssize"], 100);
    }

    #[test]
    fn audio_wallpapers_and_requests() {
        assert_eq!(audio(0.123456), json!({ "type": "audio", "bass": 0.123 }));
        assert_eq!(audio(7.0)["bass"], 1.0);
        assert_eq!(next_wallpaper(), json!({ "type": "next-wallpaper" }));
        assert_eq!(wallpapers("d", &[])["error"], "no images found");
        assert_eq!(wallpapers("d", &["d\\a.jpg".into()])["error"], Value::Null);
        assert_eq!(parse_request(r#"{"type":"list-wallpapers","dir":"C:\\x"}"#), Some(Request::ListWallpapers { dir: r"C:\x".into() }));
        assert_eq!(parse_request(r#"{"type":"audio","on":true}"#), Some(Request::Audio { on: true }));
        assert_eq!(parse_request(r#"{"type":"audio"}"#), Some(Request::Audio { on: false }));
        assert_eq!(parse_request(r#"{"type":"tap"}"#), None);
        assert_eq!(parse_request("not json"), None);
    }
}
