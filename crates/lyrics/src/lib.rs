//! Lyrics: resolves the playing track to synced lyrics (Spicy Lyrics API key,
//! Spotify, Musixmatch, NetEase, LRCLIB) with a disk cache, and owns the optional accounts.
//!
//! In: `TrackChanged`, `TrackUpdated`, `AccountsCommand`, `Quit`.
//! Out: `Lyrics` (every `TrackChanged` ends in exactly one terminal publish, maybe
//! preceded by interim ones), `AccountsChanged` (see `syllable_core::accounts`).

mod accounts;
pub mod cache;
mod cover;
mod http;
mod lrclib;
mod musixmatch;
mod netease;
pub mod parse;
mod resolver;
mod secrets;
mod spicy;
mod spotify_api;
mod spotify_app;
mod spotify_web;

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use syllable_core::config::data_dir;
use syllable_core::{log, Bus, Context, Event, TrackInfo};

use crate::accounts::Accounts;
use crate::cache::Cache;
use crate::http::Generation;
use crate::resolver::Resolver;

/// Start the module. Returns once it is running.
pub fn start(ctx: Context) -> anyhow::Result<()> {
    start_in(ctx, data_dir())
}

/// Same, with the cache and secrets in `dir` instead of the app's data folder
/// (examples and tests).
pub fn start_in(ctx: Context, dir: PathBuf) -> anyhow::Result<()> {
    let events = ctx.bus.subscribe();
    let port = ctx.config.read().spotify_redirect_port;
    let acc = Accounts::new(dir.clone(), port, http::agent(), Some(ctx.bus.clone()));
    let service = Arc::new(Service {
        resolver: Arc::new(Resolver { acc, cache: Cache::new(dir.join("cache")) }),
        bus: ctx.bus.clone(),
        gen: Generation::default(),
        prefetch_gen: Generation::default(),
        publish: Arc::new(Mutex::new(())),
        config: ctx.config.clone(),
        cover_ids: cover::Ids::load(dir.join("cache").join("cover-ids.json")),
    });
    std::thread::Builder::new().name("lyrics".into()).spawn(move || service.run(events))?;
    Ok(())
}

struct Service {
    resolver: Arc<Resolver>,
    bus: Bus,
    gen: Generation,
    prefetch_gen: Generation,
    /// Held while the generation moves on and while a worker checks it and publishes,
    /// so a cancelled lookup can never publish after the track changed.
    publish: Arc<Mutex<()>>,
    config: syllable_core::ConfigStore,
    /// Spotify ids of songs whose cover was looked up, kept across runs.
    cover_ids: cover::Ids,
}

/// What the control thread knows about the lookup in flight.
struct Current {
    info: Arc<TrackInfo>,
    done: Arc<AtomicBool>,
    found: Arc<AtomicBool>,
}

/// What is set up; a change re-resolves the current song.
fn setup(acc: &Accounts) -> (bool, bool, bool) {
    (acc.spicy_key().is_some(), acc.spotify_signed_in(), acc.spotify_app_connected())
}

impl Service {
    /// Spotify's 640 px cover for a Spotify song, in place of the media session's small
    /// thumbnail (see `cover`). Cancelled with the lookup when the song changes.
    fn spawn_cover(self: &Arc<Self>, info: &Arc<TrackInfo>, token: &http::Token) {
        if !info.is_spotify {
            return;
        }
        let (me, info, token) = (self.clone(), info.clone(), token.clone());
        let _ = std::thread::Builder::new().name("lyrics-cover".into()).spawn(move || {
            match cover::fetch(&me.resolver.acc, &me.cover_ids, &info, &token) {
                Ok(Some(bytes)) => {
                    let _g = me.publish.lock().unwrap();
                    if !token.cancelled() {
                        me.bus.publish(Event::HiResArtwork { key: info.key(), bytes: Arc::new(bytes) });
                    }
                }
                Ok(None) => {}
                Err(e) if http::is_cancelled(&e) => {}
                Err(e) => log!("cover: {e:#}"),
            }
        });
    }

    fn run(self: Arc<Self>, events: std::sync::mpsc::Receiver<Event>) {
        log!("lyrics: started (cache in {})", self.resolver.cache_dir_display());
        let mut current: Option<Current> = None;
        let mut last_setup = setup(&self.resolver.acc);
        for event in events {
            match event {
                Event::TrackChanged(None) => {
                    let _g = self.publish.lock().unwrap();
                    self.gen.cancel();
                    self.prefetch_gen.cancel();
                    current = None;
                    self.bus.publish(Event::Lyrics(None));
                }
                Event::TrackChanged(Some(info)) => current = Some(self.spawn(info)),
                Event::TrackUpdated(info) => {
                    let Some(cur) = &mut current else { continue };
                    if cur.info.key() != info.key() {
                        continue;
                    }
                    let adds_id = info.spotify_track_id.is_some() && cur.info.spotify_track_id != info.spotify_track_id;
                    let adds_duration = cur.info.duration_ms <= 0.0 && info.duration_ms > 0.0;
                    let settled = cur.done.load(Ordering::SeqCst) && cur.found.load(Ordering::SeqCst);
                    if (adds_id || adds_duration) && !settled {
                        log!("resolve: track updated ({}), asking again", if adds_id { "spotify id" } else { "duration" });
                        current = Some(self.spawn(info));
                    } else {
                        cur.info = info;
                    }
                }
                Event::AccountsCommand(cmd) => self.resolver.acc.handle(cmd),
                Event::ConfigChanged => {
                    let port = self.config.read().spotify_redirect_port;
                    self.resolver.acc.set_redirect_port(port);
                }
                Event::AccountsChanged => {
                    let now = setup(&self.resolver.acc);
                    if now != last_setup {
                        last_setup = now;
                        if let Some(cur) = &current {
                            log!("resolve: lyric sources changed, asking again");
                            current = Some(self.spawn(cur.info.clone()));
                        }
                    }
                }
                Event::Quit => {
                    self.gen.cancel();
                    self.prefetch_gen.cancel();
                    self.resolver.acc.app_connect.cancel();
                    break;
                }
                _ => {}
            }
        }
    }

    fn spawn(self: &Arc<Self>, info: Arc<TrackInfo>) -> Current {
        let token = {
            let _g = self.publish.lock().unwrap();
            self.gen.next()
        };
        let cur = Current { info: info.clone(), done: Arc::new(AtomicBool::new(false)), found: Arc::new(AtomicBool::new(false)) };
        let (done, found) = (cur.done.clone(), cur.found.clone());
        self.spawn_cover(&info, &token);
        let me = self.clone();
        std::thread::Builder::new()
            .name("lyrics-resolve".into())
            .spawn(move || {
                let publish = {
                    let (bus, lock, token) = (me.bus.clone(), me.publish.clone(), token.clone());
                    move |l: Option<&parse::Found>| {
                        let _g = lock.lock().unwrap();
                        if !token.cancelled() {
                            bus.publish(Event::Lyrics(l.map(|f| Arc::new(f.lyrics.clone()))));
                            true
                        } else {
                            false
                        }
                    }
                };
                let interim: resolver::Interim = {
                    let publish = publish.clone();
                    Arc::new(move |f: &parse::Found| {
                        publish(Some(f));
                    })
                };
                match me.resolver.resolve(&info, &token, Some(interim), false) {
                    Ok(l) => {
                        found.store(l.is_some(), Ordering::SeqCst);
                        if publish(l.as_ref()) {
                            me.start_prefetch(&info);
                        }
                    }
                    Err(e) if http::is_cancelled(&e) => {}
                    Err(e) => {
                        log!("resolve: error: {e:#}");
                        publish(None);
                    }
                }
                done.store(true, Ordering::SeqCst);
            })
            .expect("spawn lyrics worker");
        cur
    }

    fn start_prefetch(self: &Arc<Self>, current: &Arc<TrackInfo>) {
        if !current.is_spotify || !self.resolver.acc.has_spotify() {
            return;
        }
        let token = self.prefetch_gen.next().with_deadline(std::time::Duration::from_secs(120));
        let (resolver, current) = (self.resolver.clone(), current.clone());
        std::thread::spawn(move || resolver.prefetch(&current, &token));
    }
}

impl Resolver {
    fn cache_dir_display(&self) -> String {
        self.acc.data_dir.join("cache").display().to_string()
    }
}
