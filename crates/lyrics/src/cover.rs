//! Spotify's own cover at 640 px, for Spotify songs.
//!
//! The media session only carries Spotify desktop's ~300 px thumbnail, which looks
//! soft once the wallpaper scales it up. With the song's Spotify id, Spotify's public
//! embed endpoint (`open.spotify.com/oembed`, no sign-in, not the rate-limited Web API)
//! names the cover on its image CDN at 300 px; the same image is served at 640 px under
//! the `ab67616d0000b273` size prefix.
//!
//! The id comes from the player (when it gives one), from an earlier lookup of the same
//! song (kept on disk, so a song played again needs no search), from Spicy Lyrics'
//! search, or from Spotify's own lookup when signed in.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;

use serde_json::Value;
use syllable_core::{log, TrackInfo};

use crate::accounts::Accounts;
use crate::http::{self, Token};
use crate::{cache, spicy, spotify_api};

const SMALL: &str = "ab67616d00001e02"; // 300 px
const LARGE: &str = "ab67616d0000b273"; // 640 px
const MAX_BYTES: u64 = 4 * 1024 * 1024;

/// Spotify ids by song (`cache::key`), saved as JSON beside the lyrics cache.
pub struct Ids {
    path: PathBuf,
    map: Mutex<HashMap<String, String>>,
}

impl Ids {
    pub fn load(path: PathBuf) -> Self {
        let map = std::fs::read_to_string(&path).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default();
        Self { path, map: Mutex::new(map) }
    }

    fn get(&self, key: &str) -> Option<String> {
        self.map.lock().unwrap().get(key).cloned()
    }

    fn remember(&self, key: &str, id: &str) {
        let mut map = self.map.lock().unwrap();
        if map.get(key).map(String::as_str) == Some(id) {
            return;
        }
        map.insert(key.to_string(), id.to_string());
        if let Ok(json) = serde_json::to_string(&*map) {
            if let Some(dir) = self.path.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            let tmp = self.path.with_extension("json.tmp");
            if std::fs::write(&tmp, json).is_ok() {
                let _ = std::fs::rename(&tmp, &self.path);
            }
        }
    }
}

/// The cover's bytes, or `None` when the song's id or its cover can't be found.
pub fn fetch(acc: &Accounts, ids: &Ids, info: &TrackInfo, token: &Token) -> anyhow::Result<Option<Vec<u8>>> {
    let Some(id) = track_id(acc, ids, info, token)? else { return Ok(None) };
    token.check()?;
    let url = format!("https://open.spotify.com/oembed?url={}", http::enc(&format!("https://open.spotify.com/track/{id}")));
    let res = http::get(&acc.agent, &url, &[])?;
    if !res.ok() {
        log!("cover: oembed {} for {id}", res.status);
        return Ok(None);
    }
    let doc: Value = serde_json::from_str(&res.body).unwrap_or(Value::Null);
    let Some(large) = doc.get("thumbnail_url").and_then(Value::as_str).and_then(large_url) else {
        log!("cover: no thumbnail for {id}");
        return Ok(None);
    };
    token.check()?;
    let (status, bytes) = http::get_bytes(&acc.agent, &large, MAX_BYTES)?;
    if status != 200 || !is_image(&bytes) {
        log!("cover: {status} for {large}");
        return Ok(None);
    }
    log!("cover: {} bytes from Spotify (640 px) for \"{} - {}\"", bytes.len(), info.artist, info.title);
    Ok(Some(bytes))
}

fn track_id(acc: &Accounts, ids: &Ids, info: &TrackInfo, token: &Token) -> anyhow::Result<Option<String>> {
    let key = cache::key(&info.title, &info.artist);
    if let Some(id) = info.spotify_track_id.clone().filter(|id| spicy::is_spotify_id(id)) {
        ids.remember(&key, &id);
        return Ok(Some(id));
    }
    if let Some(id) = ids.get(&key) {
        return Ok(Some(id));
    }
    let mut found = None;
    if let Some(spicy_key) = acc.spicy_key() {
        found = spicy::search(&acc.agent, &spicy_key, info, token)?.into_iter().next();
    }
    if found.is_none() && acc.has_spotify() {
        found = spotify_api::find_track_id(acc, info, token).unwrap_or_else(|e| {
            if !http::is_cancelled(&e) {
                log!("cover: spotify lookup failed: {e:#}");
            }
            None
        });
    }
    let found = found.filter(|id| spicy::is_spotify_id(id));
    if let Some(id) = &found {
        ids.remember(&key, id);
    }
    Ok(found)
}

/// The 640 px address of a Spotify album-art thumbnail, or `None` if it isn't one.
fn large_url(thumb: &str) -> Option<String> {
    let host_ok = thumb.starts_with("https://i.scdn.co/image/") || (thumb.starts_with("https://image-cdn-") && thumb.contains(".spotifycdn.com/image/"));
    if !host_ok {
        return None;
    }
    Some(if thumb.contains(SMALL) { thumb.replacen(SMALL, LARGE, 1) } else { thumb.to_string() })
}

fn is_image(b: &[u8]) -> bool {
    b.starts_with(&[0xFF, 0xD8, 0xFF]) || b.starts_with(&[0x89, b'P', b'N', b'G'])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thumbnail_becomes_640() {
        assert_eq!(
            large_url("https://image-cdn-ak.spotifycdn.com/image/ab67616d00001e02baf89eb11ec7c657805d2da0").as_deref(),
            Some("https://image-cdn-ak.spotifycdn.com/image/ab67616d0000b273baf89eb11ec7c657805d2da0")
        );
        assert_eq!(
            large_url("https://i.scdn.co/image/ab67616d00001e02abc").as_deref(),
            Some("https://i.scdn.co/image/ab67616d0000b273abc")
        );
        assert_eq!(large_url("https://example.com/image/ab67616d00001e02abc"), None);
        assert_eq!(large_url("http://i.scdn.co/image/ab67616d00001e02abc"), None);
    }

    #[test]
    fn ids_survive_a_restart() {
        let dir = std::env::temp_dir().join(format!("syllable-cover-test-{}", std::process::id()));
        let path = dir.join("cover-ids.json");
        let _ = std::fs::remove_file(&path);
        let ids = Ids::load(path.clone());
        ids.remember("a|b", "0VjIjW4GlUZAMYd2vXMi3b");
        let again = Ids::load(path);
        assert_eq!(again.get("a|b").as_deref(), Some("0VjIjW4GlUZAMYd2vXMi3b"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn only_images_count() {
        assert!(is_image(&[0xFF, 0xD8, 0xFF, 0xE0]));
        assert!(!is_image(b"<html>"));
    }
}
