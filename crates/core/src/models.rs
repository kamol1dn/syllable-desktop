//! Track and lyrics models. All times are milliseconds.

use serde::{Deserialize, Serialize};

/// What is playing, as far as we know it. Built by `syllable-media`; the lyrics
/// module may fill `spotify_track_id` / `duration_ms` in later copies.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct TrackInfo {
    pub title: String,
    pub artist: String,
    pub album: String,
    /// An https cover, when a provider knows one. Cover bytes from the media
    /// session arrive separately as `Event::Artwork`.
    pub cover_url: Option<String>,
    pub duration_ms: f64,
    /// The source app is Spotify (desktop).
    pub is_spotify: bool,
    pub spotify_track_id: Option<String>,
    /// The media session's source app id (e.g. `Spotify.exe`, a browser's AUMID).
    pub source_app: String,
}

impl TrackInfo {
    /// Identity of the song for "did the track change?". Deliberately excludes the
    /// duration and Spotify id, which trickle in after the title and artist.
    pub fn key(&self) -> String {
        format!("{}|{}|{}", self.title, self.artist, self.is_spotify)
    }
}

/// One timed text segment: a syllable, or a whole word.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Seg {
    pub start: f64,
    pub end: f64,
    /// Includes its trailing space when it ends a word.
    pub text: String,
    /// True when the next segment continues the same word.
    pub part_of_word: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct LyricLine {
    pub start: f64,
    pub end: f64,
    pub text: String,
    /// `None` when only the line is timed.
    pub sylls: Option<Vec<Seg>>,
    /// A second singer (duet): drawn on the opposite side.
    pub opposite: bool,
}

/// A background vocal ("yeah", echoes) sung near a main line.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct BgVocal {
    pub start: f64,
    pub end: f64,
    pub text: String,
    pub sylls: Option<Vec<Seg>>,
    pub opposite: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum SyncType {
    /// Word or syllable timing.
    Syllable,
    /// Line timing only.
    #[default]
    Line,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Lyrics {
    pub sync: SyncType,
    /// When the first vocal starts.
    pub start_time: f64,
    pub lines: Vec<LyricLine>,
    /// Flattened, sorted by start.
    pub bg: Vec<BgVocal>,
    /// Which provider answered: "spicy-api", "spotify", "lrclib" (from `syllable-lyrics`).
    pub source: String,
}

impl Lyrics {
    pub fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }
}
