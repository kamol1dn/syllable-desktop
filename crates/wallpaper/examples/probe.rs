//! Connect to the running app's wallpaper feed like a page does and print what arrives.
//!
//!     cargo run -p syllable-wallpaper --example probe -- [port] [seconds]

use std::net::TcpStream;
use std::time::{Duration, Instant};

use tungstenite::Message;

fn main() {
    let mut args = std::env::args().skip(1);
    let port: u16 = args.next().and_then(|a| a.parse().ok()).unwrap_or(9012);
    let secs: u64 = args.next().and_then(|a| a.parse().ok()).unwrap_or(3);
    let stream = TcpStream::connect(("127.0.0.1", port)).expect("connect");
    let (mut ws, _) = tungstenite::client::client(format!("ws://127.0.0.1:{port}/wallpaper?screen=probe"), stream).expect("handshake");
    ws.send(Message::text(r#"{"type":"audio","on":true}"#)).unwrap();
    ws.send(Message::text(r#"{"type":"list-wallpapers","dir":"%WINDIR%\\Web\\Wallpaper"}"#)).unwrap();
    ws.get_ref().set_read_timeout(Some(Duration::from_millis(200))).unwrap();
    let end = Instant::now() + Duration::from_secs(secs);
    let (mut pos, mut audio) = (0, 0);
    while Instant::now() < end {
        match ws.read() {
            Ok(Message::Text(t)) => {
                let s = t.as_str();
                if s.contains(r#""type":"pos""#) {
                    pos += 1;
                    if pos % 4 != 1 {
                        continue;
                    }
                } else if s.contains(r#""type":"audio""#) {
                    audio += 1;
                    if audio % 30 != 1 {
                        continue;
                    }
                }
                let shown: String = s.chars().take(220).collect();
                println!("{shown}{}", if s.len() > 220 { format!(" ... ({} bytes)", s.len()) } else { String::new() });
            }
            Ok(_) => {}
            Err(tungstenite::Error::Io(e)) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {}
            Err(e) => {
                println!("error: {e}");
                break;
            }
        }
    }
    println!("-- {pos} pos and {audio} audio messages in {secs} s");
}
