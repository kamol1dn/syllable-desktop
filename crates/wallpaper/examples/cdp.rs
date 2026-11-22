//! Evaluate a JavaScript expression in every wallpaper page of a debug build started
//! with `SYLLABLE_WEBVIEW_DEBUG_PORT` (Chrome DevTools Protocol, `Runtime.evaluate`).
//!
//!     cargo run -p syllable-wallpaper --example cdp -- 9223 "document.title"

use std::io::{Read, Write};
use std::net::TcpStream;

use serde_json::{json, Value};
use tungstenite::Message;

fn main() {
    let mut args = std::env::args().skip(1);
    let port: u16 = args.next().and_then(|a| a.parse().ok()).unwrap_or(9223);
    let expr = args.next().unwrap_or_else(|| "location.href".into());

    let mut s = TcpStream::connect(("127.0.0.1", port)).expect("connect to the debugging port");
    write!(s, "GET /json/list HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n").unwrap();
    // Headers and body may come in separate writes; read until the server closes.
    s.set_read_timeout(Some(std::time::Duration::from_secs(2))).unwrap();
    let mut raw = Vec::new();
    let mut buf = [0u8; 8192];
    while let Ok(n) = s.read(&mut buf) {
        if n == 0 {
            break;
        }
        raw.extend_from_slice(&buf[..n]);
        if raw.ends_with(b"]\n") || raw.ends_with(b"]") {
            break;
        }
    }
    let body = String::from_utf8_lossy(&raw).into_owned();
    let json = body.find('[').map(|i| &body[i..]).unwrap_or("");
    let targets: Vec<Value> = serde_json::from_str(json).expect("target list");
    for t in targets.iter().filter(|t| t["type"] == "page") {
        let url = t["webSocketDebuggerUrl"].as_str().unwrap_or_default();
        let stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
        let (mut ws, _) = tungstenite::client::client(url, stream).expect("devtools socket");
        // `--shot <file.png>` saves what the page renders instead of evaluating.
        let shot = expr.strip_prefix("--shot ").map(str::to_string);
        let call = match &shot {
            Some(_) => json!({ "id": 1, "method": "Page.captureScreenshot", "params": { "format": "png" } }),
            None => json!({ "id": 1, "method": "Runtime.evaluate", "params": { "expression": expr, "returnByValue": true, "awaitPromise": true } }),
        };
        ws.send(Message::text(call.to_string())).unwrap();
        loop {
            if let Ok(Message::Text(m)) = ws.read() {
                let v: Value = serde_json::from_str(m.as_str()).unwrap_or_default();
                if v["id"] == 1 {
                    if let Some(path) = &shot {
                        let b64 = v["result"]["data"].as_str().unwrap_or_default();
                        std::fs::write(path, decode_base64(b64)).unwrap();
                        println!("{} => saved {path}", t["url"].as_str().unwrap_or("?"));
                    } else {
                        let r = &v["result"]["result"];
                        println!("{} => {}", t["url"].as_str().unwrap_or("?"), r.get("value").unwrap_or(r));
                    }
                    break;
                }
            }
        }
    }
}

fn decode_base64(s: &str) -> Vec<u8> {
    let val = |c: u8| match c {
        b'A'..=b'Z' => c - b'A',
        b'a'..=b'z' => c - b'a' + 26,
        b'0'..=b'9' => c - b'0' + 52,
        b'+' => 62,
        _ => 63,
    } as u32;
    let bytes: Vec<u8> = s.bytes().filter(|&c| c != b'=' && !c.is_ascii_whitespace()).collect();
    let mut out = Vec::with_capacity(bytes.len() * 3 / 4);
    for chunk in bytes.chunks(4) {
        let n = chunk.iter().enumerate().fold(0u32, |n, (i, &c)| n | val(c) << (18 - 6 * i));
        out.push((n >> 16) as u8);
        if chunk.len() > 2 {
            out.push((n >> 8) as u8);
        }
        if chunk.len() > 3 {
            out.push(n as u8);
        }
    }
    out
}
