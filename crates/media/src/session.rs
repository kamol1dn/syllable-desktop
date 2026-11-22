//! WinRT glue: the Windows media session manager (SMTC, the API behind the
//! volume-flyout media card). Reads one session into plain data and keeps change
//! handlers attached. Handlers run on WinRT pool threads and only send a wake-up.

use std::sync::mpsc::Sender;

use windows::core::HSTRING;
use windows::Foundation::TypedEventHandler;
use windows::Media::Control::{
    GlobalSystemMediaTransportControlsSession as Session,
    GlobalSystemMediaTransportControlsSessionManager as Manager,
    GlobalSystemMediaTransportControlsSessionPlaybackStatus as Status,
};
use windows::Storage::Streams::{DataReader, IRandomAccessStreamReference};

use crate::clock::Timeline;

pub enum Msg {
    /// Something about a session changed: poll now.
    Wake,
    /// The set of sessions changed: re-attach handlers, then poll.
    Resubscribe,
    Quit,
}

/// One session, read into plain data.
pub struct Snapshot {
    pub source_app: String,
    pub title: String,
    pub artist: String,
    pub album: String,
    /// `None` while the player says "changing" (between tracks) or gives no status.
    pub playing: Option<bool>,
    pub rate: f64,
    /// `None` when the player reports no timeline at all.
    pub timeline: Option<Timeline>,
    pub duration_ms: f64,
    pub thumbnail: Option<IRandomAccessStreamReference>,
}

pub fn manager() -> windows::core::Result<Manager> {
    Manager::RequestAsync()?.join()
}

fn is_playing(s: &Session) -> bool {
    s.GetPlaybackInfo().and_then(|p| p.PlaybackStatus()).map(|st| st == Status::Playing).unwrap_or(false)
}

fn is_spotify(s: &Session) -> bool {
    s.SourceAppUserModelId().map(|id| id.to_string().to_lowercase().contains("spotify")).unwrap_or(false)
}

/// Windows' "current" session is not reliably the one the user cares about: a
/// paused browser tab regularly takes it while Spotify keeps playing, and keeps it
/// after Spotify is paused. So: what is actually playing (Spotify first); else the
/// session we were already following (`follow`, its app id), so pausing does not
/// switch songs; else "current".
pub fn pick(mgr: &Manager, follow: Option<&str>) -> Option<Session> {
    // Debug aid: `SYLLABLE_MEDIA_FOLLOW=<part of an app id>` always follows that session
    // (e.g. `fake_sessions` from the example of that name). Unset: the rules below.
    static FORCE: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    if let Some(want) = FORCE.get_or_init(|| std::env::var("SYLLABLE_MEDIA_FOLLOW").ok().filter(|s| !s.is_empty())) {
        if let Some(s) = mgr.GetSessions().ok().and_then(|all| {
            all.into_iter().find(|s| s.SourceAppUserModelId().is_ok_and(|id| id.to_string().contains(want.as_str())))
        }) {
            return Some(s);
        }
    }
    let current = mgr.GetCurrentSession().ok();
    if current.as_ref().is_some_and(is_playing) {
        return current;
    }
    let Ok(sessions) = mgr.GetSessions() else { return current };
    let mut playing = None;
    let mut followed = None;
    for s in sessions {
        if is_playing(&s) {
            if is_spotify(&s) {
                return Some(s);
            }
            playing.get_or_insert_with(|| s.clone());
        }
        if followed.is_none() && follow.is_some_and(|f| s.SourceAppUserModelId().is_ok_and(|id| id == f)) {
            followed = Some(s);
        }
    }
    playing.or(followed).or(current)
}

/// WinRT `DateTime`/`TimeSpan` ticks are 100 ns; `DateTime` counts from 1601.
const TICKS_PER_MS: f64 = 10_000.0;
const UNIX_EPOCH_TICKS: i64 = 116_444_736_000_000_000;
/// 2000-01-01 in unix ms: anything earlier is "not set".
const Y2000_UNIX_MS: f64 = 946_684_800_000.0;

pub fn read(s: &Session) -> windows::core::Result<Snapshot> {
    let props = s.TryGetMediaPropertiesAsync()?.join()?;
    let text = |r: windows::core::Result<HSTRING>| r.map(|h| h.to_string()).unwrap_or_default();

    let (playing, rate) = match s.GetPlaybackInfo() {
        Ok(info) => {
            let playing = match info.PlaybackStatus() {
                Ok(Status::Playing) => Some(true),
                Ok(Status::Paused | Status::Stopped | Status::Closed | Status::Opened) => Some(false),
                _ => None, // Changing
            };
            let rate = info.PlaybackRate().and_then(|r| r.Value()).unwrap_or(1.0);
            (playing, rate)
        }
        Err(_) => (None, 1.0),
    };

    let (timeline, duration_ms) = match s.GetTimelineProperties() {
        Ok(tl) => {
            let ms = |d: windows::core::Result<windows::Foundation::TimeSpan>| {
                d.map(|t| t.Duration as f64 / TICKS_PER_MS).unwrap_or(0.0)
            };
            let (pos, start, end) = (ms(tl.Position()), ms(tl.StartTime()), ms(tl.EndTime()));
            let updated = tl
                .LastUpdatedTime()
                .ok()
                .map(|d| (d.UniversalTime - UNIX_EPOCH_TICKS) as f64 / TICKS_PER_MS)
                .filter(|&u| u > Y2000_UNIX_MS);
            let none = pos == 0.0 && end == 0.0 && updated.is_none();
            let timeline = (!none).then_some(Timeline { position_ms: pos - start, updated_unix_ms: updated });
            (timeline, (end - start).max(0.0))
        }
        Err(_) => (None, 0.0),
    };

    Ok(Snapshot {
        source_app: text(s.SourceAppUserModelId()),
        title: text(props.Title()),
        artist: text(props.Artist()),
        album: text(props.AlbumTitle()),
        playing,
        rate,
        timeline,
        duration_ms,
        thumbnail: props.Thumbnail().ok(),
    })
}

/// The thumbnail's bytes (PNG/JPEG as the player provided them). Empty when it has none.
pub fn read_thumbnail(r: &IRandomAccessStreamReference) -> windows::core::Result<Vec<u8>> {
    let stream = r.OpenReadAsync()?.join()?;
    let size = stream.Size()? as u32;
    if size == 0 {
        return Ok(Vec::new());
    }
    let reader = DataReader::CreateDataReader(&stream.GetInputStreamAt(0)?)?;
    let loaded = reader.LoadAsync(size)?.join()?;
    let mut bytes = vec![0u8; loaded as usize];
    reader.ReadBytes(&mut bytes)?;
    Ok(bytes)
}

/// Change handlers on the manager and on every session; detached on drop.
pub struct Subscriptions {
    mgr: Manager,
    mgr_tokens: [i64; 2],
    sessions: Vec<(Session, [i64; 3])>,
    tx: Sender<Msg>,
}

impl Subscriptions {
    pub fn new(mgr: &Manager, tx: Sender<Msg>) -> windows::core::Result<Self> {
        let t1 = tx.clone();
        let current = mgr.CurrentSessionChanged(&TypedEventHandler::new(move |_, _| {
            let _ = t1.send(Msg::Wake);
            Ok(())
        }))?;
        let t2 = tx.clone();
        let list = mgr.SessionsChanged(&TypedEventHandler::new(move |_, _| {
            let _ = t2.send(Msg::Resubscribe);
            Ok(())
        }))?;
        let mut subs = Self { mgr: mgr.clone(), mgr_tokens: [current, list], sessions: Vec::new(), tx };
        subs.resubscribe();
        Ok(subs)
    }

    /// Drop every session handler and attach to the sessions that exist now.
    pub fn resubscribe(&mut self) {
        self.detach_sessions();
        let Ok(sessions) = self.mgr.GetSessions() else { return };
        for s in sessions {
            // A macro, not a closure-returning closure: each event has its own
            // handler type, so each needs its own closure.
            macro_rules! wake {
                () => {{
                    let tx = self.tx.clone();
                    TypedEventHandler::new(move |_, _| {
                        let _ = tx.send(Msg::Wake);
                        Ok(())
                    })
                }};
            }
            // All three or none: if a later registration fails (the session is closing),
            // remove the ones already made instead of leaving them registered.
            let Ok(props) = s.MediaPropertiesChanged(&wake!()) else { continue };
            let Ok(playback) = s.PlaybackInfoChanged(&wake!()) else {
                let _ = s.RemoveMediaPropertiesChanged(props);
                continue;
            };
            let Ok(timeline) = s.TimelinePropertiesChanged(&wake!()) else {
                let _ = s.RemoveMediaPropertiesChanged(props);
                let _ = s.RemovePlaybackInfoChanged(playback);
                continue;
            };
            self.sessions.push((s, [props, playback, timeline]));
        }
    }

    fn detach_sessions(&mut self) {
        for (s, [a, b, c]) in self.sessions.drain(..) {
            let _ = s.RemoveMediaPropertiesChanged(a);
            let _ = s.RemovePlaybackInfoChanged(b);
            let _ = s.RemoveTimelinePropertiesChanged(c);
        }
    }
}

impl Drop for Subscriptions {
    fn drop(&mut self) {
        self.detach_sessions();
        let _ = self.mgr.RemoveCurrentSessionChanged(self.mgr_tokens[0]);
        let _ = self.mgr.RemoveSessionsChanged(self.mgr_tokens[1]);
    }
}
