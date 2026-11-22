//! Resolves a track to synced lyrics. Order:
//!   0. disk cache (final unless a provider it never asked is available now)
//!   1. "spicy-api": Spicy Lyrics developer API with the user's own key
//!   2. "spotify":   Spotify's own lyrics, when signed in to the web player
//!   3. "lrclib":    LRCLIB, no setup (runs in parallel; shown early if the others are slow)
//! Word timing beats line timing; within the same kind the earlier provider wins.

use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use syllable_core::{log, matching, TrackInfo};

use crate::accounts::Accounts;
use crate::cache::{self, beats, is_final, now_secs, was_tried, Cache, Entry, LRCLIB, SPICY_API, SPOTIFY};
use crate::http::{is_cancelled, Token};
use crate::parse::Found;
use crate::{lrclib, spicy, spotify_api};

/// Show LRCLIB's answer if better sources are this slow, then upgrade when they land.
const GRACE: Duration = Duration::from_millis(3500);
/// Rapid skipping shouldn't fire a search per skipped track.
pub const DEBOUNCE: Duration = Duration::from_millis(600);

pub type Interim = Arc<dyn Fn(&Found) + Send + Sync>;

pub struct Resolver {
    pub acc: Arc<Accounts>,
    pub cache: Cache,
}

/// LRCLIB's answer, filled by its own thread.
#[derive(Default)]
struct Slot {
    value: Mutex<Option<Result<Option<Found>, String>>>,
    ready: Condvar,
}

impl Slot {
    fn put(&self, v: Result<Option<Found>, String>) {
        *self.value.lock().unwrap() = Some(v);
        self.ready.notify_all();
    }

    fn wait(&self, token: &Token) -> anyhow::Result<Result<Option<Found>, String>> {
        let mut guard = self.value.lock().unwrap();
        loop {
            if let Some(v) = guard.as_ref() {
                return Ok(v.clone());
            }
            token.check()?;
            guard = self.ready.wait_timeout(guard, Duration::from_millis(50)).unwrap().0;
        }
    }
}

impl Resolver {
    /// Provider ids usable right now, in priority order (LRCLIB always last).
    pub fn available(&self) -> Vec<&'static str> {
        let mut list = Vec::new();
        if self.acc.spicy_key().is_some() {
            list.push(SPICY_API);
        }
        if self.acc.spotify_signed_in() {
            list.push(SPOTIFY);
        }
        list.push(LRCLIB);
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
                }
            }
        } else if prefetch {
            log!("prefetch: resolving \"{} - {}\"", info.artist, info.title);
        }

        if !prefetch && info.spotify_track_id.is_none() {
            token.sleep(DEBOUNCE)?;
        }

        // The Spotify track id from an account lookup, shared by the Spicy API and Spotify providers.
        let id_memo: Mutex<Option<Option<String>>> = Mutex::new(None);
        let spotify_track_id = || -> anyhow::Result<Option<String>> {
            if let Some(id) = &info.spotify_track_id {
                return Ok(Some(id.clone()));
            }
            if !self.acc.has_spotify() {
                return Ok(None);
            }
            if let Some(v) = id_memo.lock().unwrap().clone() {
                return Ok(v);
            }
            let v = spotify_api::find_track_id(&self.acc, info, token)?;
            *id_memo.lock().unwrap() = Some(v.clone());
            Ok(v)
        };

        // LRCLIB runs alongside the better providers so a slow one doesn't leave the screen empty.
        let slot = Arc::new(Slot::default());
        match &cached {
            Some(c) if was_tried(&tried, LRCLIB) && c.found.source() == LRCLIB => slot.put(Ok(Some(c.found.clone()))),
            _ => {
                let (slot, agent, info, token) = (slot.clone(), self.acc.agent.clone(), info.clone(), token.clone());
                std::thread::spawn(move || slot.put(lrclib::fetch(&agent, &info, &token).map_err(|e| format!("{e:#}"))));
            }
        }
        let started = Instant::now();
        let resolved = Arc::new(Mutex::new(false));
        let has_cached_lines = cached.as_ref().is_some_and(|c| c.found.rank() >= 2);
        if let (false, Some(interim), false) = (prefetch, interim.clone(), has_cached_lines) {
            let (slot, token, resolved) = (slot.clone(), token.clone(), resolved.clone());
            std::thread::spawn(move || {
                if token.sleep(GRACE).is_err() {
                    return;
                }
                if let Ok(Ok(Some(early))) = slot.wait(&token) {
                    let done = resolved.lock().unwrap();
                    if !*done && !token.cancelled() {
                        log!("resolve: better sources are slow, showing lrclib for now");
                        interim(&early);
                    }
                }
            });
        }

        let mut best: Option<Found> = cached.as_ref().filter(|c| c.found.rank() >= 2).map(|c| c.found.clone());
        let mut newly_tried: Vec<String> = Vec::new();
        for id in &available {
            if *id == LRCLIB || best.as_ref().is_some_and(|b| b.rank() >= 3) {
                break;
            }
            if was_tried(&tried, id) {
                continue; // already answered for this song
            }
            token.check()?;
            let got = match *id {
                SPICY_API => match self.acc.spicy_key() {
                    Some(key) => {
                        let mut lookup = || spotify_track_id();
                        let lookup_ref: Option<&mut dyn FnMut() -> anyhow::Result<Option<String>>> =
                            if self.acc.has_spotify() { Some(&mut lookup) } else { None };
                        spicy::fetch(&self.acc.agent, &key, info, info.spotify_track_id.as_deref(), lookup_ref, token)
                    }
                    None => Ok(None),
                },
                SPOTIFY => spotify_track_id().and_then(|tid| match tid {
                    Some(tid) => spotify_api::lyrics(&self.acc, &tid, token),
                    None => Ok(None),
                }),
                _ => Ok(None),
            };
            let got = match got {
                Ok(g) => g,
                Err(e) if is_cancelled(&e) => return Err(e),
                Err(e) => {
                    // An error isn't an answer: leave it out of "tried" so the next play asks again.
                    log!("resolve: {id} failed: {e:#}");
                    continue;
                }
            };
            newly_tried.push(id.to_string());
            log!("resolve: {id} -> {}", got.as_ref().map(Found::describe).unwrap_or_else(|| "nothing".into()));
            if beats(got.as_ref(), best.as_ref()) {
                best = got;
            }
        }

        if best.as_ref().is_some_and(|b| b.rank() >= 2) {
            // LRCLIB can't beat synced lyrics from an earlier provider: count it as answered.
            if !was_tried(&tried, LRCLIB) {
                newly_tried.push(LRCLIB.into());
            }
        } else {
            match slot.wait(token)? {
                Ok(l) => {
                    newly_tried.push(LRCLIB.into()); // errors when unreachable, so this is an answer
                    log!("resolve: lrclib -> {}", l.as_ref().map(Found::describe).unwrap_or_else(|| "nothing".into()));
                    if beats(l.as_ref(), best.as_ref()) {
                        best = l;
                    }
                }
                Err(e) => log!("resolve: lrclib failed: {e}"),
            }
        }
        *resolved.lock().unwrap() = true;
        token.check()?;

        let mut all_tried = tried.clone();
        for t in &newly_tried {
            if !all_tried.contains(t) {
                all_tried.push(t.clone());
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
