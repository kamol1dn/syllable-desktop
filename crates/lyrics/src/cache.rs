//! Disk cache of resolved lyrics and the rules for when an entry is final.
//! One JSON file per song under `<data_dir>/cache/`, named by a hash of the
//! normalised title and artist.

use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use syllable_core::matching;

use crate::parse::Found;

pub const SPICY_API: &str = "spicy-api";
pub const SPOTIFY: &str = "spotify";
pub const LRCLIB: &str = "lrclib";
/// The prototype's spicetify bridge. No longer a provider, but old cache entries
/// may name it and it serves the same data as the Spicy API.
pub const BRIDGE: &str = "spicy";

/// Earlier wins ties.
const PRIORITY: [&str; 4] = [BRIDGE, SPICY_API, SPOTIFY, LRCLIB];

/// A "nothing found" is asked again after this long (songs get synced over time).
pub const NEGATIVE_MAX_AGE_SECS: f64 = 3.0 * 86_400.0;
/// Spicy Lyrics' terms allow keeping a response for at most 30 days.
pub const SPICY_API_MAX_AGE_SECS: f64 = 30.0 * 86_400.0;

pub fn now_secs() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0)
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    /// No lines means a cached "nothing found" (source "none").
    pub found: Found,
    /// Providers asked when this was cached, so one set up later gets its turn.
    /// `None` on entries from before this was tracked.
    #[serde(default)]
    pub tried: Option<Vec<String>>,
    /// Unix seconds of the last provider answer. Drives the age limits.
    #[serde(default)]
    pub fetched_at: Option<f64>,
}

/// Word timing beats line timing; at the same kind the earlier provider wins.
pub fn beats(candidate: Option<&Found>, current: Option<&Found>) -> bool {
    let Some(c) = candidate else { return false };
    if c.rank() == 0 {
        return false;
    }
    match current {
        None => true,
        Some(cur) if c.rank() != cur.rank() => c.rank() > cur.rank(),
        Some(cur) => {
            let p = |s: &str| PRIORITY.iter().position(|x| *x == s).unwrap_or(PRIORITY.len());
            p(c.source()) < p(cur.source())
        }
    }
}

/// Both Spicy routes serve the same data; asking one counts as asking the other.
pub fn was_tried(tried: &[String], id: &str) -> bool {
    let has = |x: &str| tried.iter().any(|t| t == x);
    has(id) || ((id == BRIDGE || id == SPICY_API) && (has(BRIDGE) || has(SPICY_API)))
}

/// Whether a cache entry stands as is, or a provider it never asked could still
/// beat it. Also returns the providers that need not be asked again.
pub fn is_final(cached: &Entry, available: &[&str], now: f64) -> (bool, Vec<String>) {
    let f = &cached.found;
    if f.lyrics.lines.is_empty() {
        if let Some(at) = cached.fetched_at {
            if now - at > NEGATIVE_MAX_AGE_SECS {
                return (false, Vec::new()); // stale miss: ask everyone again
            }
        }
    }
    // Entries from before "tried": spicy ones were final, everything else was retried.
    let mut tried = cached.tried.clone().unwrap_or_else(|| {
        if f.source() == BRIDGE { vec![BRIDGE.into(), LRCLIB.into()] } else { vec![LRCLIB.into()] }
    });
    // LRCLIB tops out at line timing and loses ties to every earlier provider.
    if f.rank() >= 2 && !tried.iter().any(|t| t == LRCLIB) {
        tried.push(LRCLIB.into());
    }
    if f.rank() >= 3 || (cached.tried.is_none() && f.source() == BRIDGE) {
        return (true, tried);
    }
    let fin = available.iter().all(|p| was_tried(&tried, p));
    (fin, tried)
}

/// Metadata-only key: stable while the track id and duration trickle in.
pub fn key(title: &str, artist: &str) -> String {
    format!("{}|{}", matching::norm(title), matching::norm(artist))
}

#[derive(Clone)]
pub struct Cache {
    dir: PathBuf,
}

impl Cache {
    pub fn new(dir: PathBuf) -> Self {
        let _ = fs::create_dir_all(&dir);
        Self { dir }
    }

    fn path(&self, key: &str) -> PathBuf {
        let hash = Sha256::digest(key.as_bytes());
        let hex: String = hash.iter().take(10).map(|b| format!("{b:02x}")).collect();
        self.dir.join(format!("{hex}.json"))
    }

    pub fn load(&self, key: &str, now: f64) -> Option<Entry> {
        let p = self.path(key);
        let text = fs::read_to_string(&p).ok()?;
        let mut e: Entry = serde_json::from_str(&text).ok()?;
        if e.fetched_at.is_none() {
            e.fetched_at = fs::metadata(&p)
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_secs_f64());
        }
        if e.found.source() == SPICY_API && e.fetched_at.is_some_and(|at| now - at > SPICY_API_MAX_AGE_SECS) {
            let _ = fs::remove_file(&p);
            return None;
        }
        Some(e)
    }

    pub fn save(&self, key: &str, entry: &Entry) {
        let p = self.path(key);
        let tmp = p.with_extension("json.tmp");
        let result = serde_json::to_string(entry)
            .map_err(std::io::Error::other)
            .and_then(|t| fs::write(&tmp, t))
            .and_then(|_| fs::rename(&tmp, &p));
        if let Err(e) = result {
            syllable_core::log!("cache: save failed: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    //! Ported from TaskbarLyrics.Tests/CacheAndAuthTests.cs (CacheFinalityTests).
    use super::*;
    use crate::parse::Kind;
    use syllable_core::models::{LyricLine, Lyrics};

    const NOW: f64 = 1_795_262_400.0; // 2026-11-21 12:00 UTC

    fn found(kind: Kind, source: &str, lines: usize) -> Found {
        Found {
            lyrics: Lyrics {
                source: source.into(),
                lines: (0..lines).map(|_| LyricLine { text: "x".into(), start: 0.0, end: 1.0, ..Default::default() }).collect(),
                ..Default::default()
            },
            kind,
            origin: None,
            credits: None,
        }
    }

    fn line(source: &str, tried: &[&str]) -> Entry {
        Entry {
            found: found(Kind::Line, source, 1),
            tried: Some(tried.iter().map(|s| s.to_string()).collect()),
            fetched_at: Some(NOW - 86_400.0),
        }
    }

    fn legacy(source: &str) -> Entry {
        Entry { found: found(Kind::Line, source, 1), tried: None, fetched_at: None }
    }

    #[test]
    fn line_from_spicy_api_is_final_once_asked() {
        // Saved with LRCLIB skipped (it can't beat a Spicy line): must not re-ask on every play.
        let (fin, tried) = is_final(&line(SPICY_API, &[SPICY_API]), &[SPICY_API, LRCLIB], NOW);
        assert!(fin);
        assert!(tried.iter().any(|t| t == LRCLIB));
    }

    #[test]
    fn lrclib_line_upgrades_when_a_key_appears() {
        let cached = line(LRCLIB, &[LRCLIB]);
        assert!(is_final(&cached, &[LRCLIB], NOW).0);
        let (fin, tried) = is_final(&cached, &[SPICY_API, LRCLIB], NOW);
        assert!(!fin);
        assert!(!tried.iter().any(|t| t == SPICY_API));
    }

    #[test]
    fn bridge_and_api_count_as_the_same_source() {
        assert!(is_final(&line(LRCLIB, &[BRIDGE, LRCLIB]), &[SPICY_API, LRCLIB], NOW).0);
    }

    #[test]
    fn legacy_entries() {
        assert!(is_final(&legacy(BRIDGE), &[SPICY_API, SPOTIFY, LRCLIB], NOW).0);
        let lrc = legacy(LRCLIB);
        assert!(is_final(&lrc, &[LRCLIB], NOW).0);
        assert!(!is_final(&lrc, &[BRIDGE, LRCLIB], NOW).0);
        assert!(!is_final(&lrc, &[SPICY_API, LRCLIB], NOW).0);
    }

    #[test]
    fn negative_expires() {
        let mut miss = Entry { found: Found::negative(), tried: Some(vec![LRCLIB.into()]), fetched_at: Some(NOW - 86_400.0) };
        assert!(is_final(&miss, &[LRCLIB], NOW).0);
        miss.fetched_at = Some(NOW - NEGATIVE_MAX_AGE_SECS - 3600.0);
        let (fin, tried) = is_final(&miss, &[LRCLIB], NOW);
        assert!(!fin);
        assert!(tried.is_empty());
    }

    #[test]
    fn word_timing_is_always_final() {
        let word = Entry { found: found(Kind::Syllable, SPOTIFY, 1), tried: Some(vec![SPOTIFY.into()]), fetched_at: None };
        assert!(is_final(&word, &[SPICY_API, SPOTIFY, LRCLIB], NOW).0);
    }

    #[test]
    fn earlier_provider_wins_ties() {
        let api = found(Kind::Line, SPICY_API, 1);
        let lrc = found(Kind::Line, LRCLIB, 1);
        let word = found(Kind::Syllable, LRCLIB, 1);
        let unsynced = found(Kind::Static, SPICY_API, 1);
        assert!(beats(Some(&api), Some(&lrc)));
        assert!(!beats(Some(&lrc), Some(&api)));
        assert!(beats(Some(&word), Some(&api)));
        assert!(!beats(None, Some(&lrc)));
        assert!(beats(Some(&lrc), None));
        assert!(beats(Some(&lrc), Some(&unsynced)));
        assert!(!beats(Some(&found(Kind::Line, SPICY_API, 0)), None));
    }

    #[test]
    fn disk_round_trip_and_spicy_age_limit() {
        let dir = std::env::temp_dir().join(format!("syllable-lyrics-cache-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let cache = Cache::new(dir.clone());
        let k = key("Blinding Lights", "The Weeknd");
        assert_eq!(k, "blinding lights|the weeknd");
        assert!(cache.load(&k, NOW).is_none());

        let e = line(LRCLIB, &[LRCLIB]);
        cache.save(&k, &e);
        assert_eq!(cache.load(&k, NOW), Some(e));

        let old = Entry { fetched_at: Some(NOW - SPICY_API_MAX_AGE_SECS - 1.0), ..line(SPICY_API, &[SPICY_API]) };
        cache.save(&k, &old);
        assert!(cache.load(&k, NOW).is_none(), "Spicy API answers expire after 30 days");
        assert!(cache.load(&k, NOW).is_none(), "and the file is deleted");
        let _ = fs::remove_dir_all(&dir);
    }
}
