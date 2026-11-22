//! The optional lyric sources the user can set up, as a contract between
//! `syllable-lyrics` (which owns them) and `syllable-settings` (which shows them).
//!
//! - Settings asks for things by publishing `Event::AccountsCommand(..)`.
//! - Lyrics keeps the current state in this slot ([`current`]) and publishes
//!   `Event::AccountsChanged` whenever it changes, including when a command
//!   finishes (its outcome is in [`AccountsState::last_result`]).
//!
//! Read [`current`] when a window opens (the bus has no history) and again on every
//! `AccountsChanged`. Keys and tokens never leave the lyrics module: the state only
//! says whether they are set.

use std::fmt;
use std::sync::{Arc, RwLock};

/// A request from the UI. Long-running ones (key test, sign-in, app connect) set
/// [`AccountsState::busy`] while they run and end with a [`AccountsState::last_result`].
#[derive(Clone)]
pub enum AccountsCommand {
    /// Store the Spicy Lyrics developer API key (`None` or blank removes it).
    SetSpicyKey(Option<String>),
    /// Check a key with one cheap request, without storing it.
    TestSpicyKey(String),
    /// Open the Spotify web-player sign-in window (Spotify's own lyrics, exact track ids, queue).
    SpotifySignIn,
    /// Forget the web-player token and clear its sign-in cookies.
    SpotifySignOut,
    /// Store the user's own Spotify app Client ID and run the PKCE consent flow in the
    /// default browser (exact track ids and queue only). Register
    /// [`AccountsState::spotify_redirect_uri`] in the Spotify app first.
    ConnectSpotifyApp { client_id: String },
    /// Stop waiting for the consent redirect started by `ConnectSpotifyApp`.
    CancelSpotifyApp,
    /// Forget the client id and its tokens.
    DisconnectSpotifyApp,
}

/// Keys are redacted so the bus can be logged safely.
impl fmt::Debug for AccountsCommand {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SetSpicyKey(k) => write!(f, "SetSpicyKey({})", if k.is_some() { "<redacted>" } else { "None" }),
            Self::TestSpicyKey(_) => write!(f, "TestSpicyKey(<redacted>)"),
            Self::SpotifySignIn => write!(f, "SpotifySignIn"),
            Self::SpotifySignOut => write!(f, "SpotifySignOut"),
            Self::ConnectSpotifyApp { client_id } => write!(f, "ConnectSpotifyApp {{ client_id: {client_id:?} }}"),
            Self::CancelSpotifyApp => write!(f, "CancelSpotifyApp"),
            Self::DisconnectSpotifyApp => write!(f, "DisconnectSpotifyApp"),
        }
    }
}

/// Which long-running command a `busy` flag or a result belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AccountsAction {
    TestSpicyKey,
    SpotifySignIn,
    SpotifySignOut,
    ConnectSpotifyApp,
}

#[derive(Clone, Debug, PartialEq)]
pub struct AccountsResult {
    pub action: AccountsAction,
    pub ok: bool,
    /// One sentence for the user ("Key works.", "Timed out waiting for Spotify (5 minutes).").
    pub message: String,
    /// Increases with every result, so a UI can tell a new result from one it has shown.
    pub seq: u64,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct AccountsState {
    /// False until the lyrics module has loaded its secrets.
    pub ready: bool,
    pub has_spicy_key: bool,
    /// Signed in to the Spotify web player (gives Spotify's own lyrics).
    pub spotify_signed_in: bool,
    pub spotify_user_name: Option<String>,
    /// The user's own Spotify app's Client ID (not a secret).
    pub spotify_client_id: Option<String>,
    /// The user's own Spotify app is authorised (refresh token held).
    pub spotify_app_connected: bool,
    /// What to register as the redirect URI in the user's Spotify app,
    /// e.g. `http://127.0.0.1:9013/callback`.
    pub spotify_redirect_uri: String,
    /// Commands still running.
    pub busy: Vec<AccountsAction>,
    /// The outcome of the last finished long-running command.
    pub last_result: Option<AccountsResult>,
}

impl AccountsState {
    /// Either Spotify connection: enough for exact track ids and the queue.
    pub fn has_spotify(&self) -> bool {
        self.spotify_signed_in || self.spotify_app_connected
    }
}

static STATE: RwLock<Option<Arc<AccountsState>>> = RwLock::new(None);

/// The latest state (a not-`ready` default before the lyrics module starts).
pub fn current() -> Arc<AccountsState> {
    STATE.read().unwrap().clone().unwrap_or_default()
}

/// Written by `syllable-lyrics` only; it publishes `Event::AccountsChanged` after.
pub fn set(state: AccountsState) {
    *STATE.write().unwrap() = Some(Arc::new(state));
}
