//! Resolves a track to synced lyrics, as the Android app's `LyricsRepository`:
//!   0. the disk cache (final unless a provider it never asked is available now),
//!   then every provider at once, in priority order:
//!   1. "spicy-api":  Spicy Lyrics developer API with the user's own key
//!   2. "spotify":    Spotify's own lyrics, when signed in to the web player
//!   3. "musixmatch": Musixmatch (word timing from "richsync"), no setup
//!   4. "netease":    NetEase Cloud Music (word timing from "YRC"), no setup
//!   5. "lrclib":     LRCLIB, no setup
//!
//! Word timing beats line timing; within the same kind the earlier provider wins.
//! The answer is final once no provider still running could beat it; when a
//! better one is slow, the best so far is shown after `GRACE`.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use syllable_core::{log, matching, TrackInfo};

use crate::accounts::Accounts;
use crate::cache::{
    self, beats, is_final, now_secs, priority, was_tried, Cache, Entry, LRCLIB, MUSIXMATCH, NETEASE, SPICY_API, SPOTIFY,
};
use crate::http::{is_cancelled, Token};
use crate::parse::Found;
use crate::{lrclib, musixmatch, netease, spicy, spotify_api};

/// Show the best answer so far if a better source is this slow, then upgrade when it lands.
const GRACE: Duration = Duration::from_millis(3500);
/// A provider slower than this is given up on for this lookup (not counted as asked).
const PROVIDER_TIMEOUT: Duration = Duration::from_secs(30);
/// Rapid skipping shouldn't fire a search per skipped track.
pub const DEBOUNCE: Duration = Duration::from_millis(600);

pub type Interim = Arc<dyn Fn(&Found) + Send + Sync>;

pub struct Resolver {
    pub acc: Arc<Accounts>,
    pub cache: Cache,
}

/// Spotify ids for one lookup, each fetched once and shared by the providers.
struct Ids {
    acc: Arc<Accounts>,
    info: TrackInfo,
    token: Token,
    spicy_key: Option<String>,
    spotify: Mutex<Option<Option<String>>>,
    spicy: Mutex<Option<Vec<String>>>,
}

impl Ids {
    /// The exact id: the player's, or Spotify's own lookup (needs a Spotify sign-in).
    fn spotify_id(&self) -> anyhow::Result<Option<String>> {
        if let Some(id) = &self.info.spotify_track_id {
            return Ok(Some(id.clone()));
        }
        if !self.acc.has_spotify() {
            return Ok(None);
        }
        let mut memo = self.spotify.lock().unwrap();
        if let Some(v) = memo.clone() {
            return Ok(v);
        }
        let v = spotify_api::find_track_id(&self.acc, &self.info, &self.token)?;
        *memo = Some(v.clone());
        Ok(v)
    }

    /// Spicy's confident search matches (Spotify ids), best first.
    fn spicy_search(&self) -> anyhow::Result<Vec<String>> {
        let Some(key) = &self.spicy_key else { return Ok(Vec::new()) };
        let mut memo = self.spicy.lock().unwrap();
        if let Some(v) = memo.clone() {
            return Ok(v);
        }
        let v = spicy::search(&self.acc.agent, key, &self.info, &self.token)?;
        *memo = Some(v.clone());
        Ok(v)
    }
}

/// One provider's answer: `Ok(None)` is "they have nothing", `Err` is "couldn't ask".
fn ask(provider: &str, ids: &Ids) -> anyhow::Result<Option<Found>> {
    let (acc, info, token) = (&ids.acc, &ids.info, &ids.token);
    match provider {
        SPICY_API => match &ids.spicy_key {
            Some(key) => {
                let lookup = || ids.spotify_id();
                let lookup_ref: Option<&dyn Fn() -> anyhow::Result<Option<String>>> =
                    if acc.has_spotify() { Some(&lookup) } else { None };
                spicy::fetch(&acc.agent, key, info.spotify_track_id.as_deref(), lookup_ref, &|| ids.spicy_search(), token)
            }
            None => Ok(None),
        },
        SPOTIFY => {
            // Spotify's catalog lookup (api.spotify.com) is often rate-limited while its
            // lyrics endpoint is not; Spicy's search knows the track's Spotify id too.
            let id = match ids.spotify_id()? {
                Some(id) => Some(id),
                None => ids.spicy_search()?.into_iter().next(),
            };
            match id {
                Some(id) => spotify_api::lyrics(acc, &id, token),
                None => Ok(None),
            }
        }
        MUSIXMATCH => musixmatch::fetch(&acc.agent, info, token),
        NETEASE => netease::fetch(&acc.agent, info, token),
        LRCLIB => lrclib::fetch(&acc.agent, info, token),
        _ => Ok(None),
    }
}

/// Whether a provider that hasn't answered could still give something better than `best`.
fn could_beat(provider: &str, best: Option<&Found>) -> bool {
    match best {
        None => true,
        Some(b) => b.rank() < 3 || priority(provider) < priority(b.source()),
    }
}

impl Resolver {
    /// Provider ids usable right now, in priority order.
    pub fn available(&self) -> Vec<&'static str> {
        let mut list = Vec::new();
        if self.acc.spicy_key().is_some() {
            list.push(SPICY_API);
        }
        if self.acc.spotify_signed_in() {
            list.push(SPOTIFY);
        }
        list.extend([MUSIXMATCH, NETEASE, LRCLIB]);
        list
    }

    /// The published answer: synced lyrics, or `None`. `interim` gets earlier,
    /// possibly worse, answers while better providers are still being asked.
    pub fn resolve(&self, info: &TrackInfo, token: &Token, interim: Option<Interim>, prefetch: bool) -> anyhow::Result<Option<Found>> {
        if !info.is_spotify && !matching::is_identifiable(&info.title, &info.artist) {
            log!("resolve: skipping \"{}\" (no artist to match on)", info.title);
            return Ok(None);
        }

        let cache_key = cache::key(&info.title, &info.artist);
        let available = self.available();
        let cached = self.cache.load(&cache_key, now_secs());
        let mut tried: Vec<String> = Vec::new();
        let mut shown: Option<Found> = None;
        if let Some(c) = &cached {
            let (fin, t) = is_final(c, &available, now_secs());
            tried = t;
            let what = if c.found.lyrics.lines.is_empty() {
                "negative".to_string()
            } else {
                format!("{}, {}", c.found.source(), c.found.kind_name())
            };
            if fin {
                if !prefetch {
                    log!("resolve: cache hit ({what})");
                }
                return Ok(Some(c.found.clone()).filter(|f| f.rank() >= 2));
            }
            let upgrades: Vec<&str> = available.iter().copied().filter(|p| !was_tried(&tried, p)).collect();
            log!("resolve: cached {what} may upgrade (now available: {})", upgrades.join(", "));
            // Show what we have while the better providers are asked.
            if c.found.rank() >= 2 && !prefetch {
                if let Some(f) = &interim {
                    f(&c.found);
                    shown = Some(c.found.clone());
                }
            }
        } else if prefetch {
            log!("prefetch: resolving \"{} - {}\"", info.artist, info.title);
        }

        if !prefetch && info.spotify_track_id.is_none() {
            token.sleep(DEBOUNCE)?;
        }

        // Every provider not yet asked for this song, all at once. Their token also
        // stops once the answer is settled, so the slow ones give up.
        let stop = Arc::new(AtomicBool::new(false));
        let ids = Arc::new(Ids {
            acc: self.acc.clone(),
            info: info.clone(),
            token: token.with_stop(stop.clone()),
            spicy_key: self.acc.spicy_key(),
            spotify: Mutex::new(None),
            spicy: Mutex::new(None),
        });
        let mut pending: Vec<&'static str> = available.iter().copied().filter(|p| !was_tried(&tried, p)).collect();
        let (tx, rx) = mpsc::channel::<(&'static str, anyhow::Result<Option<Found>>)>();
        pending.retain(|&p| {
            let (tx, ids) = (tx.clone(), ids.clone());
            std::thread::Builder::new()
                .name(format!("lyrics-{p}"))
                .spawn(move || {
                    let _ = tx.send((p, ask(p, &ids)));
                })
                .is_ok()
        });
        drop(tx);

        let started = Instant::now();
        let mut best: Option<Found> = cached.as_ref().filter(|c| c.found.rank() >= 2).map(|c| c.found.clone());
        let mut newly_tried: Vec<String> = Vec::new();
        let outcome = loop {
            if pending.iter().all(|p| !could_beat(p, best.as_ref())) {
                break Ok(());
            }
            if let Err(e) = token.check() {
                break Err(e);
            }
            if started.elapsed() >= PROVIDER_TIMEOUT {
                log!("resolve: gave up waiting for {}", pending.join(", "));
                break Ok(());
            }
            if let (false, Some(f), Some(b)) = (prefetch, &interim, &best) {
                if started.elapsed() >= GRACE && shown.as_ref() != Some(b) {
                    log!("resolve: better sources are slow, showing {} for now", b.source());
                    f(b);
                    shown = Some(b.clone());
                }
            }
            match rx.recv_timeout(Duration::from_millis(50)) {
                Ok((p, got)) => {
                    pending.retain(|x| *x != p);
                    match got {
                        Ok(got) => {
                            newly_tried.push(p.to_string());
                            log!("resolve: {p} -> {}", got.as_ref().map(Found::describe).unwrap_or_else(|| "nothing".into()));
                            if beats(got.as_ref(), best.as_ref()) {
                                best = got;
                            }
                        }
                        Err(e) if is_cancelled(&e) => {}
                        // An error isn't an answer: leave it out of "tried" so the next play asks again.
                        Err(e) => log!("resolve: {p} failed: {e:#}"),
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break Ok(()),
            }
        };
        stop.store(true, Ordering::SeqCst);
        outcome?;
        token.check()?;

        let mut all_tried = tried.clone();
        for t in &newly_tried {
            if !all_tried.contains(t) {
                all_tried.push(t.clone());
            }
        }
        // Providers still running that can't beat the answer count as asked (not
        // ones given up on as too slow: they get another turn next time).
        if best.as_ref().is_some_and(|b| b.rank() >= 2) {
            for p in pending.iter().filter(|p| !could_beat(p, best.as_ref())) {
                if !all_tried.iter().any(|t| t == p) {
                    all_tried.push(p.to_string());
                }
            }
        }
        let elapsed = started.elapsed().as_millis();
        let label = if prefetch { "prefetch" } else { "resolve" };
        // Unsynced lyrics are never shown: the renderers are time-based.
        let shown = best.filter(|b| b.rank() >= 2);
        match &shown {
            Some(s) => {
                let origin = s.origin.as_deref().map(|o| format!(", {o}")).unwrap_or_default();
                let credits = s.credits.as_ref().map(|c| format!(", {}", c.join(", "))).unwrap_or_default();
                log!(
                    "{label}: answered by {} ({}{origin}, {} lines{credits}) in {elapsed} ms",
                    s.source(),
                    s.kind_name(),
                    s.lyrics.lines.len()
                );
                let same_as_cached = cached.as_ref().is_some_and(|c| c.found == *s);
                if !same_as_cached || !newly_tried.is_empty() {
                    // Re-saving a cached entry keeps its fetch time, so the Spicy 30-day
                    // limit counts from when the lyrics were downloaded.
                    let fetched_at = match &cached {
                        Some(c) if same_as_cached && c.fetched_at.is_some() => c.fetched_at,
                        _ => Some(now_secs()),
                    };
                    self.cache.save(&cache_key, &Entry { found: s.clone(), tried: Some(all_tried), fetched_at });
                }
            }
            None => {
                let asked = if newly_tried.is_empty() { "nobody".to_string() } else { newly_tried.join(", ") };
                log!("{label}: no synced lyrics found (asked {asked})");
                // Cache the negative only when some provider answered: an offline moment must not stick.
                if !newly_tried.is_empty() {
                    self.cache.save(&cache_key, &Entry { found: Found::negative(), tried: Some(all_tried), fetched_at: Some(now_secs()) });
                }
            }
        }
        Ok(shown)
    }

    /// Cache lyrics for the next songs in Spotify's queue. Needs a Spotify connection
    /// and Spotify as the player (the queue belongs to the account, not to a browser tab).
    pub fn prefetch(&self, current: &TrackInfo, token: &Token) {
        let run = || -> anyhow::Result<()> {
            token.sleep(Duration::from_millis(1500))?; // after the current song's own requests
            let queue = spotify_api::queue(&self.acc, 2, token)?;
            if queue.is_empty() {
                log!("prefetch: queue empty or unavailable");
                return Ok(());
            }
            for item in queue {
                token.check()?;
                let t = &item.track;
                if matching::norm(&t.name) == matching::norm(&current.title) {
                    continue;
                }
                // Spotify desktop's media session carries the full track name and only
                // the first artist, so that's the cache key a track change will look up.
                let next = TrackInfo {
                    title: t.name.clone(),
                    artist: t.artists.first().cloned().unwrap_or_default(),
                    album: item.album.clone(),
                    duration_ms: t.duration_ms,
                    is_spotify: true,
                    spotify_track_id: Some(t.id.clone()),
                    ..Default::default()
                };
                let l = self.resolve(&next, token, None, true)?;
                // Also under every artist, in case a Spotify build reports them all.
                if let (Some(l), true) = (l, t.artists.len() > 1) {
                    self.cache.save(
                        &cache::key(&t.name, &t.artists.join(", ")),
                        &Entry { found: l, tried: Some(self.available().iter().map(|s| s.to_string()).collect()), fetched_at: Some(now_secs()) },
                    );
                }
            }
            Ok(())
        };
        match run() {
            Ok(()) => {}
            Err(e) if is_cancelled(&e) => {}
            Err(e) => log!("prefetch: error: {e:#}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::Kind;
    use syllable_core::models::{LyricLine, Lyrics};

    fn found(kind: Kind, source: &str) -> Found {
        Found {
            lyrics: Lyrics { source: source.into(), lines: vec![LyricLine { text: "x".into(), end: 1.0, ..Default::default() }], ..Default::default() },
            kind,
            origin: None,
            credits: None,
        }
    }

    /// "League of Legends - Phoenix": Spicy has line timing only, so the lookup
    /// keeps waiting for the word-timed sources, as the Android app does.
    #[test]
    fn a_line_answer_waits_for_word_timed_sources() {
        let spicy_line = found(Kind::Line, SPICY_API);
        for p in [SPOTIFY, MUSIXMATCH, NETEASE, LRCLIB] {
            assert!(could_beat(p, Some(&spicy_line)), "{p}");
        }
        let mxm_word = found(Kind::Syllable, MUSIXMATCH);
        assert!(beats(Some(&mxm_word), Some(&spicy_line)));
        // Once a word-timed answer is in, only earlier providers can still win.
        assert!(could_beat(SPICY_API, Some(&mxm_word)));
        assert!(could_beat(SPOTIFY, Some(&mxm_word)));
        assert!(!could_beat(NETEASE, Some(&mxm_word)));
        assert!(!could_beat(LRCLIB, Some(&mxm_word)));
        // Same kind: the earlier provider wins.
        assert!(beats(Some(&found(Kind::Syllable, SPOTIFY)), Some(&mxm_word)));
        assert!(!beats(Some(&found(Kind::Syllable, NETEASE)), Some(&mxm_word)));
        assert!(could_beat(LRCLIB, None));
    }
}
