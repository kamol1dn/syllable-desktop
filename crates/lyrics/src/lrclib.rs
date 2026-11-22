//! LRCLIB (https://lrclib.net): free, no setup, line timing at best.

use serde_json::Value;
use syllable_core::{log, matching, TrackInfo};
use ureq::Agent;

use crate::http::{self, enc, Token};
use crate::parse::{self, Found};

/// `Ok(None)` is an answer ("they have nothing"); `Err` means LRCLIB could not be reached.
/// As the Android app: the exact lookup, then one search by title and artist,
/// scored so a same-titled song by someone else never wins. No title-only search.
pub fn fetch(agent: &Agent, info: &TrackInfo, token: &Token) -> anyhow::Result<Option<Found>> {
    let q = matching::query(info);
    if q.title.is_empty() || q.artist.is_empty() {
        return Ok(None); // nothing could score as a match without an artist
    }
    let mut answered = false;
    let mut last_error: Option<anyhow::Error> = None;

    token.check()?;
    match http::get(agent, &get_url(&q), &[]) {
        Ok(res) => {
            answered = res.ok() || res.status == 404;
            if res.ok() {
                if let Some(lrc) = synced(&serde_json::from_str(&res.body).unwrap_or(Value::Null)) {
                    return Ok(parse::from_lrc(lrc, info.duration_ms));
                }
            }
        }
        Err(e) => {
            log!("resolve: lrclib error: {e}");
            last_error = Some(e);
        }
    }

    token.check()?;
    let url = format!("https://lrclib.net/api/search?track_name={}&artist_name={}", enc(&q.title), enc(&q.artist));
    match http::get(agent, &url, &[]) {
        Ok(res) if res.ok() => {
            let doc: Value = serde_json::from_str(&res.body).unwrap_or(Value::Null);
            return Ok(pick_search_result(&doc, &q).and_then(|lrc| parse::from_lrc(lrc, info.duration_ms)));
        }
        Ok(_) => {}
        Err(e) => {
            log!("resolve: lrclib error: {e}");
            last_error = Some(e);
        }
    }
    if let (false, Some(e)) = (answered, last_error) {
        anyhow::bail!("lrclib unreachable: {e}");
    }
    Ok(None)
}

/// The exact lookup. LRCLIB matches the duration within 2 s, so an early-ending
/// Spotify duration is left out (it would find nothing, or a cut-down upload).
fn get_url(q: &matching::Query) -> String {
    let mut url = format!("https://lrclib.net/api/get?track_name={}&artist_name={}", enc(&q.title), enc(&q.artist));
    if !q.album.is_empty() {
        url.push_str(&format!("&album_name={}", enc(&q.album)));
    }
    if let Some(d) = q.strict_duration_ms() {
        url.push_str(&format!("&duration={}", (d / 1000.0) as i64));
    }
    url
}

fn synced(v: &Value) -> Option<&str> {
    v.get("syncedLyrics").and_then(Value::as_str).filter(|s| !s.is_empty() && *s != "null")
}

/// The best-scoring search result (synced ones get +0.1), as the Android app picks
/// it; its synced lyrics, if it has any.
fn pick_search_result<'a>(doc: &'a Value, q: &matching::Query) -> Option<&'a str> {
    let text = |e: &Value, k: &str| e.get(k).and_then(Value::as_str).unwrap_or("").to_string();
    let mut best: Option<&Value> = None;
    let mut best_score = 0.0;
    for e in doc.as_array()? {
        let dur = e.get("duration").and_then(Value::as_f64).unwrap_or(0.0) * 1000.0;
        let score = matching::match_score(q, &text(e, "trackName"), &text(e, "artistName"), dur)
            + if synced(e).is_some() { 0.1 } else { 0.0 };
        if score > best_score {
            best_score = score;
            best = Some(e);
        }
    }
    best.and_then(synced)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn query(title: &str, artist: &str, secs: f64, spotify: bool) -> matching::Query {
        matching::query(&TrackInfo {
            title: title.into(),
            artist: artist.into(),
            duration_ms: secs * 1000.0,
            is_spotify: spotify,
            source_app: if spotify { "Spotify.exe" } else { "Chrome" }.into(),
            ..Default::default()
        })
    }

    #[test]
    fn search_picks_the_best_scoring_synced_result() {
        let doc: Value = serde_json::from_str(
            r#"[{"trackName":"Song","artistName":"Artist","duration":300,"syncedLyrics":"[00:01.00]far"},
                {"trackName":"Song","artistName":"Artist","duration":201,"syncedLyrics":"[00:01.00]near"},
                {"trackName":"Song","artistName":"Artist","duration":200,"syncedLyrics":null,"plainLyrics":"plain"}]"#,
        )
        .unwrap();
        assert_eq!(pick_search_result(&doc, &query("Song", "Artist", 200.0, false)), Some("[00:01.00]near"));
        // Only a far one: another cut, rejected.
        let far: Value =
            serde_json::from_str(r#"[{"trackName":"Song","artistName":"Artist","duration":300,"syncedLyrics":"[00:01.00]far"}]"#).unwrap();
        assert_eq!(pick_search_result(&far, &query("Song", "Artist", 200.0, false)), None);
        // Unknown duration: fine.
        assert_eq!(pick_search_result(&far, &query("Song", "Artist", 0.0, false)), Some("[00:01.00]far"));
    }

    /// LRCLIB's search for "Yamakasi" (no artist) once supplied another artist's
    /// song; with the artist and the early-ending Spotify duration (233 s for 264 s)
    /// it is Miyagi's.
    #[test]
    fn search_never_takes_another_artists_song() {
        let doc: Value = serde_json::from_str(
            r#"[{"trackName":"Yamakasi","artistName":"Sticky M.A./Lusi","duration":146,"syncedLyrics":"[00:01.00]other"},
                {"trackName":"Yamakasi","artistName":"Miyagi & Andy Panda","duration":383,"syncedLyrics":"[00:01.00]video cut"},
                {"trackName":"Yamakasi","artistName":"Miyagi & Andy Panda","duration":264,"syncedLyrics":"[00:01.00]right"}]"#,
        )
        .unwrap();
        let q = query("Yamakasi", "Miyagi & Andy Panda", 233.0, true);
        assert_eq!(pick_search_result(&doc, &q), Some("[00:01.00]right"));
        assert_eq!(pick_search_result(&doc, &query("Yamakasi", "Someone Else", 233.0, true)), None);
    }

    #[test]
    fn exact_lookup_leaves_out_an_early_ending_duration() {
        let q = query("Phoenix", "League of Legends", 184.0, true);
        assert_eq!(get_url(&q), "https://lrclib.net/api/get?track_name=Phoenix&artist_name=League%20of%20Legends");
        assert!(get_url(&query("Phoenix", "League of Legends", 198.0, false)).ends_with("&duration=198"));
    }
}
