//! The user's own Spotify app (developer.spotify.com Client ID), signed in with
//! Authorization Code + PKCE (no client secret) against a loopback redirect,
//! `http://127.0.0.1:PORT/callback`. Its token works on api.spotify.com (track ids,
//! queue), not on Spotify's lyrics endpoint, which needs the web player sign-in.

use std::io::{ErrorKind, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::Value;
use sha2::{Digest, Sha256};
use syllable_core::log;

use crate::accounts::Accounts;
use crate::cache::now_secs;
use crate::http::{self, dec, enc, Token};

pub const SCOPES: &str = "user-read-currently-playing user-read-playback-state";
const TOKEN_URL: &str = "https://accounts.spotify.com/api/token";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5 * 60);

// ---- tokens ----

fn fresh(s: &crate::secrets::Secrets) -> Option<String> {
    s.spotify_access_token.clone().filter(|_| now_secs() < s.spotify_access_expires - 60.0)
}

/// A usable access token, refreshed when it is about to expire. `None` when not connected.
pub fn token(acc: &Accounts) -> Option<String> {
    let (client_id, refresh) = {
        let s = acc.secrets();
        if let Some(t) = fresh(&s) {
            if s.spotify_refresh_token.is_some() {
                return Some(t);
            }
        }
        (s.spotify_client_id.clone()?, s.spotify_refresh_token.clone()?)
    };
    let _guard = acc.app_refresh.lock().unwrap();
    if let Some(t) = fresh(&acc.secrets()) {
        return Some(t);
    }
    let form = [("grant_type", "refresh_token"), ("refresh_token", refresh.as_str()), ("client_id", client_id.as_str())];
    match token_request(acc, &form, None) {
        Ok(()) => fresh(&acc.secrets()),
        Err(err) => {
            log!("spotify-app: refresh failed: {err}");
            if err.contains("invalid_grant") || err.contains("invalid_client") {
                // Revoked, or the app was deleted: needs a fresh connect.
                {
                    let mut s = acc.secrets();
                    s.spotify_refresh_token = None;
                    s.spotify_access_token = None;
                }
                acc.save_and_notify();
            }
            None
        }
    }
}

pub fn invalidate(acc: &Accounts) {
    acc.secrets().spotify_access_expires = 0.0;
}

/// POST to the token endpoint and store what comes back.
fn token_request(acc: &Accounts, form: &[(&str, &str)], new_client_id: Option<&str>) -> Result<(), String> {
    let res = http::post_form(&acc.agent, TOKEN_URL, form).map_err(|e| e.to_string())?;
    if !res.ok() {
        return Err(format!("HTTP {} {}", res.status, clip(&res.body, 200)));
    }
    let r: Value = serde_json::from_str(&res.body).map_err(|e| e.to_string())?;
    let access = r.get("access_token").and_then(Value::as_str).unwrap_or("");
    if access.is_empty() {
        return Err("no access_token in the response".into());
    }
    let expires = r.get("expires_in").and_then(Value::as_f64).unwrap_or(3600.0);
    {
        let mut s = acc.secrets();
        if let Some(id) = new_client_id {
            s.spotify_client_id = Some(id.to_string());
        }
        s.spotify_access_token = Some(access.to_string());
        s.spotify_access_expires = now_secs() + expires;
        // Refreshes may or may not rotate the refresh token.
        if let Some(rt) = r.get("refresh_token").and_then(Value::as_str).filter(|t| !t.is_empty()) {
            s.spotify_refresh_token = Some(rt.to_string());
        }
    }
    if new_client_id.is_some() {
        acc.save_and_notify();
    } else {
        acc.save();
    }
    Ok(())
}

/// At most `n` characters (not bytes).
fn clip(s: &str, n: usize) -> &str {
    s.char_indices().nth(n).map(|(i, _)| &s[..i]).unwrap_or(s)
}

// ---- PKCE ----

pub fn b64url(bytes: &[u8]) -> String {
    const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = chunk.iter().enumerate().fold(0u32, |acc, (i, b)| acc | (*b as u32) << (16 - 8 * i));
        for i in 0..=chunk.len() {
            out.push(A[(n >> (18 - 6 * i) & 63) as usize] as char);
        }
    }
    out
}

pub fn random_bytes(n: usize) -> Vec<u8> {
    use windows::Win32::Security::Cryptography::{BCryptGenRandom, BCRYPT_USE_SYSTEM_PREFERRED_RNG};
    let mut buf = vec![0u8; n];
    // SAFETY: fills our own buffer.
    unsafe { BCryptGenRandom(None, &mut buf, BCRYPT_USE_SYSTEM_PREFERRED_RNG) }.ok().expect("system RNG");
    buf
}

/// (verifier, S256 challenge)
pub fn pkce_pair() -> (String, String) {
    let verifier = b64url(&random_bytes(48));
    let challenge = b64url(&Sha256::digest(verifier.as_bytes()));
    (verifier, challenge)
}

pub fn authorize_url(client_id: &str, redirect: &str, challenge: &str, state: &str) -> String {
    format!(
        "https://accounts.spotify.com/authorize?client_id={}&response_type=code&redirect_uri={}\
         &code_challenge_method=S256&code_challenge={challenge}&scope={}&state={state}",
        enc(client_id),
        enc(redirect),
        enc(SCOPES)
    )
}

pub fn open_in_browser(url: &str) {
    use windows::core::{w, HSTRING};
    use windows::Win32::UI::Shell::ShellExecuteW;
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    // SAFETY: plain shell call with owned strings.
    unsafe { ShellExecuteW(None, w!("open"), &HSTRING::from(url), None, None, SW_SHOWNORMAL) };
}

/// Opens the consent page and waits (up to 5 minutes, or until `cancel`) for
/// Spotify to redirect back to the loopback listener.
pub fn connect(acc: &Arc<Accounts>, client_id: &str, cancel: &Token) -> (bool, String) {
    let (verifier, challenge) = pkce_pair();
    let state = b64url(&random_bytes(16));
    let redirect = acc.redirect_uri();

    let port = acc.redirect_port();
    let listener = match TcpListener::bind(("127.0.0.1", port)) {
        Ok(l) => l,
        Err(e) => {
            return (
                false,
                format!(
                    "Port {} is busy ({e}). Change SpotifyRedirectPort in config.json and the redirect URI in your Spotify app.",
                    port
                ),
            )
        }
    };
    let _ = listener.set_nonblocking(true);
    (acc.open_browser)(&authorize_url(client_id, &redirect, &challenge, &state));
    log!("spotify-app: waiting for the consent redirect on {redirect}");

    let result: Arc<Mutex<Option<(bool, String)>>> = Arc::new(Mutex::new(None));
    let handling = Arc::new(AtomicBool::new(false)); // only the first valid callback exchanges its code
    let flow = Arc::new(Flow { client_id: client_id.to_string(), verifier, state, redirect });
    let deadline = Instant::now() + CONNECT_TIMEOUT;

    // Each connection on its own thread: browsers open idle speculative
    // connections, and a sequential loop would sit on one while the real redirect waits.
    loop {
        if let Some(r) = result.lock().unwrap().take() {
            if r.0 {
                log!("spotify-app: connected");
            }
            return r;
        }
        if cancel.cancelled() {
            return (false, "Cancelled.".into());
        }
        if Instant::now() >= deadline {
            return (false, "Timed out waiting for Spotify (5 minutes).".into());
        }
        match listener.accept() {
            Ok((stream, _)) => {
                let (acc, result, handling, flow) = (acc.clone(), result.clone(), handling.clone(), flow.clone());
                std::thread::spawn(move || {
                    if let Err(e) = handle(&acc, stream, &flow, &handling, &result) {
                        if e.kind() != ErrorKind::WouldBlock && e.kind() != ErrorKind::TimedOut {
                            log!("spotify-app: callback handling failed: {e}");
                        }
                    }
                });
            }
            Err(e) if e.kind() == ErrorKind::WouldBlock => std::thread::sleep(Duration::from_millis(50)),
            Err(e) => {
                log!("spotify-app: accept failed: {e}");
                std::thread::sleep(Duration::from_millis(200));
            }
        }
    }
}

struct Flow {
    client_id: String,
    verifier: String,
    state: String,
    redirect: String,
}

fn handle(
    acc: &Accounts,
    mut stream: TcpStream,
    flow: &Flow,
    handling: &AtomicBool,
    result: &Mutex<Option<(bool, String)>>,
) -> std::io::Result<()> {
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    let (path, query) = read_request(&mut stream)?;
    let q = |k: &str| query.iter().find(|(key, _)| key == k).map(|(_, v)| v.as_str());

    if !path.eq_ignore_ascii_case("/callback") {
        return respond(&mut stream, 404, "Not found.");
    }
    if q("state") != Some(flow.state.as_str()) {
        return respond(&mut stream, 400, "This sign-in link is stale. Start again from Syllable.");
    }
    if let Some(error) = q("error") {
        respond(&mut stream, 200, &format!("Spotify said: {}. You can close this tab.", html_escape(error)))?;
        *result.lock().unwrap() = Some((false, format!("Spotify refused: {error}")));
        return Ok(());
    }
    let Some(code) = q("code").filter(|c| !c.is_empty()) else {
        return respond(&mut stream, 400, "No authorization code in the redirect.");
    };
    if handling.swap(true, Ordering::SeqCst) {
        return respond(&mut stream, 200, "Already handled. You can close this tab.");
    }

    let form = [
        ("grant_type", "authorization_code"),
        ("code", code),
        ("redirect_uri", flow.redirect.as_str()),
        ("client_id", flow.client_id.as_str()),
        ("code_verifier", flow.verifier.as_str()),
    ];
    let outcome = token_request(acc, &form, Some(&flow.client_id));
    let page = match &outcome {
        Ok(()) => "Syllable is connected to Spotify. You can close this tab.".to_string(),
        Err(e) => format!("Connecting failed: {}", html_escape(e)),
    };
    let r = respond(&mut stream, 200, &page);
    *result.lock().unwrap() = Some(match outcome {
        Ok(()) => (true, "Connected.".into()),
        Err(e) => (false, e),
    });
    r
}

/// `GET /callback?code=...&state=... HTTP/1.1` -> ("/callback", [(k, v)]).
fn read_request(stream: &mut TcpStream) -> std::io::Result<(String, Vec<(String, String)>)> {
    let mut data = Vec::new();
    let mut buf = [0u8; 8192];
    while !data.windows(4).any(|w| w == b"\r\n\r\n") && data.len() < 16_384 {
        let n = stream.read(&mut buf)?;
        if n == 0 {
            break;
        }
        data.extend_from_slice(&buf[..n]);
    }
    Ok(parse_request_line(&String::from_utf8_lossy(&data)))
}

pub fn parse_request_line(head: &str) -> (String, Vec<(String, String)>) {
    let first = head.split("\r\n").next().unwrap_or("");
    let target = first.split(' ').nth(1).unwrap_or("/");
    let (path, qs) = target.split_once('?').unwrap_or((target, ""));
    let query = qs
        .split('&')
        .filter(|p| !p.is_empty())
        .map(|pair| {
            let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
            (dec(k), dec(v))
        })
        .collect();
    (path.to_string(), query)
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

fn respond(stream: &mut TcpStream, status: u16, message: &str) -> std::io::Result<()> {
    let html = format!(
        "<!doctype html><meta charset=utf-8><title>Syllable</title>\
         <body style=\"font:16px Segoe UI,sans-serif;margin:3em\"><p>{message}</p></body>"
    );
    let head = format!(
        "HTTP/1.1 {status} {}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        if status == 200 { "OK" } else { "Error" },
        html.len()
    );
    stream.write_all(head.as_bytes())?;
    stream.write_all(html.as_bytes())?;
    stream.flush()
}

#[cfg(test)]
mod tests {
    //! Ported from SpotifyAppAuthTests: runs the loopback listener for real.
    use super::*;
    use std::sync::mpsc;

    #[test]
    fn b64url_matches_rfc4648() {
        assert_eq!(b64url(b""), "");
        assert_eq!(b64url(b"f"), "Zg");
        assert_eq!(b64url(b"fo"), "Zm8");
        assert_eq!(b64url(b"foo"), "Zm9v");
        assert_eq!(b64url(b"foobar"), "Zm9vYmFy");
        assert_eq!(b64url(&[0xfb, 0xff]), "-_8");
    }

    #[test]
    fn pkce_challenge_is_s256_of_the_verifier() {
        // RFC 7636 appendix B.
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        assert_eq!(b64url(&Sha256::digest(verifier.as_bytes())), "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM");
        let (v, c) = pkce_pair();
        assert_eq!(v.len(), 64);
        assert!(v.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'));
        assert_eq!(c, b64url(&Sha256::digest(v.as_bytes())));
        assert_ne!(pkce_pair().0, v);
    }

    #[test]
    fn request_line_parsing() {
        let (p, q) = parse_request_line("GET /callback?code=a%2Bb&state=x+y&flag HTTP/1.1\r\nHost: x\r\n\r\n");
        assert_eq!(p, "/callback");
        assert_eq!(q, [("code".into(), "a+b".into()), ("state".into(), "x y".into()), ("flag".into(), String::new())]);
    }

    fn free_port() -> u16 {
        TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
    }

    fn get(port: u16, path: &str) -> (u16, String) {
        let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
        write!(s, "GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n").unwrap();
        let mut out = String::new();
        s.read_to_string(&mut out).unwrap();
        let status = out.split(' ').nth(1).unwrap().parse().unwrap();
        (status, out)
    }

    #[test]
    fn callback_survives_idle_connections_and_stale_states() {
        let dir = std::env::temp_dir().join(format!("syllable-lyrics-pkce-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let port = free_port();
        let (tx, rx) = mpsc::channel::<String>();
        let tx = Mutex::new(tx);
        let mut acc = Accounts::new(dir.clone(), port, http::agent(), None);
        Arc::get_mut(&mut acc).unwrap().open_browser = Box::new(move |url| {
            let _ = tx.lock().unwrap().send(url.to_string());
        });

        let worker = {
            let acc = acc.clone();
            std::thread::spawn(move || acc.connect_app("0123456789abcdef0123456789abcdef", &Token::none()))
        };
        let url = rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(url.contains(&format!("redirect_uri={}", enc(&format!("http://127.0.0.1:{port}/callback")))));
        assert!(url.contains("code_challenge_method=S256"));
        assert!(url.contains("scope=user-read-currently-playing%20user-read-playback-state"));
        let state = url.split("state=").nth(1).unwrap().split('&').next().unwrap().to_string();

        // A browser's speculative connection that never sends anything.
        let _idle = TcpStream::connect(("127.0.0.1", port)).unwrap();

        assert_eq!(get(port, "/favicon.ico").0, 404);
        assert_eq!(get(port, "/callback?state=stale&code=x").0, 400);

        // The real redirect with a made-up code: the listener must take it and hand
        // the code to Spotify's token endpoint, which refuses it.
        let (status, page) = get(port, &format!("/callback?state={state}&code=not-a-real-code"));
        assert_eq!(status, 200);
        let (ok, msg) = worker.join().unwrap();
        assert!(!ok);
        assert!(page.contains("Connecting failed"), "{page}");
        assert!(!acc.spotify_app_connected());
        assert!(!msg.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn connect_can_be_cancelled() {
        let dir = std::env::temp_dir().join(format!("syllable-lyrics-pkce-cancel-{}", std::process::id()));
        let mut acc = Accounts::new(dir.clone(), free_port(), http::agent(), None);
        Arc::get_mut(&mut acc).unwrap().open_browser = Box::new(|_| {});
        let token = acc.app_connect.next();
        let a = acc.clone();
        let worker = std::thread::spawn(move || a.connect_app("abc", &token));
        std::thread::sleep(Duration::from_millis(200));
        acc.app_connect.cancel();
        assert_eq!(worker.join().unwrap(), (false, "Cancelled.".to_string()));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
