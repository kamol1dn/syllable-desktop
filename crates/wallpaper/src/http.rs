//! Plain HTTP for the wallpaper pages, on the feed's loopback port, so a host that loads
//! pages by URL (Aura Wallpaper's lock screen) gets one fixed address:
//!
//!   http://127.0.0.1:<port>/lockscreen   lockscreen.html (the lock-screen layout)
//!   http://127.0.0.1:<port>/             index.html (the desktop layout)
//!   http://127.0.0.1:<port>/<file>       any other file under assets\wallpaper
//!
//! Read-only: GET and HEAD only, no directory listings, nothing outside the folder, and
//! only `Host: 127.0.0.1` / `localhost` (so a web page can't reach it through DNS
//! rebinding). Every response is `Cache-Control: no-store`, so the host always loads the
//! current page; settings changes then reach it live over the feed's WebSocket.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Component, Path, PathBuf};

use syllable_core::log;

/// The request line and headers, if the client sent a plain HTTP request.
pub(crate) struct Head {
    pub method: String,
    pub path: String,
    pub host: Option<String>,
    pub upgrade_websocket: bool,
    /// Bytes up to and including the blank line that ends the head.
    pub len: usize,
}

pub(crate) fn parse_head(bytes: &[u8]) -> Option<Head> {
    let end = bytes.windows(4).position(|w| w == b"\r\n\r\n")?;
    let text = std::str::from_utf8(&bytes[..end]).ok()?;
    let mut lines = text.split("\r\n");
    let mut first = lines.next()?.split(' ');
    let method = first.next()?.to_string();
    let path = first.next()?.to_string();
    let mut host = None;
    let mut upgrade_websocket = false;
    for line in lines {
        let Some((k, v)) = line.split_once(':') else { continue };
        let (k, v) = (k.trim().to_ascii_lowercase(), v.trim());
        if k == "host" {
            host = Some(v.to_string());
        } else if k == "upgrade" && v.eq_ignore_ascii_case("websocket") {
            upgrade_websocket = true;
        }
    }
    Some(Head { method, path, host, upgrade_websocket, len: end + 4 })
}

fn host_allowed(host: Option<&str>) -> bool {
    let Some(h) = host else { return false };
    let h = h.to_ascii_lowercase();
    let name = if h.starts_with('[') { h.split(']').next().map(|s| &s[1..]).unwrap_or("") } else { h.split(':').next().unwrap_or("") };
    name == "127.0.0.1" || name == "localhost" || name == "::1"
}

fn decode(path: &str) -> Option<String> {
    let bytes = path.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = std::str::from_utf8(bytes.get(i + 1..i + 3)?).ok()?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// The file a URL path maps to, or `None` if it isn't one we serve.
pub(crate) fn resolve(root: &Path, url_path: &str) -> Option<PathBuf> {
    let path = url_path.split(['?', '#']).next().unwrap_or("");
    let path = decode(path)?;
    let rel = match path.trim_end_matches('/') {
        "" => "index.html".to_string(),
        "/lockscreen" | "/lock" => "lockscreen.html".to_string(),
        p => p.trim_start_matches('/').to_string(),
    };
    if rel.contains('\\') || rel.contains(':') {
        return None;
    }
    let rel = Path::new(&rel);
    if !rel.components().all(|c| matches!(c, Component::Normal(_))) {
        return None;
    }
    let file = root.join(rel);
    file.is_file().then_some(file)
}

fn content_type(path: &Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()).map(|e| e.to_ascii_lowercase()).as_deref() {
        Some("html") => "text/html; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("js") => "text/javascript; charset=utf-8",
        Some("json") => "application/json",
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("webp") => "image/webp",
        Some("svg") => "image/svg+xml",
        Some("woff2") => "font/woff2",
        Some("ico") => "image/x-icon",
        _ => "application/octet-stream",
    }
}

fn respond(stream: &mut TcpStream, status: &str, ctype: &str, body: &[u8], head_only: bool) {
    let header = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nCache-Control: no-store\r\n\
         X-Content-Type-Options: nosniff\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let _ = stream.write_all(header.as_bytes());
    if !head_only {
        let _ = stream.write_all(body);
    }
    let _ = stream.flush();
}

/// Answer one plain HTTP request (the head has only been peeked at so far).
pub(crate) fn serve(mut stream: TcpStream, head: &Head, root: Option<&Path>) {
    let mut consumed = vec![0u8; head.len];
    if stream.read_exact(&mut consumed).is_err() {
        return;
    }
    if !host_allowed(head.host.as_deref()) {
        respond(&mut stream, "403 Forbidden", "text/plain", b"forbidden", false);
        return;
    }
    let head_only = head.method == "HEAD";
    if head.method != "GET" && !head_only {
        respond(&mut stream, "405 Method Not Allowed", "text/plain", b"method not allowed", false);
        return;
    }
    let Some(file) = root.and_then(|r| resolve(r, &head.path)) else {
        respond(&mut stream, "404 Not Found", "text/plain", b"not found", head_only);
        return;
    };
    match std::fs::read(&file) {
        Ok(body) => respond(&mut stream, "200 OK", content_type(&file), &body, head_only),
        Err(e) => {
            log!("wallpaper feed: cannot read {}: {e}", file.display());
            respond(&mut stream, "500 Internal Server Error", "text/plain", b"error", head_only);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..\\..\\assets\\wallpaper")
    }

    #[test]
    fn maps_the_named_pages() {
        assert!(resolve(&root(), "/lockscreen").unwrap().ends_with("lockscreen.html"));
        assert!(resolve(&root(), "/lockscreen?x=1").unwrap().ends_with("lockscreen.html"));
        assert!(resolve(&root(), "/").unwrap().ends_with("index.html"));
        assert!(resolve(&root(), "/js/app.js").unwrap().ends_with("app.js"));
    }

    #[test]
    fn refuses_paths_outside_the_folder() {
        for p in ["/../Cargo.toml", "/js/../../x", "/%2e%2e/x", "/C:/Windows/win.ini", "/js\\app.js", "/nope.html", "/js"] {
            assert!(resolve(&root(), p).is_none(), "{p}");
        }
    }

    #[test]
    fn hosts() {
        assert!(host_allowed(Some("127.0.0.1:9012")));
        assert!(host_allowed(Some("localhost:9012")));
        assert!(host_allowed(Some("[::1]:9012")));
        assert!(!host_allowed(Some("evil.example:9012")));
        assert!(!host_allowed(None));
    }

    #[test]
    fn parses_heads() {
        let h = parse_head(b"GET /lockscreen HTTP/1.1\r\nHost: 127.0.0.1:9012\r\n\r\n").unwrap();
        assert_eq!((h.method.as_str(), h.path.as_str(), h.upgrade_websocket), ("GET", "/lockscreen", false));
        let w = parse_head(b"GET /wallpaper HTTP/1.1\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\r\n").unwrap();
        assert!(w.upgrade_websocket);
        assert!(parse_head(b"GET / HTTP/1.1\r\nHost: x").is_none());
    }
}
