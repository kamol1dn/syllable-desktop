//! Spicy Lyrics developer API (https://developers.spicylyrics.org) with the user's
//! own key. One request returns the best sync it has: a community sync (word
//! timing, background vocals, duet sides), then Apple Music, then Spotify.

use std::collections::HashSet;
use std::time::Duration;

use serde_json::Value;
use syllable_core::{log, matching, TrackInfo};
use ureq::Agent;

use crate::cache::SPICY_API;
use crate::http::{self, enc, Token};
use crate::parse::{self, Found};

const BASE: &str = "https://api.spicylyrics.org";

#[derive(Debug)]
pub struct KeyRejected {
    pub status: u16,
    pub detail: Option<String>,
}

impl std::fmt::Display for KeyRejected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Spicy Lyrics rejected the API key (HTTP {}", self.status)?;
        if let Some(d) = &self.detail {
            write!(f, ": {d}")?;
        }
        f.write_str(")")
    }
}

impl std::error::Error for KeyRejected {}

pub fn is_spotify_id(id: &str) -> bool {
    id.len() == 22 && id.bytes().all(|b| b.is_ascii_alphanumeric())
}

/// The API's own explanation: `{"Body":{"error":"key_not_found","message":"..."}}`.
fn error_detail(body: &str) -> Option<String> {
    let doc: Value = serde_json::from_str(body).ok()?;
    let r = doc.get("Body").filter(|b| b.is_object()).unwrap_or(&doc);
    ["message", "error"]
        .iter()
        .find_map(|k| r.get(*k).and_then(Value::as_str).filter(|s| !s.is_empty()))
        .map(str::to_string)
}

/// GET with one retry when the API is busy (503). 404 gives `None`; a rejected key
/// is a `KeyRejected` error; anything else non-2xx is an error.
fn get(agent: &Agent, key: &str, url: &str, token: &Token) -> anyhow::Result<Option<String>> {
    let auth = format!("Bearer {key}");
    for attempt in 0.. {
        let res = http::get(agent, url, &[("Authorization", &auth), ("Accept", "application/json")])?;
        match res.status {
            503 if attempt == 0 => {
                log!("spicy-api: busy (503), retrying in 2s");
                token.sleep(Duration::from_secs(2))?;
            }
            404 => return Ok(None),
            401 | 403 => return Err(KeyRejected { status: res.status, detail: error_detail(&res.body) }.into()),
            s if !(200..300).contains(&s) => {
                let detail = error_detail(&res.body).unwrap_or_default();
                anyhow::bail!("{}", format!("Spicy Lyrics API: HTTP {s} {detail}").trim_end().to_string());
            }
            _ => return Ok(Some(res.body)),
        }
    }
    unreachable!()
}

pub fn lyrics_for(agent: &Agent, key: &str, track_id: &str, token: &Token) -> anyhow::Result<Option<Found>> {
    let Some(body) = get(agent, key, &format!("{BASE}/v1/lyrics/{track_id}"), token)? else {
        return Ok(None);
    };
    Ok(parse::from_spicy(&serde_json::from_str(&body)?, SPICY_API))
}

/// The search request for a track: one query, as the Android app sends it. There
/// is no title-only retry: a title alone matches whichever song of that name the
/// API ranks first ("Phoenix" by Stray Kids for League of Legends' "Phoenix").
fn search_url(q: &matching::Query) -> String {
    let mut url = format!("{BASE}/v1/tracks/search?limit=3&title={}", enc(&q.title));
    if !q.artist.is_empty() {
        url.push_str(&format!("&artist={}", enc(&q.artist)));
    }
    if !q.album.is_empty() {
        url.push_str(&format!("&album={}", enc(&q.album)));
    }
    // The API demotes a duration mismatch to "version", so an early-ending Spotify
    // duration would hide the right song: leave it out, as for an unknown one.
    if let Some(d) = q.strict_duration_ms() {
        url.push_str(&format!("&duration={:.3}", d / 1000.0));
    }
    url
}

/// Ids of confident matches ("exact" or "likely"), best first.
pub fn search(agent: &Agent, key: &str, info: &TrackInfo, token: &Token) -> anyhow::Result<Vec<String>> {
    let q = matching::query(info);
    if q.title.is_empty() {
        return Ok(Vec::new());
    }
    token.check()?;
    let ids = match get(agent, key, &search_url(&q), token)? {
        Some(body) => confident_ids(&serde_json::from_str(&body)?),
        None => Vec::new(),
    };
    if ids.is_empty() {
        log!("spicy-api: search found no confident match for \"{} - {}\"", info.artist, info.title);
    }
    Ok(ids)
}

fn confident_ids(doc: &Value) -> Vec<String> {
    let root = doc.get("Body").filter(|b| b.is_object()).unwrap_or(doc);
    let mut ids = Vec::new();
    for r in root.get("results").and_then(Value::as_array).into_iter().flatten() {
        let id = r.get("id").and_then(Value::as_str).unwrap_or("");
        let tier = r.get("tier").and_then(Value::as_str).unwrap_or("");
        if !id.is_empty() && matches!(tier, "exact" | "likely") && !ids.iter().any(|x| x == id) {
            ids.push(id.to_string());
        }
    }
    ids
}

/// Lyrics for the track: the player's own Spotify id first (exact), then the id a
/// Spotify lookup gives, then Spicy's own search. Unsynced results are only
/// returned when nothing synced turned up.
/// `search_ids` is `search` for this track, shared with the Spotify provider.
pub fn fetch(
    agent: &Agent,
    key: &str,
    known_id: Option<&str>,
    spotify_lookup: Option<&dyn Fn() -> anyhow::Result<Option<String>>>,
    search_ids: &dyn Fn() -> anyhow::Result<Vec<String>>,
    token: &Token,
) -> anyhow::Result<Option<Found>> {
    let mut tried = HashSet::new();
    let mut fallback: Option<Found> = None;
    let mut attempt = |id: Option<&str>, via: &str, fallback: &mut Option<Found>| -> anyhow::Result<Option<Found>> {
        let Some(id) = id.filter(|id| is_spotify_id(id) && tried.insert(id.to_string())) else { return Ok(None) };
        token.check()?;
        let l = lyrics_for(agent, key, id, token)?;
        match &l {
            None => log!("spicy-api: {via} {id} -> no lyrics"),
            Some(f) => log!("spicy-api: {via} {id} -> {}", f.describe()),
        }
        match l {
            Some(f) if f.rank() >= 2 => Ok(Some(f)),
            Some(f) => {
                fallback.get_or_insert(f);
                Ok(None)
            }
            None => Ok(None),
        }
    };

    if let Some(f) = attempt(known_id, "player id", &mut fallback)? {
        return Ok(Some(f));
    }
    if let Some(lookup) = spotify_lookup {
        let looked = match lookup() {
            Ok(id) => id,
            Err(e) if http::is_cancelled(&e) => return Err(e),
            Err(e) => {
                log!("spicy-api: spotify lookup failed: {e}");
                None
            }
        };
        if let Some(f) = attempt(looked.as_deref(), "spotify match", &mut fallback)? {
            return Ok(Some(f));
        }
    }
    for id in search_ids()? {
        if let Some(f) = attempt(Some(&id), "search match", &mut fallback)? {
            return Ok(Some(f));
        }
    }
    Ok(fallback)
}

/// For the settings Test button: one cheap search.
pub fn test_key(agent: &Agent, key: &str) -> (bool, String) {
    let probe = TrackInfo {
        title: "Blinding Lights".into(),
        artist: "The Weeknd".into(),
        duration_ms: 200_040.0,
        ..Default::default()
    };
    match search(agent, key.trim(), &probe, &Token::none()) {
        Ok(ids) if !ids.is_empty() => (true, "Key works.".into()),
        Ok(_) => (true, "Key accepted (the test search found nothing, which is odd but not fatal).".into()),
        Err(e) if e.is::<KeyRejected>() => (false, format!("{e}.")),
        Err(e) => (false, format!("Could not reach api.spicylyrics.org: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_and_errors() {
        assert!(is_spotify_id("0VjIjW4GlUZAMYd2vXMi3b"));
        assert!(!is_spotify_id("0VjIjW4GlUZAMYd2vXMi3"));
        assert!(!is_spotify_id("0VjIjW4GlUZAMYd2vXMi3-"));
        assert_eq!(
            error_detail(r#"{"Body":{"error":"key_not_found","message":"No such key"}}"#).as_deref(),
            Some("No such key")
        );
        assert_eq!(error_detail(r#"{"error":"busy"}"#).as_deref(), Some("busy"));
        assert_eq!(error_detail("<html>"), None);
    }

    #[test]
    fn only_confident_search_tiers_count() {
        let doc: Value = serde_json::from_str(
            r#"{"Body":{"results":[{"id":"a","tier":"likely"},{"id":"b","tier":"weak"},{"id":"c","tier":"exact"},{"id":"a","tier":"exact"}]}}"#,
        )
        .unwrap();
        assert_eq!(confident_ids(&doc), ["a", "c"]);
    }

    fn spotify_track(title: &str, artist: &str, secs: f64) -> TrackInfo {
        TrackInfo {
            title: title.into(),
            artist: artist.into(),
            album: "YAMAKASI".into(),
            duration_ms: secs * 1000.0,
            is_spotify: true,
            source_app: "Spotify.exe".into(),
            ..Default::default()
        }
    }

    #[test]
    fn search_sends_the_artist_and_no_early_ending_duration() {
        let q = matching::query(&spotify_track("Yamakasi", "Miyagi & Andy Panda", 233.0));
        assert_eq!(search_url(&q), format!("{BASE}/v1/tracks/search?limit=3&title=Yamakasi&artist=Miyagi%20%26%20Andy%20Panda&album=YAMAKASI"));
        let browser = TrackInfo { source_app: "Chrome".into(), is_spotify: false, ..spotify_track("Song", "Artist", 200.0) };
        assert!(search_url(&matching::query(&browser)).ends_with("&duration=200.000"));
    }
}
