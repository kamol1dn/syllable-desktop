//! The local feed: a WebSocket server on `127.0.0.1:Config.port`, path `/wallpaper`.
//!
//! Every page (one per monitor, or a browser tab) gets a snapshot when it connects
//! (hello, settings, track, lyrics, art, position), then live updates from the bus.
//! Position goes out every 250 ms and at once when play/pause flips; the page
//! interpolates in between. The bass level goes out ~30x/s to pages that asked for it;
//! audio capture is demanded (`audio::demand("wallpaper", ..)`) only while some page wants it.
//!
//! Blocking `tungstenite`, one thread per connection: the thread owns its socket and
//! alternates between draining its outgoing queue and a short-timeout read.

use std::io::ErrorKind;
use std::net::{Ipv4Addr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::Value;
use syllable_core::{audio, log, position, surfaces, Context, Event};
use tungstenite::handshake::server::{ErrorResponse, Request, Response};
use tungstenite::http::StatusCode;
use tungstenite::{Message, WebSocket};

use crate::{http, images, protocol};

const POS_EVERY: Duration = Duration::from_millis(250);
const AUDIO_EVERY: Duration = Duration::from_millis(33);
const READ_SLICE: Duration = Duration::from_millis(10);
const SILENT_AFTER: Duration = Duration::from_millis(300);

type Text = Arc<str>;
/// Open pages: an id and the queue its connection thread writes from.
type Viewers = Vec<(u64, Sender<Text>)>;

/// What a page needs to be brought up to date, kept pre-serialised.
struct Snapshot {
    settings: Text,
    track: Option<Text>,
    lyrics: Text,
    art: Option<Text>,
}

struct Shared {
    ctx: Context,
    quit: AtomicBool,
    next_id: AtomicU64,
    /// Snapshot and viewer list under one lock: a page registered while holding it
    /// sees every broadcast that follows its snapshot, and none twice.
    state: Mutex<(Snapshot, Viewers)>,
    /// Pages that asked for the bass level; audio capture runs only while this is non-empty.
    audio_pages: Mutex<Vec<u64>>,
}

fn text(v: &Value) -> Text {
    Arc::from(v.to_string())
}

impl Shared {
    /// Change the snapshot and send the same messages to every page.
    fn publish(&self, change: impl FnOnce(&mut Snapshot) -> Vec<Text>) {
        let mut guard = self.state.lock().unwrap();
        let (snap, viewers) = &mut *guard;
        let msgs = change(snap);
        viewers.retain(|(_, tx)| msgs.iter().all(|m| tx.send(m.clone()).is_ok()));
    }

    /// A page switched its bass feed on or off: tell the audio module whether anyone still wants it.
    fn want_audio(&self, id: u64, on: bool) {
        let mut pages = self.audio_pages.lock().unwrap();
        let before = !pages.is_empty();
        pages.retain(|p| *p != id);
        if on {
            pages.push(id);
        }
        let now = !pages.is_empty();
        if now != before {
            audio::demand("wallpaper", now);
            log!("wallpaper feed: bass level {}", if now { "wanted" } else { "no longer wanted" });
        }
    }

    fn viewer_count(&self) -> usize {
        self.state.lock().unwrap().1.len()
    }
}

pub fn start(ctx: Context) {
    let settings = text(&protocol::settings(&ctx.config.read()));
    let shared = Arc::new(Shared {
        ctx: ctx.clone(),
        quit: AtomicBool::new(false),
        next_id: AtomicU64::new(1),
        state: Mutex::new((
            Snapshot { settings, track: None, lyrics: text(&protocol::lyrics(None)), art: None },
            Vec::new(),
        )),
        audio_pages: Mutex::new(Vec::new()),
    });

    let events = ctx.bus.subscribe();
    let s = shared.clone();
    thread::Builder::new().name("wallpaper-feed-events".into()).spawn(move || bus_loop(s, events)).ok();
    let s = shared;
    thread::Builder::new().name("wallpaper-feed-accept".into()).spawn(move || accept_loop(s)).ok();
}

/// The playback clock as the page wants it (sync offset added).
fn position_msg(ctx: &Context) -> Value {
    let offset = ctx.config.read().global_offset_ms;
    protocol::position(position::now_ms(), position::playing(), position::rate(), offset)
}

fn bus_loop(shared: Arc<Shared>, events: Receiver<Event>) {
    while let Ok(event) = events.recv() {
        match event {
            Event::TrackChanged(info) => shared.publish(|s| {
                let track = text(&protocol::track(info.as_deref()));
                let lyrics = text(&if info.is_some() { protocol::lyrics_loading() } else { protocol::lyrics(None) });
                s.track = Some(track.clone());
                s.lyrics = lyrics.clone();
                s.art = None; // the new song's cover follows as Event::Artwork
                // The new song's position right behind it (the media clock restarted
                // before this event), not on the next 250 ms tick: until then the page
                // would still run the previous song's clock under the new song.
                vec![track, lyrics, text(&position_msg(&shared.ctx))]
            }),
            Event::TrackUpdated(info) => shared.publish(|s| {
                let track = text(&protocol::track(Some(&info)));
                s.track = Some(track.clone());
                vec![track]
            }),
            Event::Artwork(bytes) => shared.publish(|s| {
                s.art = bytes.and_then(|b| protocol::art(&b)).map(|v| text(&v));
                s.art.iter().cloned().collect()
            }),
            Event::Lyrics(lyrics) => shared.publish(|s| {
                s.lyrics = text(&protocol::lyrics(lyrics.as_deref()));
                vec![s.lyrics.clone()]
            }),
            Event::ConfigChanged => {
                let settings = text(&protocol::settings(&shared.ctx.config.read()));
                shared.publish(|s| {
                    if s.settings == settings {
                        return vec![];
                    }
                    s.settings = settings.clone();
                    vec![settings]
                });
            }
            // Settings "Next wallpaper": every page in image mode moves to its next image.
            Event::NextWallpaper => shared.publish(|_| vec![text(&protocol::next_wallpaper())]),
            Event::Quit => break,
            // Play/pause: every connection watches position::playing() itself.
            _ => {}
        }
    }
    shared.quit.store(true, Ordering::SeqCst);
}

fn accept_loop(shared: Arc<Shared>) {
    let mut bound: Option<(u16, TcpListener)> = None;
    let mut last_error = String::new();
    let mut next_try = Instant::now();
    while !shared.quit.load(Ordering::SeqCst) {
        let port = shared.ctx.config.read().port;
        if bound.as_ref().map(|(p, _)| *p) != Some(port) && Instant::now() >= next_try {
            bound = None;
            match TcpListener::bind((Ipv4Addr::LOCALHOST, port)).and_then(|l| l.set_nonblocking(true).map(|_| l)) {
                Ok(l) => {
                    log!("wallpaper feed: listening on 127.0.0.1:{port}");
                    last_error.clear();
                    bound = Some((port, l));
                }
                Err(e) => {
                    // Usually another app (or a second copy) on the same port: keep trying.
                    let msg = format!("wallpaper feed: cannot listen on 127.0.0.1:{port}: {e}; retrying");
                    if msg != last_error {
                        log!("{msg}");
                        last_error = msg;
                    }
                    next_try = Instant::now() + Duration::from_secs(5);
                }
            }
        }
        match bound.as_ref().map(|(_, l)| l.accept()) {
            Some(Ok((stream, _))) => {
                let s = shared.clone();
                thread::Builder::new().name("wallpaper-feed-conn".into()).spawn(move || serve(s, stream)).ok();
            }
            Some(Err(e)) if e.kind() == ErrorKind::WouldBlock => thread::sleep(Duration::from_millis(50)),
            Some(Err(e)) => {
                log!("wallpaper feed: accept failed: {e}");
                thread::sleep(Duration::from_millis(200));
            }
            None => thread::sleep(Duration::from_millis(200)),
        }
    }
}

/// Pages from the local disk (the desktop host: file://, origin "null") or from a
/// loopback http page may connect; ordinary web pages may not.
pub(crate) fn origin_allowed(origin: Option<&str>) -> bool {
    let Some(o) = origin.map(str::trim) else { return true };
    if o.is_empty() || o == "null" || o.to_ascii_lowercase().starts_with("file:") {
        return true;
    }
    let lower = o.to_ascii_lowercase();
    let Some(rest) = lower.strip_prefix("http://").or_else(|| lower.strip_prefix("https://")) else { return false };
    let host = rest.split('/').next().unwrap_or("");
    let host = if host.starts_with('[') { host.split(']').next().map(|h| &h[1..]).unwrap_or("") } else { host.split(':').next().unwrap_or("") };
    host == "localhost" || host == "::1" || host.starts_with("127.")
}

fn path_allowed(path: &str) -> bool {
    path.trim_end_matches('/').eq_ignore_ascii_case("/wallpaper")
}

fn handshake(stream: TcpStream) -> Option<(WebSocket<TcpStream>, String, String)> {
    let mut screen = String::new();
    let mut surface = String::new();
    let check = |req: &Request, resp: Response| -> Result<Response, ErrorResponse> {
        let origin = req.headers().get("Origin").and_then(|v| v.to_str().ok());
        let refuse = |code: StatusCode| {
            let mut r = ErrorResponse::new(None);
            *r.status_mut() = code;
            Err(r)
        };
        if !path_allowed(req.uri().path()) {
            log!("wallpaper feed: refused path {}", req.uri().path());
            return refuse(StatusCode::NOT_FOUND);
        }
        if !origin_allowed(origin) {
            log!("wallpaper feed: refused origin {}", origin.unwrap_or("-"));
            return refuse(StatusCode::FORBIDDEN);
        }
        let param = |name: &str| {
            req.uri().query().and_then(|q| q.split('&').find_map(|kv| kv.strip_prefix(name).map(str::to_string)))
        };
        screen = param("screen=").unwrap_or_else(|| "-".into());
        surface = param("surface=").unwrap_or_default();
        Ok(resp)
    };
    let ws = tungstenite::accept_hdr(stream, check).ok()?;
    Some((ws, screen, surface))
}

fn serve(shared: Arc<Shared>, stream: TcpStream) {
    // The handshake itself runs blocking, with a deadline so a silent client can't hold a thread.
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));
    let _ = stream.set_nodelay(true);
    // Plain HTTP requests are for the page files (Aura loads the lock page by URL);
    // WebSocket upgrades are pages joining the feed.
    let Some(head) = peek_head(&stream) else { return };
    if !head.upgrade_websocket {
        let root = crate::page_path().and_then(|p| p.parent().map(|d| d.to_path_buf()));
        http::serve(stream, &head, root.as_deref());
        return;
    }
    let Some((mut ws, screen, surface)) = handshake(stream) else { return };
    let lock_page = surface.eq_ignore_ascii_case("lockscreen");
    let _ = ws.get_ref().set_read_timeout(Some(READ_SLICE));

    let id = shared.next_id.fetch_add(1, Ordering::Relaxed);
    let (tx, rx) = channel::<Text>();
    {
        let mut guard = shared.state.lock().unwrap();
        let (snap, viewers) = &mut *guard;
        let _ = tx.send(text(&protocol::hello()));
        let _ = tx.send(snap.settings.clone()); // first, so the page lays out right from the start
        if let Some(t) = &snap.track {
            let _ = tx.send(t.clone());
        }
        let _ = tx.send(snap.lyrics.clone());
        if let Some(a) = &snap.art {
            let _ = tx.send(a.clone());
        }
        viewers.push((id, tx.clone()));
    }
    log!("wallpaper feed: page connected (screen {screen}, {} open)", shared.viewer_count());
    if lock_page {
        surfaces::lock_page_opened();
        shared.ctx.bus.publish(Event::SurfacesChanged);
    }

    let why = run(&shared, id, &mut ws, &rx, &tx);
    shared.want_audio(id, false);
    shared.state.lock().unwrap().1.retain(|(v, _)| *v != id);
    log!("wallpaper feed: page disconnected (screen {screen}: {why}, {} open)", shared.viewer_count());
    if lock_page {
        surfaces::lock_page_closed();
        shared.ctx.bus.publish(Event::SurfacesChanged);
    }
}

/// Look at the request head without consuming it, so a WebSocket handshake can still
/// read it from the start.
fn peek_head(stream: &TcpStream) -> Option<http::Head> {
    let mut buf = [0u8; 8192];
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let n = stream.peek(&mut buf).ok()?;
        if n == 0 {
            return None;
        }
        if buf[..n].windows(4).any(|w| w == b"\r\n\r\n") {
            return http::parse_head(&buf[..n]);
        }
        if n == buf.len() || Instant::now() >= deadline {
            return None;
        }
        thread::sleep(Duration::from_millis(5));
    }
}

fn run(shared: &Shared, id: u64, ws: &mut WebSocket<TcpStream>, rx: &Receiver<Text>, tx: &Sender<Text>) -> String {
    let mut audio_on = false;
    let mut last_pos: Option<Instant> = None;
    let mut last_audio = Instant::now();
    let mut last_frame: Option<Arc<audio::AudioFrame>> = None;
    let mut last_frame_at = Instant::now();
    let mut sent_silence = true;
    let mut last_playing = position::playing();
    loop {
        if shared.quit.load(Ordering::SeqCst) {
            let _ = ws.close(None);
            let _ = ws.flush();
            return "app exiting".into();
        }

        // Outgoing: queued snapshot/broadcasts, then the timed messages.
        loop {
            match rx.try_recv() {
                Ok(m) => {
                    if let Err(e) = ws.write(Message::text(m.to_string())) {
                        return format!("write: {e}");
                    }
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => return "dropped".into(),
            }
        }
        let playing = position::playing();
        if last_pos.is_none_or(|t| t.elapsed() >= POS_EVERY) || playing != last_playing {
            last_pos = Some(Instant::now());
            last_playing = playing;
            let m = position_msg(&shared.ctx);
            if let Err(e) = ws.write(Message::text(m.to_string())) {
                return format!("write: {e}");
            }
        }
        if audio_on && last_audio.elapsed() >= AUDIO_EVERY {
            last_audio = Instant::now();
            // The audio module stops replacing the frame once it goes silent: send only new
            // frames, and one 0 after a short silence so the page doesn't hold a loud value.
            let frame = audio::latest();
            let fresh = match (&frame, &last_frame) {
                (Some(f), Some(l)) => !Arc::ptr_eq(f, l),
                (Some(_), None) => true,
                _ => false,
            };
            let bass = if fresh {
                last_frame_at = Instant::now();
                frame.as_ref().map(|f| f.bass)
            } else if !sent_silence && last_frame_at.elapsed() >= SILENT_AFTER {
                sent_silence = true;
                Some(0.0)
            } else {
                None
            };
            if fresh {
                sent_silence = false;
            }
            last_frame = frame;
            if let Some(bass) = bass {
                if let Err(e) = ws.write(Message::text(protocol::audio(bass).to_string())) {
                    return format!("write: {e}");
                }
            }
        }
        if let Err(e) = ws.flush() {
            if !is_timeout(&e) {
                return format!("flush: {e}");
            }
        }

        // Incoming, waiting at most READ_SLICE.
        match ws.read() {
            Ok(Message::Text(t)) => match protocol::parse_request(t.as_str()) {
                Some(protocol::Request::ListWallpapers { dir }) => {
                    let files = images::list(&dir);
                    log!("wallpaper feed: listed {} images in {dir}", files.len());
                    let _ = tx.send(text(&protocol::wallpapers(&dir, &files)));
                }
                Some(protocol::Request::Audio { on }) => {
                    audio_on = on;
                    shared.want_audio(id, on);
                }
                None => {}
            },
            Ok(Message::Close(_)) => return "closed by page".into(),
            Ok(_) => {}
            Err(e) if is_timeout(&e) => {}
            Err(e) => return format!("read: {e}"),
        }
    }
}

fn is_timeout(e: &tungstenite::Error) -> bool {
    matches!(e, tungstenite::Error::Io(io) if matches!(io.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The real path a connection takes: peek the head, see it isn't a WebSocket
    /// upgrade, and answer it as a page request.
    fn http_get(path: &str, host: &str) -> String {
        use std::io::{Read, Write};
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let addr = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let head = peek_head(&stream).expect("head");
            assert!(!head.upgrade_websocket);
            let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..\\..\\assets\\wallpaper");
            http::serve(stream, &head, Some(&root));
        });
        let mut client = TcpStream::connect(addr).unwrap();
        let host = host.replace("{port}", &addr.port().to_string());
        client.write_all(format!("GET {path} HTTP/1.1\r\nHost: {host}\r\n\r\n").as_bytes()).unwrap();
        let mut response = String::new();
        client.read_to_string(&mut response).unwrap();
        server.join().unwrap();
        response
    }

    #[test]
    fn serves_the_lock_page_over_http() {
        let r = http_get("/lockscreen", "127.0.0.1:{port}");
        assert!(r.starts_with("HTTP/1.1 200 OK"), "{}", &r[..r.len().min(200)]);
        assert!(r.contains("Content-Type: text/html"));
        assert!(r.contains("Cache-Control: no-store"));
        assert!(r.contains("window.WALLPAPER_SURFACE = \"lockscreen\""));
        assert!(http_get("/js/app.js", "localhost:{port}").contains("Content-Type: text/javascript"));
    }

    #[test]
    fn refuses_other_hosts_and_paths() {
        assert!(http_get("/lockscreen", "evil.example:{port}").starts_with("HTTP/1.1 403"));
        assert!(http_get("/../Cargo.toml", "127.0.0.1:{port}").starts_with("HTTP/1.1 404"));
        assert!(http_get("/%2e%2e/%2e%2e/Cargo.toml", "127.0.0.1:{port}").starts_with("HTTP/1.1 404"));
    }

    #[test]
    fn origins() {
        for ok in [None, Some(""), Some("null"), Some("file://"), Some("http://localhost:5173"), Some("http://127.0.0.1"), Some("http://[::1]:80")] {
            assert!(origin_allowed(ok), "{ok:?}");
        }
        for bad in [Some("https://example.com"), Some("http://localhost.evil.com"), Some("https://open.spotify.com"), Some("chrome-extension://x")] {
            assert!(!origin_allowed(bad), "{bad:?}");
        }
    }

    #[test]
    fn only_the_wallpaper_path() {
        assert!(path_allowed("/wallpaper"));
        assert!(path_allowed("/Wallpaper/"));
        assert!(!path_allowed("/"));
        assert!(!path_allowed("/wallpaperx"));
    }
}
