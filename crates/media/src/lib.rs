//! Now playing: watches the Windows media session (SMTC) and publishes the track,
//! artwork and playback state on the bus, and keeps `syllable_core::position` in
//! time with the player.
//!
//! One thread ("media") owns the session manager. It polls when a session raises a
//! change event, and every 500 ms as a safety net (some players miss events).
//! - `TrackChanged` when the song identity (`TrackInfo::key()`) changes, debounced
//!   because browsers rewrite their metadata while a page loads.
//! - `TrackUpdated` when the same song's duration or album arrives later.
//! - `Artwork` after each `TrackChanged`, and again if the thumbnail changes within a
//!   few seconds (players publish the new title before the new cover).
//! - `PlaybackChanged` when playing/paused flips.
//! The position rules (stale timelines, pause, seek, slewing) are in `clock`.

mod clock;
mod session;

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use syllable_core::{log, position, Bus, Context, Event, TrackInfo};

use clock::{Clock, Sample};
use session::{Msg, Snapshot};

const POLL: Duration = Duration::from_millis(500);
/// After an SMTC change event, wait this long (gathering more events) before reading.
const EVENT_SETTLE: Duration = Duration::from_millis(100);
/// A new identity must hold this long before it counts (browsers flicker while loading).
const DEBOUNCE: Duration = Duration::from_millis(700);
/// Spotify does not flicker; a short hold still lets title and artist land together.
const DEBOUNCE_SPOTIFY: Duration = Duration::from_millis(200);
/// After a track change, keep re-reading the thumbnail this long in case it lags.
const ART_RECHECK: Duration = Duration::from_secs(6);
/// ...and this long when the song had no thumbnail at first.
const ART_LATE: Duration = Duration::from_secs(30);
const ART_MIN_INTERVAL: Duration = Duration::from_millis(400);
/// Duration changes smaller than this are not news (live streams move `EndTime`).
const DURATION_NEWS_MS: f64 = 1000.0;

/// Start the module. Returns once it is running: the watcher runs on its own
/// thread and stops when it sees `Event::Quit` on the bus.
pub fn start(ctx: Context) -> anyhow::Result<()> {
    let (tx, rx) = channel();

    // The bus receiver cannot be waited on together with our own channel, so a small
    // thread drains it (it must be drained, or it grows) and forwards `Quit`.
    let events = ctx.bus.subscribe();
    let quit_tx = tx.clone();
    std::thread::Builder::new().name("media-bus".into()).spawn(move || {
        for ev in events {
            if matches!(ev, Event::Quit) {
                let _ = quit_tx.send(Msg::Quit);
                break;
            }
        }
    })?;

    let bus = ctx.bus.clone();
    std::thread::Builder::new().name("media".into()).spawn(move || {
        if let Err(e) = run(bus, tx, rx) {
            log!("media: stopped: {e}");
        }
    })?;
    Ok(())
}

fn run(bus: Bus, tx: Sender<Msg>, rx: Receiver<Msg>) -> windows::core::Result<()> {
    let mgr = session::manager()?;
    let mut subs = session::Subscriptions::new(&mgr, tx)?;
    let mut w = Watcher::new(bus);
    log!("media: watching media sessions");

    loop {
        if let Err(e) = w.poll(&mgr) {
            log!("media: poll error: {e}");
        }
        let wait = w.next_wait();
        let first = match rx.recv_timeout(wait) {
            Ok(m) => m,
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => break,
        };
        // Woken by a change event: do not call back into SMTC while the work item that
        // raised it is still running. That work item swaps the session's cached
        // PlaybackInfo, and reading it at that moment (GetPlaybackInfo on every session,
        // right after PlaybackInfoChanged) is the suspected cause of the 2026-11-21 crash:
        // a PlaybackInfo freed twice inside Windows.Media.MediaControl.dll. So settle,
        // coalescing the burst, then poll once.
        let settle_until = Instant::now() + EVENT_SETTLE;
        let mut pending = Some(first);
        loop {
            while let Some(m) = pending.take().or_else(|| rx.try_recv().ok()) {
                match m {
                    Msg::Quit => {
                        drop(subs);
                        return Ok(());
                    }
                    Msg::Resubscribe => subs.resubscribe(),
                    Msg::Wake => {}
                }
            }
            let left = settle_until.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            match rx.recv_timeout(left) {
                Ok(m) => pending = Some(m),
                Err(RecvTimeoutError::Timeout) => break,
                Err(RecvTimeoutError::Disconnected) => return Ok(()),
            }
        }
    }
    Ok(())
}

struct ArtState {
    key: Option<String>,
    hash: Option<u64>,
    since: Instant,
    last_read: Option<Instant>,
}

struct Watcher {
    bus: Bus,
    clock: Clock,
    mono0: Instant,
    /// The track last published with `TrackChanged`; `None` = nothing playing.
    emitted: Option<Arc<TrackInfo>>,
    /// A different identity seen but not yet held long enough.
    pending: Option<(Option<String>, Instant)>,
    /// Identity + source of the previous poll, to restart the clock on a change.
    observed: Option<(String, String)>,
    /// Last known playing state (kept through "changing").
    playing: bool,
    published_playing: Option<bool>,
    art: ArtState,
    /// Details of the previous poll and since when they have held still. Spotify
    /// switches album and duration a moment before the title on a track change, so
    /// details are only news once they have settled.
    details: (String, String, i64, String),
    details_since: Instant,
}

impl Watcher {
    fn new(bus: Bus) -> Self {
        Self {
            bus,
            clock: Clock::new(),
            mono0: Instant::now(),
            emitted: None,
            pending: None,
            observed: None,
            playing: false,
            published_playing: None,
            art: ArtState { key: None, hash: None, since: Instant::now(), last_read: None },
            details: Default::default(),
            details_since: Instant::now(),
        }
    }

    fn mono_ms(&self) -> f64 {
        self.mono0.elapsed().as_secs_f64() * 1000.0
    }

    /// How long the loop may sleep before something is due.
    fn next_wait(&self) -> Duration {
        let mut wait = POLL;
        if let Some((key, since)) = &self.pending {
            let hold = if key.as_deref().is_some_and(|k| k.ends_with("|true")) { DEBOUNCE_SPOTIFY } else { DEBOUNCE };
            wait = wait.min(hold.saturating_sub(since.elapsed()));
        }
        if let Some(due) = self.clock.deadline() {
            wait = wait.min(Duration::from_secs_f64(((due - self.mono_ms()) / 1000.0).max(0.0)));
        }
        wait.max(Duration::from_millis(5))
    }

    fn poll(&mut self, mgr: &windows::Media::Control::GlobalSystemMediaTransportControlsSessionManager) -> windows::core::Result<()> {
        let follow = self.observed.as_ref().map(|(_, app)| app.as_str());
        let snap = match session::pick(mgr, follow) {
            Some(s) => Some(session::read(&s)?),
            None => None,
        };
        match snap.filter(|s| !s.title.trim().is_empty()) {
            Some(snap) => self.observe(snap),
            None => self.observe_nothing(),
        }
        Ok(())
    }

    fn observe(&mut self, snap: Snapshot) {
        let is_spotify = snap.source_app.to_lowercase().contains("spotify");
        let info = TrackInfo {
            title: snap.title.clone(),
            artist: snap.artist.clone(),
            album: snap.album.clone(),
            duration_ms: snap.duration_ms,
            is_spotify,
            source_app: snap.source_app.clone(),
            ..Default::default()
        };
        let key = info.key();
        let details = (key.clone(), info.album.clone(), (info.duration_ms / 1000.0).round() as i64, info.source_app.clone());
        if details != self.details {
            self.details = details;
            self.details_since = Instant::now();
        }

        // Position first: the clock must restart on the very poll the song changes.
        let id = (key.clone(), snap.source_app.clone());
        if self.observed.as_ref().is_some_and(|o| *o != id) {
            self.clock.track_changed();
        }
        self.observed = Some(id);
        if let Some(p) = snap.playing {
            self.playing = p;
        }
        let sample = Sample { playing: self.playing, rate: snap.rate, timeline: snap.timeline };
        if let Some(w) = self.clock.update(&sample, self.mono_ms(), wall_unix_ms()) {
            position::set(w.position_ms, w.playing, w.rate);
        }
        if self.published_playing != Some(self.playing) {
            self.published_playing = Some(self.playing);
            self.bus.publish(Event::PlaybackChanged { playing: self.playing });
        }

        let changed = self.emit_if_changed(Some(info.clone()), is_spotify);
        let Some(current) = self.emitted.clone() else { return };
        if current.key() != key {
            return; // a change is pending; nothing below applies to it yet
        }
        if !changed && self.details_since.elapsed() >= DEBOUNCE {
            let longer = (info.duration_ms - current.duration_ms).abs() > DURATION_NEWS_MS;
            if longer || info.album != current.album || info.source_app != current.source_app {
                let updated = Arc::new(TrackInfo { title: current.title.clone(), artist: current.artist.clone(), ..info });
                self.emitted = Some(updated.clone());
                self.bus.publish(Event::TrackUpdated(updated));
            }
        }
        self.artwork(&key, snap.thumbnail.as_ref());
    }

    fn observe_nothing(&mut self) {
        if !self.emit_if_changed(None, false) {
            return;
        }
        self.clock.clear();
        position::clear();
        self.observed = None;
        self.art = ArtState { key: None, hash: None, since: Instant::now(), last_read: None };
        self.bus.publish(Event::Artwork(None));
        if self.published_playing == Some(true) {
            self.bus.publish(Event::PlaybackChanged { playing: false });
        }
        self.playing = false;
        self.published_playing = None;
    }

    /// Publish `TrackChanged` once a new identity has held for the debounce time.
    /// Returns true when it did.
    fn emit_if_changed(&mut self, info: Option<TrackInfo>, is_spotify: bool) -> bool {
        let key = info.as_ref().map(TrackInfo::key);
        if key == self.emitted.as_ref().map(|t| t.key()) {
            self.pending = None;
            return false;
        }
        let hold = if is_spotify { DEBOUNCE_SPOTIFY } else { DEBOUNCE };
        match &self.pending {
            Some((k, since)) if *k == key => {
                if since.elapsed() < hold {
                    return false;
                }
            }
            _ => {
                self.pending = Some((key, Instant::now()));
                return false;
            }
        }
        self.pending = None;
        let info = info.map(Arc::new);
        match &info {
            Some(t) => log!(
                "media: track -> {} - {}{} ({}, {:.0} s)",
                t.artist,
                t.title,
                if t.is_spotify { " [spotify]" } else { "" },
                t.source_app,
                t.duration_ms / 1000.0
            ),
            None => log!("media: track -> (none)"),
        }
        self.emitted = info.clone();
        self.bus.publish(Event::TrackChanged(info));
        true
    }

    fn artwork(&mut self, key: &str, thumb: Option<&windows::Storage::Streams::IRandomAccessStreamReference>) {
        let new_track = self.art.key.as_deref() != Some(key);
        if new_track {
            self.art = ArtState { key: Some(key.to_string()), hash: None, since: Instant::now(), last_read: None };
        }
        let age = self.art.since.elapsed();
        let recheck = age < ART_RECHECK || (self.art.hash.is_none() && age < ART_LATE);
        let due = self.art.last_read.is_none_or(|t| t.elapsed() >= ART_MIN_INTERVAL);
        if !new_track && !(recheck && due) {
            return;
        }
        self.art.last_read = Some(Instant::now());

        let bytes = match thumb.map(session::read_thumbnail) {
            Some(Ok(b)) if !b.is_empty() => Some(b),
            Some(Err(e)) => {
                log!("media: artwork read failed: {e}");
                None
            }
            _ => None,
        };
        let hash = bytes.as_ref().map(|b| {
            let mut h = DefaultHasher::new();
            b.hash(&mut h);
            h.finish()
        });
        if new_track || (hash.is_some() && hash != self.art.hash) {
            match &bytes {
                Some(b) => log!("media: artwork {} bytes{}", b.len(), if new_track { "" } else { " (updated)" }),
                None => log!("media: artwork none"),
            }
            self.art.hash = hash;
            self.bus.publish(Event::Artwork(bytes.map(Arc::new)));
        }
    }
}

fn wall_unix_ms() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs_f64() * 1000.0).unwrap_or(0.0)
}
