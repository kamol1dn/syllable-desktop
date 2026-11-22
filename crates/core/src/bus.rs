//! The event bus: how modules talk without depending on each other.
//!
//! Every subscriber gets every event, in order, on its own channel. Publishing never
//! blocks. High-rate data does not go through here: the playback position lives in
//! [`crate::position`] and audio levels in [`crate::audio`], both read at render time.

use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};

use crate::accounts::AccountsCommand;
use crate::models::{Lyrics, TrackInfo};

#[derive(Clone, Debug)]
pub enum Event {
    /// A different song is playing, or nothing is (`None`). Published by `syllable-media`.
    TrackChanged(Option<Arc<TrackInfo>>),
    /// More is now known about the current song (duration, Spotify id). Same `key()`.
    TrackUpdated(Arc<TrackInfo>),
    /// Cover image bytes (PNG/JPEG) for the current song, or `None` when it has none.
    Artwork(Option<Arc<Vec<u8>>>),
    /// Lyrics for the current song, or `None` when there are none. Published by
    /// `syllable-lyrics`; may be published again if a better source answers later.
    Lyrics(Option<Arc<Lyrics>>),
    /// Playing/paused flipped. The position itself is in `position`.
    PlaybackChanged { playing: bool },
    /// The config was changed and saved: re-read what you use from `ConfigStore`.
    ConfigChanged,
    /// The user asked for the settings window (tray click).
    OpenSettings,
    /// A request for the lyrics module's accounts (Spicy key, Spotify sign-in), from
    /// settings or the tray. See `accounts`.
    AccountsCommand(AccountsCommand),
    /// `accounts::current()` changed (something was set up, removed, started or finished).
    AccountsChanged,
    /// The user asked for a different wallpaper image now (settings "Next wallpaper").
    /// Published by `syllable-settings`; handled by `syllable-wallpaper`.
    NextWallpaper,
    /// The app is exiting: stop threads and destroy windows.
    Quit,
}

#[derive(Clone, Default)]
pub struct Bus {
    subscribers: Arc<Mutex<Vec<Sender<Event>>>>,
}

impl Bus {
    pub fn new() -> Self {
        Self::default()
    }

    /// A new receiver that sees every event published from now on.
    pub fn subscribe(&self) -> Receiver<Event> {
        let (tx, rx) = channel();
        self.subscribers.lock().unwrap().push(tx);
        rx
    }

    /// Deliver to every live subscriber; subscribers whose receiver was dropped are removed.
    pub fn publish(&self, event: Event) {
        self.subscribers
            .lock()
            .unwrap()
            .retain(|tx| tx.send(event.clone()).is_ok());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_subscriber_gets_events_and_dropped_ones_are_removed() {
        let bus = Bus::new();
        let a = bus.subscribe();
        let b = bus.subscribe();
        bus.publish(Event::ConfigChanged);
        assert!(matches!(a.try_recv(), Ok(Event::ConfigChanged)));
        assert!(matches!(b.try_recv(), Ok(Event::ConfigChanged)));
        drop(b);
        bus.publish(Event::Quit);
        assert_eq!(bus.subscribers.lock().unwrap().len(), 1);
        assert!(matches!(a.try_recv(), Ok(Event::Quit)));
    }
}
