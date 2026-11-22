//! Musixmatch desktop-app API, as the Android app's `MusixmatchProvider`:
//! "richsync" lyrics carry word timing; otherwise line-synced subtitles. Uses an
//! anonymous user token, kept in memory for 6 hours.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde_json::Value;
use syllable_core::{matching, TrackInfo};
use ureq::Agent;

use crate::cache::MUSIXMATCH;
use crate::http::{self, enc, Token};
use crate::parse::{self, Found, Word};

const BASE: &str = "https://apic-desktop.musixmatch.com/ws/1.1/";
const APP_ID: &str = "web-desktop-app-v1.0";
const TOKEN_TTL: Duration = Duration::from_secs(6 * 3600);
const HEADERS: [(&str, &str); 2] = [("Cookie", "AWSELBCORS=0; AWSELB=0"), ("Origin", "musixmatch.com")];

/// After a refused token request, don't ask again for this long.
const REFUSED_PAUSE: Duration = Duration::from_secs(5 * 60);

static USER_TOKEN: Mutex<Option<(String, Instant)>> = Mutex::new(None);
static TOKEN_REFUSED_AT: Mutex<Option<Instant>> = Mutex::new(None);

/// Musixmatch answers a throttled token request with a placeholder: all zeros
/// (seen from this PC), or "UpgradeOnly...". Every call with it fails.
fn usable_token(tok: &str) -> bool {
    let t = tok.trim();
    !t.is_empty() && !t.starts_with("UpgradeOnly") && !t.chars().all(|c| c == '0')
}

/// GET a body: `None` for 404, an error for anything else that isn't 2xx.
fn get(agent: &Agent, url: &str) -> anyhow::Result<Option<String>> {
    let res = http::get(agent, url, &HEADERS)?;
    match res.status {
        404 => Ok(None),
        s if (200..300).contains(&s) => Ok(Some(res.body)),
        s => anyhow::bail!("Musixmatch: HTTP {s}"),
    }
}

fn user_token(agent: &Agent, force_new: bool) -> anyhow::Result<Option<String>> {
    if !force_new {
        if let Some((t, at)) = USER_TOKEN.lock().unwrap().as_ref() {
            if at.elapsed() < TOKEN_TTL {
                return Ok(Some(t.clone()));
            }
        }
    }
    if TOKEN_REFUSED_AT.lock().unwrap().is_some_and(|at| at.elapsed() < REFUSED_PAUSE) {
        return Ok(None);
    }
    let Some(body) = get(agent, &format!("{BASE}token.get?app_id={APP_ID}"))? else { return Ok(None) };
    let doc: Value = serde_json::from_str(&body)?;
    let tok = doc.pointer("/message/body/user_token").and_then(Value::as_str).unwrap_or("");
    if !usable_token(tok) {
        syllable_core::log!("musixmatch: no usable token (throttled), not asking again for {} min", REFUSED_PAUSE.as_secs() / 60);
        *TOKEN_REFUSED_AT.lock().unwrap() = Some(Instant::now());
        return Ok(None);
    }
    *TOKEN_REFUSED_AT.lock().unwrap() = None;
    *USER_TOKEN.lock().unwrap() = Some((tok.to_string(), Instant::now()));
    Ok(Some(tok.to_string()))
}

/// The `message.body` of a call, or `None` when Musixmatch has nothing (any status but 200).
fn call(agent: &Agent, method: &str, params: &str, token: &Token) -> anyhow::Result<Option<Value>> {
    for attempt in 0..2 {
        token.check()?;
        let Some(tok) = user_token(agent, attempt > 0)? else { return Ok(None) };
        let url = format!("{BASE}{method}?format=json&app_id={APP_ID}&usertoken={}&{params}", enc(&tok));
        let Some(body) = get(agent, &url)? else { return Ok(None) };
        let mut doc: Value = serde_json::from_str(&body)?;
        let status = doc.pointer("/message/header/status_code").and_then(Value::as_i64).unwrap_or(0);
        if status == 401 {
            *USER_TOKEN.lock().unwrap() = None;
            continue;
        }
        if status != 200 {
            return Ok(None);
        }
        return Ok(doc.pointer_mut("/message/body").map(Value::take));
    }
    Ok(None)
}

pub fn fetch(agent: &Agent, info: &TrackInfo, token: &Token) -> anyhow::Result<Option<Found>> {
    let q = matching::query(info);
    if q.title.is_empty() || q.artist.is_empty() {
        return Ok(None);
    }
    let mut params = format!("q_track={}&q_artist={}", enc(&q.title), enc(&q.artist));
    if let Some(d) = q.strict_duration_ms() {
        params.push_str(&format!("&q_duration={}", (d / 1000.0) as i64));
    }
    let Some(body) = call(agent, "matcher.track.get", &params, token)? else { return Ok(None) };
    let Some(track) = body.get("track") else { return Ok(None) };
    let text = |k: &str| track.get(k).and_then(Value::as_str).unwrap_or("").to_string();
    let int = |k: &str| track.get(k).and_then(Value::as_i64).unwrap_or(0);
    if matching::match_score(&q, &text("track_name"), &text("artist_name"), int("track_length") as f64 * 1000.0) < 0.0 {
        return Ok(None);
    }
    if int("instrumental") == 1 {
        return Ok(None);
    }
    let id = int("track_id");

    if int("has_richsync") == 1 {
        let rs = call(agent, "track.richsync.get", &format!("track_id={id}"), token)?;
        if let Some(body) = rs.as_ref().and_then(|b| b.pointer("/richsync/richsync_body")).and_then(Value::as_str) {
            if let Some(f) = parse_richsync(body) {
                return Ok(Some(f));
            }
        }
    }
    if int("has_subtitles") == 1 {
        let sub = call(agent, "track.subtitle.get", &format!("track_id={id}&subtitle_format=lrc"), token)?;
        if let Some(body) = sub.as_ref().and_then(|b| b.pointer("/subtitle/subtitle_body")).and_then(Value::as_str) {
            if let Some(f) = parse::from_lrc_as(body, info.duration_ms, MUSIXMATCH) {
                return Ok(Some(f));
            }
        }
    }
    Ok(None)
}

/// `[{"ts":1.2,"te":3.4,"l":[{"c":"Hel","o":0.0},{"c":"lo","o":0.2},{"c":" ","o":0.4},...]}]`
/// (seconds; `o` is the offset into the line). A blank piece ends the word before it.
pub fn parse_richsync(body: &str) -> Option<Found> {
    let doc: Value = serde_json::from_str(body).ok()?;
    let mut lines = Vec::new();
    for o in doc.as_array()? {
        let sec = |v: &Value, k: &str| v.get(k).and_then(Value::as_f64).unwrap_or(0.0) * 1000.0;
        let (ts, te) = (sec(o, "ts"), sec(o, "te"));
        let Some(chars) = o.get("l").and_then(Value::as_array) else { continue };
        let pieces: Vec<(String, f64)> = chars
            .iter()
            .map(|c| (c.get("c").and_then(Value::as_str).unwrap_or("").to_string(), ts + sec(c, "o")))
            .collect();
        let mut words: Vec<Word> = Vec::new();
        for (i, (text, start)) in pieces.iter().enumerate() {
            if text.trim().is_empty() {
                if let Some(w) = words.last_mut() {
                    w.text.push(' ');
                }
                continue;
            }
            let next_start = pieces.get(i + 1).map(|p| p.1).unwrap_or(te);
            words.push(Word { text: text.clone(), start: *start, end: next_start.max(start + 60.0) });
        }
        if !words.is_empty() {
            lines.push((ts, te, words));
        }
    }
    parse::from_words(lines, MUSIXMATCH)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn placeholder_tokens_are_refused() {
        assert!(!usable_token("00000000000000000000000000000000000000000000000000000000"));
        assert!(!usable_token("UpgradeOnlyUpgradeOnly"));
        assert!(!usable_token(" "));
        assert!(usable_token("2203269256ff7abcb649269df00e14c833dbf4ddfb5b36a1aae8b0"));
    }

    #[test]
    fn richsync_words_and_spaces() {
        let f = parse_richsync(
            r#"[{"ts":1.0,"te":3.0,"l":[{"c":"Hel","o":0.0},{"c":"lo","o":0.25},{"c":" ","o":0.5},{"c":"world","o":0.6}]}]"#,
        )
        .unwrap();
        assert_eq!(f.rank(), 3);
        assert_eq!(f.source(), MUSIXMATCH);
        let line = &f.lyrics.lines[0];
        assert_eq!(line.text, "Hello world");
        assert_eq!((line.start, line.end), (1000.0, 3000.0));
        let segs = line.sylls.as_ref().unwrap();
        assert_eq!(segs.iter().map(|s| s.text.as_str()).collect::<Vec<_>>(), ["Hel", "lo ", "world"]);
        assert!(segs[0].part_of_word && !segs[1].part_of_word);
        assert_eq!((segs[1].start, segs[2].start, segs[2].end), (1250.0, 1600.0, 3000.0));
    }
}
