//! Spotify calls for lyrics: the exact track id of what is playing, catalog search,
//! the play queue (prefetch) and Spotify's own lyrics. api.spotify.com takes either
//! token (the user's own app is preferred: its own rate limit); the lyrics endpoint
//! (spclient) only takes the web player's.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde_json::Value;
use syllable_core::matching::{self, TrackCandidate};
use syllable_core::{log, TrackInfo};

use crate::accounts::Accounts;
use crate::http::{self, enc, Token};
use crate::parse::{self, Found};
use crate::{spotify_app, spotify_web};

/// The web player's User-Agent, for spclient, which is picky about clients.
const BROWSER_UA: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36";

#[derive(Clone, Copy, PartialEq)]
enum Auth {
    Api,
    WebPlayer,
}

#[derive(Clone, Debug)]
pub struct QueueItem {
    pub track: TrackCandidate,
    pub album: String,
}

fn token_for(acc: &Accounts, kind: Auth) -> Option<(String, bool)> {
    if kind == Auth::Api {
        if let Some(t) = spotify_app::token(acc) {
            return Some((t, true));
        }
    }
    spotify_web::token(acc).map(|t| (t, false))
}

fn host(url: &str) -> &str {
    url.split("://").nth(1).and_then(|r| r.split('/').next()).unwrap_or(url)
}

/// api.spotify.com said 429: no calls to it before this. Calling while limited
/// only keeps the limit going (every call from 08:35 on was refused).
static API_BLOCKED_UNTIL: Mutex<Option<Instant>> = Mutex::new(None);

fn api_blocked() -> bool {
    API_BLOCKED_UNTIL.lock().unwrap().is_some_and(|t| Instant::now() < t)
}

/// GET returning the body; `None` for 204/404, rate limits, other errors, or no token.
/// One retry with a fresh token on 401.
fn get(acc: &Accounts, url: &str, kind: Auth, token: &Token) -> anyhow::Result<Option<String>> {
    let api = host(url) == "api.spotify.com";
    if api && api_blocked() {
        return Ok(None);
    }
    for attempt in 0..2 {
        token.check()?;
        let Some((tok, is_app)) = token_for(acc, kind) else { return Ok(None) };
        let auth = format!("Bearer {tok}");
        let mut headers = vec![("Authorization", auth.as_str()), ("Accept", "application/json")];
        if kind == Auth::WebPlayer {
            headers.push(("User-Agent", BROWSER_UA));
            headers.push(("app-platform", "WebPlayer"));
        }
        let res = http::get(&acc.agent, url, &headers)?;
        match res.status {
            401 if attempt == 0 => {
                log!("spotify: 401 from {}, refreshing the {} token", host(url), if is_app { "app" } else { "web player" });
                if is_app {
                    spotify_app::invalidate(acc);
                } else {
                    spotify_web::invalidate(acc);
                }
            }
            204 | 404 => return Ok(None),
            429 => {
                let wait = res.retry_after.as_deref().and_then(|r| r.trim().parse::<u64>().ok()).unwrap_or(30).clamp(1, 3600);
                if api {
                    *API_BLOCKED_UNTIL.lock().unwrap() = Some(Instant::now() + Duration::from_secs(wait));
                }
                log!("spotify: rate limited on {} (retry-after {wait}s); not calling it until then", host(url));
                return Ok(None);
            }
            s if !(200..300).contains(&s) => {
                log!("spotify: HTTP {s} from {}", url.split('?').next().unwrap_or(url));
                return Ok(None);
            }
            _ => return Ok((!res.body.is_empty()).then_some(res.body)),
        }
    }
    Ok(None)
}

fn read_track(t: &Value) -> Option<(TrackCandidate, String)> {
    if !t.is_object() || t.get("type").and_then(Value::as_str).unwrap_or("track") != "track" {
        return None;
    }
    let s = |v: &Value, k: &str| v.get(k).and_then(Value::as_str).unwrap_or("").to_string();
    let c = TrackCandidate {
        id: s(t, "id"),
        name: s(t, "name"),
        artists: t
            .get("artists")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .map(|a| s(a, "name"))
            .filter(|n| !n.is_empty())
            .collect(),
        duration_ms: t.get("duration_ms").and_then(Value::as_f64).unwrap_or(0.0),
    };
    let album = t.get("album").map(|a| s(a, "name")).unwrap_or_default();
    (!c.id.is_empty()).then_some((c, album))
}

/// What the account is playing right now (any device).
pub fn currently_playing(acc: &Accounts, token: &Token) -> anyhow::Result<Option<TrackCandidate>> {
    let Some(body) = get(acc, "https://api.spotify.com/v1/me/player/currently-playing", Auth::Api, token)? else {
        return Ok(None);
    };
    let doc: Value = serde_json::from_str(&body)?;
    Ok(doc.get("item").and_then(read_track).map(|(c, _)| c))
}

pub fn search(acc: &Accounts, q: &str, token: &Token) -> anyhow::Result<Vec<TrackCandidate>> {
    let url = format!("https://api.spotify.com/v1/search?type=track&limit=8&q={}", enc(q));
    let Some(body) = get(acc, &url, Auth::Api, token)? else { return Ok(Vec::new()) };
    let doc: Value = serde_json::from_str(&body)?;
    Ok(doc
        .pointer("/tracks/items")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(read_track)
        .map(|(c, _)| c)
        .collect())
}

/// The next tracks in the account's play queue (up to `max`).
pub fn queue(acc: &Accounts, max: usize, token: &Token) -> anyhow::Result<Vec<QueueItem>> {
    let Some(body) = get(acc, "https://api.spotify.com/v1/me/player/queue", Auth::Api, token)? else {
        return Ok(Vec::new());
    };
    let doc: Value = serde_json::from_str(&body)?;
    let mut list: Vec<QueueItem> = Vec::new();
    for t in doc.get("queue").and_then(Value::as_array).into_iter().flatten() {
        if let Some((track, album)) = read_track(t) {
            if !list.iter().any(|x| x.track.id == track.id) {
                list.push(QueueItem { track, album });
            }
        }
        if list.len() >= max {
            break;
        }
    }
    Ok(list)
}

/// Exact Spotify id for the playing song. With Spotify itself as the player, its
/// own currently-playing answer wins when the title agrees; otherwise the catalog
/// is searched by title, artist and duration.
pub fn find_track_id(acc: &Accounts, info: &TrackInfo, token: &Token) -> anyhow::Result<Option<String>> {
    let memo_key = crate::cache::key(&info.title, &info.artist);
    if let Some(id) = acc.id_memo.lock().unwrap().get(&memo_key) {
        return Ok(Some(id.clone()));
    }
    let remember = |id: &str| {
        acc.id_memo.lock().unwrap().insert(memo_key.clone(), id.to_string());
        Ok(Some(id.to_string()))
    };

    if info.is_spotify {
        if let Some(now) = currently_playing(acc, token)? {
            let n_name = matching::norm(&now.name);
            let n_title = matching::norm(&info.title);
            if !n_name.is_empty() && (n_name == n_title || n_name.contains(&n_title) || n_title.contains(&n_name)) {
                log!("spotify: playing \"{} - {}\" ({})", now.artists.first().map(String::as_str).unwrap_or(""), now.name, now.id);
                return remember(&now.id);
            }
            log!("spotify: currently-playing is \"{}\", not \"{}\"; searching instead", now.name, info.title);
        }
    }

    // One search, scored as the Android app does.
    let q = matching::query(info);
    if !q.title.is_empty() {
        let search_q = if q.artist.is_empty() {
            q.title.clone()
        } else {
            format!("track:{} artist:{}", q.title, matching::primary_artist(&q.artist))
        };
        let results = search(acc, &search_q, token)?;
        if let Some((best, score)) = best_match(&results, &q) {
            log!(
                "spotify: matched \"{} - {}\" (score {score:.2})",
                best.artists.first().map(String::as_str).unwrap_or(""),
                best.name
            );
            return remember(&best.id);
        }
    }
    log!("spotify: no confident match for \"{} - {}\"", info.artist, info.title);
    Ok(None)
}

/// The best-scoring result, unless every one is clearly another song.
fn best_match<'a>(results: &'a [TrackCandidate], q: &matching::Query) -> Option<(&'a TrackCandidate, f64)> {
    results
        .iter()
        .map(|t| (t, matching::match_score(q, &t.name, &t.artists.join(", "), t.duration_ms)))
        .filter(|(_, s)| *s > 0.0)
        .fold(None, |best: Option<(&TrackCandidate, f64)>, (t, s)| match best {
            Some((_, bs)) if bs >= s => best,
            _ => Some((t, s)),
        })
}

/// Spotify's own lyrics for a track id. Needs the web player sign-in.
pub fn lyrics(acc: &Accounts, track_id: &str, token: &Token) -> anyhow::Result<Option<Found>> {
    let url = format!(
        "https://spclient.wg.spotify.com/color-lyrics/v2/track/{track_id}?format=json&vocalRemoval=false&market=from_token"
    );
    let Some(body) = get(acc, &url, Auth::WebPlayer, token)? else { return Ok(None) };
    if body.trim().is_empty() {
        return Ok(None);
    }
    Ok(parse::from_spotify(&serde_json::from_str(&body)?))
}

/// Display name for the settings label.
pub fn user_name(acc: &Accounts, token: &Token) -> anyhow::Result<Option<String>> {
    let Some(body) = get(acc, "https://api.spotify.com/v1/me", Auth::Api, token)? else { return Ok(None) };
    let doc: Value = serde_json::from_str(&body)?;
    let field = |k: &str| doc.get(k).and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_string);
    Ok(field("display_name").or_else(|| field("id")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use matching::primary_artist;

    #[test]
    fn primary_artist_takes_the_first_name() {
        assert_eq!(primary_artist("A, B & C"), "A");
        assert_eq!(primary_artist("Calvin Harris feat. Rihanna"), "Calvin Harris");
        assert_eq!(primary_artist("Simon and Garfunkel"), "Simon");
        assert_eq!(primary_artist("Solo"), "Solo");
        assert_eq!(primary_artist("Beyoncé & JAY-Z"), "Beyoncé");
    }

    #[test]
    fn best_match_skips_same_titled_songs_by_others() {
        let t = |id: &str, name: &str, artists: &[&str], secs: f64| TrackCandidate {
            id: id.into(),
            name: name.into(),
            artists: artists.iter().map(|s| s.to_string()).collect(),
            duration_ms: secs * 1000.0,
        };
        let info = TrackInfo {
            title: "Phoenix".into(),
            artist: "League of Legends".into(),
            duration_ms: 184_000.0,
            is_spotify: true,
            source_app: "Spotify.exe".into(),
            ..Default::default()
        };
        let q = matching::query(&info);
        let results = [
            t("3yLOy7bjC1dDeOh4MArkq8", "Phoenix", &["Stray Kids"], 182.013),
            t("68dbDYr1axaxScBlWp8dI1", "Phoenix - Blanke Remix", &["League of Legends", "Cailin Russo", "Chrissy Costanza", "Blanke"], 208.571),
            t("1jsRo6lODprC33SNqTvLGz", "Phoenix", &["League of Legends", "Cailin Russo", "Chrissy Costanza"], 197.633),
        ];
        assert_eq!(best_match(&results, &q).map(|(b, _)| b.id.as_str()), Some("1jsRo6lODprC33SNqTvLGz"));
        assert!(best_match(&results[..1], &q).is_none());
    }

    #[test]
    fn reads_tracks_and_skips_episodes() {
        let t: Value = serde_json::from_str(
            r#"{"type":"track","id":"abc","name":"Song","duration_ms":1000,"artists":[{"name":"A"},{"name":""}],"album":{"name":"Al"}}"#,
        )
        .unwrap();
        let (c, album) = read_track(&t).unwrap();
        assert_eq!((c.id.as_str(), c.name.as_str(), c.artists.len(), album.as_str()), ("abc", "Song", 1, "Al"));
        assert!(read_track(&serde_json::json!({"type":"episode","id":"x"})).is_none());
        assert_eq!(host("https://api.spotify.com/v1/me"), "api.spotify.com");
    }
}
