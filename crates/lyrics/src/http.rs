//! Blocking HTTP (ureq) and cancellation for the worker threads.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use ureq::Agent;

pub const USER_AGENT: &str = concat!("Syllable/", env!("CARGO_PKG_VERSION"));

/// Status codes come back as values, not errors: callers need 204/401/404/429/503.
pub fn agent() -> Agent {
    Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(15)))
        .http_status_as_error(false)
        .user_agent(USER_AGENT)
        .build()
        .into()
}

pub struct Response {
    pub status: u16,
    pub body: String,
    pub retry_after: Option<String>,
}

impl Response {
    pub fn ok(&self) -> bool {
        (200..300).contains(&self.status)
    }
}

fn finish(res: Result<ureq::http::Response<ureq::Body>, ureq::Error>) -> anyhow::Result<Response> {
    let mut res = res?;
    let status = res.status().as_u16();
    let retry_after = res.headers().get("retry-after").and_then(|v| v.to_str().ok()).map(str::to_string);
    // A body that fails to read on an error status is not worth failing over.
    let body = match res.body_mut().with_config().limit(16 * 1024 * 1024).read_to_string() {
        Ok(b) => b,
        Err(e) if (200..300).contains(&status) => return Err(e.into()),
        Err(_) => String::new(),
    };
    Ok(Response { status, body, retry_after })
}

pub fn get(agent: &Agent, url: &str, headers: &[(&str, &str)]) -> anyhow::Result<Response> {
    let mut req = agent.get(url);
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    finish(req.call())
}

pub fn post_form(agent: &Agent, url: &str, form: &[(&str, &str)]) -> anyhow::Result<Response> {
    finish(agent.post(url).send_form(form.iter().copied()))
}

/// Like .NET `Uri.EscapeDataString`: everything but unreserved characters is %-encoded.
pub fn enc(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 3);
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Percent-decoding for query strings (`+` is a space).
pub fn dec(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let hex = |c: u8| (c as char).to_digit(16).map(|d| d as u8);
        match b[i] {
            b'+' => out.push(b' '),
            b'%' => {
                if let (Some(&h), Some(&l)) = (b.get(i + 1), b.get(i + 2)) {
                    if let (Some(h), Some(l)) = (hex(h), hex(l)) {
                        out.push(h * 16 + l);
                        i += 3;
                        continue;
                    }
                }
                out.push(b'%');
            }
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The track changed (or the app is quitting) while this work was running.
#[derive(Debug)]
pub struct Cancelled;

impl std::fmt::Display for Cancelled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("cancelled")
    }
}

impl std::error::Error for Cancelled {}

pub fn is_cancelled(e: &anyhow::Error) -> bool {
    e.is::<Cancelled>()
}

/// A generation counter: work started for generation N is cancelled once the
/// counter moves on. Optionally also by a deadline.
#[derive(Clone)]
pub struct Token {
    counter: Arc<AtomicU64>,
    gen: u64,
    deadline: Option<Instant>,
}

impl Token {
    /// A token that is never cancelled (tests, one-off commands).
    pub fn none() -> Self {
        Self { counter: Arc::new(AtomicU64::new(0)), gen: 0, deadline: None }
    }

    pub fn with_deadline(mut self, after: Duration) -> Self {
        self.deadline = Some(Instant::now() + after);
        self
    }

    pub fn cancelled(&self) -> bool {
        self.counter.load(Ordering::SeqCst) != self.gen || self.deadline.is_some_and(|d| Instant::now() >= d)
    }

    pub fn check(&self) -> anyhow::Result<()> {
        if self.cancelled() {
            Err(Cancelled.into())
        } else {
            Ok(())
        }
    }

    /// Sleep in small slices, stopping early when cancelled.
    pub fn sleep(&self, d: Duration) -> anyhow::Result<()> {
        let end = Instant::now() + d;
        loop {
            self.check()?;
            let now = Instant::now();
            if now >= end {
                return Ok(());
            }
            std::thread::sleep((end - now).min(Duration::from_millis(50)));
        }
    }
}

/// Hands out tokens; `next()` cancels everything handed out before.
#[derive(Clone, Default)]
pub struct Generation {
    counter: Arc<AtomicU64>,
}

impl Generation {
    pub fn next(&self) -> Token {
        let gen = self.counter.fetch_add(1, Ordering::SeqCst) + 1;
        Token { counter: self.counter.clone(), gen, deadline: None }
    }

    pub fn cancel(&self) {
        self.counter.fetch_add(1, Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encoding_round_trips() {
        assert_eq!(enc("a b&c/é"), "a%20b%26c%2F%C3%A9");
        assert_eq!(dec("a%20b+c%2F%C3%A9%"), "a b c/é%");
        assert_eq!(dec("%zz"), "%zz");
    }

    #[test]
    fn a_newer_generation_cancels_older_tokens() {
        let g = Generation::default();
        let a = g.next();
        assert!(!a.cancelled());
        let b = g.next();
        assert!(a.cancelled());
        assert!(!b.cancelled());
        g.cancel();
        assert!(b.cancelled());
        assert!(is_cancelled(&b.sleep(Duration::from_secs(5)).unwrap_err()));
    }
}
