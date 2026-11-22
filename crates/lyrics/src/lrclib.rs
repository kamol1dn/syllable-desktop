//! LRCLIB (https://lrclib.net): free, no setup, line timing at best.

use serde_json::Value;
use syllable_core::{log, matching, TrackInfo};
use ureq::Agent;

use crate::http::{self, enc, Token};
use crate::parse::{self, Found};

/// `Ok(None)` is an answer ("they have nothing"); `Err` means LRCLIB could not be reached.
pub fn fetch(agent: &Agent, info: &TrackInfo, token: &Token) -> anyhow::Result<Option<Found>> {
    let mut last_error: Option<anyhow::Error> = None;
    let mut answered = false;
    let dur = info.duration_ms;
    for (artist, title) in matching::candidates(&info.title, &info.artist) {
        token.check()?;
        match try_candidate(agent, &artist, &title, dur, &mut answered) {
            Ok(Some(lrc)) => return Ok(parse::from_lrc(&lrc, dur)),
            Ok(None) => {}
            Err(e) => {
                log!("resolve: lrclib error: {e}");
                last_error = Some(e);
            }
        }
    }
    if let (false, Some(e)) = (answered, last_error) {
        anyhow::bail!("lrclib unreachable: {e}");
    }
    Ok(None)
}

fn try_candidate(agent: &Agent, artist: &str, title: &str, dur: f64, answered: &mut bool) -> anyhow::Result<Option<String>> {
    if !artist.is_empty() && dur > 1000.0 {
        let url = format!(
            "https://lrclib.net/api/get?artist_name={}&track_name={}&duration={}",
            enc(artist),
            enc(title),
            (dur / 1000.0) as i64
        );
        let res = http::get(agent, &url, &[])?;
        if res.ok() || res.status == 404 {
            *answered = true;
        }
        if res.ok() {
            if let Some(lrc) = synced(&serde_json::from_str(&res.body).unwrap_or(Value::Null)) {
                return Ok(Some(lrc.to_string()));
            }
        }
    }

    let mut url = format!("https://lrclib.net/api/search?track_name={}", enc(title));
    if !artist.is_empty() {
        url.push_str(&format!("&artist_name={}", enc(artist)));
    }
    let res = http::get(agent, &url, &[])?;
    if !res.ok() {
        return Ok(None);
    }
    *answered = true;
    let doc: Value = serde_json::from_str(&res.body).unwrap_or(Value::Null);
    Ok(pick_search_result(&doc, dur).map(str::to_string))
}

fn synced(v: &Value) -> Option<&str> {
    v.get("syncedLyrics").and_then(Value::as_str).filter(|s| !s.is_empty())
}

/// The synced result closest in duration, if it is within 10 s (or durations are unknown).
fn pick_search_result(doc: &Value, dur: f64) -> Option<&str> {
    let results = doc.as_array()?;
    let duration = |e: &Value| e.get("duration").and_then(Value::as_f64);
    let best = results
        .iter()
        .filter(|e| synced(e).is_some())
        .min_by(|a, b| {
            let key = |e: &Value| match duration(e) {
                Some(d) if dur > 1000.0 => (d * 1000.0 - dur).abs(),
                _ => f64::MAX,
            };
            key(a).total_cmp(&key(b))
        })?;
    let bdur = duration(best).map(|d| d * 1000.0).unwrap_or(0.0);
    (dur < 1000.0 || bdur <= 0.0 || (bdur - dur).abs() < 10_000.0).then(|| synced(best)).flatten()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn search_picks_the_closest_synced_duration() {
        let doc: Value = serde_json::from_str(
            r#"[{"duration":300,"syncedLyrics":"[00:01.00]far"},
                {"duration":201,"syncedLyrics":"[00:01.00]near"},
                {"duration":200,"syncedLyrics":null,"plainLyrics":"plain"}]"#,
        )
        .unwrap();
        assert_eq!(pick_search_result(&doc, 200_000.0), Some("[00:01.00]near"));
        // Only a far one: rejected.
        let far: Value = serde_json::from_str(r#"[{"duration":300,"syncedLyrics":"[00:01.00]far"}]"#).unwrap();
        assert_eq!(pick_search_result(&far, 200_000.0), None);
        // Unknown duration: the first synced one is fine.
        assert_eq!(pick_search_result(&far, 0.0), Some("[00:01.00]far"));
    }
}
