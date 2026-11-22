//! The optional lyric sources and their secrets. Owns `secrets.dat`, answers
//! `Event::AccountsCommand` and keeps `syllable_core::accounts::current()` up to date.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU16, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use syllable_core::accounts::{self, AccountsAction, AccountsCommand, AccountsResult, AccountsState};
use syllable_core::{log, Bus, Event};
use ureq::Agent;

use crate::http::{Generation, Token};
use crate::secrets::{self, Secrets};
use crate::{spicy, spotify_api, spotify_app, spotify_web};

pub struct Accounts {
    secrets: Mutex<Secrets>,
    secrets_path: PathBuf,
    pub data_dir: PathBuf,
    redirect_port: AtomicU16,
    pub agent: Agent,
    bus: Option<Bus>,
    busy: Mutex<Vec<AccountsAction>>,
    last_result: Mutex<Option<AccountsResult>>,
    seq: AtomicU64,
    /// Serialises token refreshes (app) and WebView2 runs (web).
    pub app_refresh: Mutex<()>,
    pub web_lock: Mutex<()>,
    /// Title|artist -> Spotify id, so re-resolves and repeat plays don't re-query.
    pub id_memo: Mutex<HashMap<String, String>>,
    /// Cancels a pending app consent wait.
    pub app_connect: Generation,
    /// Replaceable for tests: opens the consent page.
    pub open_browser: Box<dyn Fn(&str) + Send + Sync>,
}

impl Accounts {
    pub fn new(data_dir: PathBuf, redirect_port: u16, agent: Agent, bus: Option<Bus>) -> Arc<Self> {
        let secrets_path = secrets::path_in(&data_dir);
        let acc = Arc::new(Self {
            secrets: Mutex::new(Secrets::load(&secrets_path)),
            secrets_path,
            data_dir,
            redirect_port: AtomicU16::new(redirect_port),
            agent,
            bus,
            busy: Mutex::new(Vec::new()),
            last_result: Mutex::new(None),
            seq: AtomicU64::new(0),
            app_refresh: Mutex::new(()),
            web_lock: Mutex::new(()),
            id_memo: Mutex::new(HashMap::new()),
            app_connect: Generation::default(),
            open_browser: Box::new(spotify_app::open_in_browser),
        });
        acc.publish_state();
        acc
    }

    pub fn secrets(&self) -> MutexGuard<'_, Secrets> {
        self.secrets.lock().unwrap()
    }

    // ---- state ----

    pub fn spicy_key(&self) -> Option<String> {
        self.secrets().spicy_api_key.clone().filter(|k| !k.trim().is_empty())
    }

    pub fn spotify_signed_in(&self) -> bool {
        self.secrets().spotify_web_signed_in
    }

    pub fn spotify_app_connected(&self) -> bool {
        let s = self.secrets();
        s.spotify_refresh_token.is_some() && s.spotify_client_id.is_some()
    }

    /// Either Spotify connection: enough for exact track ids and the queue.
    pub fn has_spotify(&self) -> bool {
        self.spotify_signed_in() || self.spotify_app_connected()
    }

    pub fn redirect_uri(&self) -> String {
        format!("http://127.0.0.1:{}/callback", self.redirect_port())
    }

    pub fn redirect_port(&self) -> u16 {
        self.redirect_port.load(Ordering::SeqCst)
    }

    /// Follows `SpotifyRedirectPort` in the config; announces the new redirect URI.
    pub fn set_redirect_port(&self, port: u16) {
        if self.redirect_port.swap(port, Ordering::SeqCst) != port {
            log!("accounts: Spotify redirect port is now {port}");
            self.publish_state();
        }
    }

    pub fn state(&self) -> AccountsState {
        let s = self.secrets().clone();
        AccountsState {
            ready: true,
            has_spicy_key: s.spicy_api_key.as_deref().is_some_and(|k| !k.trim().is_empty()),
            spotify_signed_in: s.spotify_web_signed_in,
            spotify_user_name: s.spotify_user_name.clone(),
            spotify_client_id: s.spotify_client_id.clone(),
            spotify_app_connected: s.spotify_refresh_token.is_some() && s.spotify_client_id.is_some(),
            spotify_redirect_uri: self.redirect_uri(),
            busy: self.busy.lock().unwrap().clone(),
            last_result: self.last_result.lock().unwrap().clone(),
        }
    }

    /// Only the state the bus-facing instance owns goes to the shared slot.
    fn publish_state(&self) {
        if let Some(bus) = &self.bus {
            accounts::set(self.state());
            bus.publish(Event::AccountsChanged);
        }
    }

    /// Persist only (token refreshes): not a change of what is set up.
    pub fn save(&self) {
        let s = self.secrets().clone();
        s.save(&self.secrets_path);
    }

    /// Persist and announce (something was set up or removed).
    pub fn save_and_notify(&self) {
        self.save();
        self.publish_state();
    }

    fn finish(&self, action: AccountsAction, ok: bool, message: String) {
        self.busy.lock().unwrap().retain(|a| *a != action);
        let seq = self.seq.fetch_add(1, Ordering::SeqCst) + 1;
        *self.last_result.lock().unwrap() = Some(AccountsResult { action, ok, message, seq });
        self.publish_state();
    }

    /// Marks `action` busy; false when it already is.
    fn start(&self, action: AccountsAction) -> bool {
        {
            let mut busy = self.busy.lock().unwrap();
            if busy.contains(&action) {
                return false;
            }
            busy.push(action);
        }
        self.publish_state();
        true
    }

    // ---- commands ----

    /// Runs a command. Long ones run on their own thread; returns at once.
    pub fn handle(self: &Arc<Self>, cmd: AccountsCommand) {
        log!("accounts: {cmd:?}");
        match cmd {
            AccountsCommand::SetSpicyKey(key) => {
                self.secrets().spicy_api_key = key.map(|k| k.trim().to_string()).filter(|k| !k.is_empty());
                log!("accounts: Spicy Lyrics API key {}", if self.spicy_key().is_some() { "set" } else { "removed" });
                self.save_and_notify();
            }
            AccountsCommand::TestSpicyKey(key) => self.background(AccountsAction::TestSpicyKey, move |acc| {
                if key.trim().is_empty() {
                    return (false, "Paste the key first.".into());
                }
                spicy::test_key(&acc.agent, &key)
            }),
            AccountsCommand::SpotifySignIn => self.background(AccountsAction::SpotifySignIn, |acc| acc.sign_in()),
            AccountsCommand::SpotifySignOut => self.background(AccountsAction::SpotifySignOut, |acc| {
                acc.sign_out();
                (true, "Signed out of Spotify.".into())
            }),
            AccountsCommand::ConnectSpotifyApp { client_id } => {
                let token = self.app_connect.next();
                self.background(AccountsAction::ConnectSpotifyApp, move |acc| acc.connect_app(&client_id, &token))
            }
            AccountsCommand::CancelSpotifyApp => self.app_connect.cancel(),
            AccountsCommand::DisconnectSpotifyApp => {
                {
                    let mut s = self.secrets();
                    s.spotify_client_id = None;
                    s.spotify_access_token = None;
                    s.spotify_refresh_token = None;
                    s.spotify_access_expires = 0.0;
                }
                log!("accounts: Spotify client ID removed");
                self.save_and_notify();
            }
        }
    }

    fn background(self: &Arc<Self>, action: AccountsAction, work: impl FnOnce(&Arc<Self>) -> (bool, String) + Send + 'static) {
        if !self.start(action) {
            log!("accounts: {action:?} is already running");
            return;
        }
        let acc = self.clone();
        std::thread::Builder::new()
            .name(format!("accounts-{action:?}"))
            .spawn(move || {
                let (ok, message) = work(&acc);
                log!("accounts: {action:?} -> {}", if ok { "ok" } else { &message });
                acc.finish(action, ok, message);
            })
            .expect("spawn accounts thread");
    }

    /// Opens the Spotify web-player sign-in window and waits for it.
    pub fn sign_in(self: &Arc<Self>) -> (bool, String) {
        match spotify_web::sign_in(self) {
            Ok(true) => {
                match spotify_api::user_name(self, &Token::none()) {
                    Ok(name) => self.secrets().spotify_user_name = name,
                    Err(e) => log!("accounts: reading the Spotify user name failed: {e}"),
                }
                self.save_and_notify();
                (true, "Signed in to Spotify.".into())
            }
            Ok(false) => (false, "Sign-in was not completed.".into()),
            Err(e) => (false, format!("Signing in needs the Microsoft Edge WebView2 Runtime ({e}).")),
        }
    }

    pub fn sign_out(self: &Arc<Self>) {
        {
            let mut s = self.secrets();
            s.spotify_web_signed_in = false;
            s.spotify_web_token = None;
            s.spotify_web_token_time = 0.0;
            s.spotify_user_name = None;
        }
        self.save_and_notify();
        if let Err(e) = spotify_web::clear_cookies(self) {
            log!("spotify-web: clearing cookies failed: {e}");
        }
        log!("accounts: signed out of Spotify");
    }

    pub fn connect_app(self: &Arc<Self>, client_id: &str, token: &Token) -> (bool, String) {
        if client_id.trim().is_empty() {
            return (false, "Paste the Client ID first.".into());
        }
        spotify_app::connect(self, client_id.trim(), token)
    }
}
