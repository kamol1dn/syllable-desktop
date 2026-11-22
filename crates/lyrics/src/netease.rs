//! NetEase Cloud Music, as the Android app's `NetEaseProvider`. Many songs,
//! western ones included, carry "YRC" lyrics with word timing:
//!   [lineStartMs,lineDurMs](wordStartMs,wordDurMs,0)word(wordStartMs,wordDurMs,0)word ...

use std::sync::LazyLock;

use regex::Regex;
use serde_json::Value;
use syllable_core::{matching, TrackInfo};
use ureq::Agent;

use crate::cache::NETEASE;
use crate::http::{self, enc, Token};
use crate::parse::{self, Found, Word};

const BROWSER_UA: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/142.0.0.0 Safari/537.36";
const HEADERS: [(&str, &str); 2] = [("Referer", "https://music.163.com/"), ("User-Agent", BROWSER_UA)];

static YRC_LINE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\[(\d+),(\d+)\](.*)$").unwrap());
static YRC_WORD_TAG: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\((\d+),(\d+),\d+\)").unwrap());
static CREDIT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^(作词|作曲|编曲|制作人|词|曲|Lyricist|Composer|Producer|Arranger|Written by)\s*[:：]").unwrap()
});
static LRC_TAGS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\[[^\]]*\]").unwrap());

fn get(agent: &Agent, url: &str) -> anyhow::Result<Option<String>> {
    let res = http::get(agent, url, &HEADERS)?;
    match res.status {
        404 => Ok(None),
        s if (200..300).contains(&s) => Ok(Some(res.body)),
        s => anyhow::bail!("NetEase: HTTP {s}"),
    }
}

pub fn fetch(agent: &Agent, info: &TrackInfo, token: &Token) -> anyhow::Result<Option<Found>> {
    let q = matching::query(info);
    if q.title.is_empty() || q.artist.is_empty() {
        return Ok(None);
    }
    token.check()?;
    let Some(id) = find_song_id(agent, &q)? else { return Ok(None) };
    token.check()?;
    let url = format!("https://music.163.com/api/song/lyric/v1?id={id}&lv=1&kv=0&tv=0&rv=0&yv=1&ytv=0&yrv=0");
    let Some(body) = get(agent, &url)? else { return Ok(None) };
    let doc: Value = serde_json::from_str(&body)?;
    let text = |p: &str| doc.pointer(p).and_then(Value::as_str).unwrap_or("").to_string();
    let (yrc, lrc) = (text("/yrc/lyric"), text("/lrc/lyric"));
    if !yrc.trim().is_empty() {
        if let Some(f) = parse_yrc(&yrc, &lrc) {
            return Ok(Some(f));
        }
    }
    // NetEase pads instrumentals with "纯音乐，请欣赏" ("pure music, enjoy").
    if lrc.trim().is_empty() || lrc.contains("纯音乐") {
        return Ok(None);
    }
    Ok(parse::from_lrc_as(&strip_credits(&lrc), info.duration_ms, NETEASE))
}

fn find_song_id(agent: &Agent, q: &matching::Query) -> anyhow::Result<Option<i64>> {
    let s = enc(&format!("{} {}", q.title, matching::primary_artist(&q.artist)));
    let Some(body) = get(agent, &format!("https://music.163.com/api/search/get?s={s}&type=1&limit=10"))? else {
        return Ok(None);
    };
    let doc: Value = serde_json::from_str(&body)?;
    Ok(best_song(&doc, q))
}

/// The best-scoring search result, unless every one is clearly another song.
fn best_song(doc: &Value, q: &matching::Query) -> Option<i64> {
    let mut best = None;
    let mut best_score = 0.0;
    for song in doc.pointer("/result/songs").and_then(Value::as_array).into_iter().flatten() {
        let artists: Vec<&str> = song
            .get("artists")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|a| a.get("name").and_then(Value::as_str))
            .collect();
        let name = song.get("name").and_then(Value::as_str).unwrap_or("");
        let dur = song.get("duration").and_then(Value::as_f64).unwrap_or(0.0);
        let score = matching::match_score(q, name, &artists.join(", "), dur);
        if score > best_score {
            best_score = score;
            best = song.get("id").and_then(Value::as_i64);
        }
    }
    best
}

fn is_credit(text: &str) -> bool {
    CREDIT.is_match(text.trim())
}

fn strip_credits(lrc: &str) -> String {
    lrc.lines().filter(|l| !is_credit(&LRC_TAGS.replace_all(l, ""))).collect::<Vec<_>>().join("\n")
}

/// Full-width ASCII ("（Oh）") to half-width, for Latin lyrics.
fn half_width(t: &str) -> String {
    t.chars()
        .map(|c| match c {
            '！'..='～' => char::from_u32(c as u32 - 0xFEE0).unwrap_or(c),
            '　' => ' ',
            c => c,
        })
        .collect()
}

fn squash(s: &str) -> String {
    s.chars().filter(|c| !c.is_whitespace()).flat_map(char::to_lowercase).collect()
}

/// YRC sometimes drops the spaces between words; the line-level LRC of the same
/// song has them, so copy them over when a line's letters agree.
fn respace(words: Vec<Word>, start: f64, lrc_lines: &[(f64, String)]) -> Vec<Word> {
    let joined = squash(&words.iter().map(|w| w.text.as_str()).collect::<String>());
    let Some((_, reference)) = lrc_lines.iter().find(|(s, t)| (s - start).abs() < 1500.0 && squash(t) == joined) else {
        return words;
    };
    let r: Vec<char> = reference.chars().collect();
    let mut i = 0;
    words
        .into_iter()
        .map(|w| {
            let mut out = String::new();
            let mut need = w.text.chars().filter(|c| !c.is_whitespace()).count();
            while i < r.len() && r[i].is_whitespace() && out.is_empty() {
                i += 1;
            }
            while i < r.len() && need > 0 {
                if !r[i].is_whitespace() {
                    need -= 1;
                }
                out.push(r[i]);
                i += 1;
            }
            while i < r.len() && r[i].is_whitespace() {
                out.push(' ');
                i += 1;
            }
            let mut text = String::new();
            for c in out.chars() {
                if !(c == ' ' && text.ends_with(' ')) {
                    text.push(c);
                }
            }
            Word { text, ..w }
        })
        .collect()
}

/// `lrc` (the same song, line timing) restores spaces YRC drops between words.
pub fn parse_yrc(yrc: &str, lrc: &str) -> Option<Found> {
    let lrc_lines: Vec<(f64, String)> = if lrc.trim().is_empty() {
        Vec::new()
    } else {
        parse::from_lrc_as(lrc, 0.0, NETEASE)
            .map(|f| f.lyrics.lines.into_iter().map(|l| (l.start, l.text)).collect())
            .unwrap_or_default()
    };
    let mut lines = Vec::new();
    for raw in yrc.lines() {
        let Some(m) = YRC_LINE.captures(raw.trim()) else { continue }; // JSON credit lines start with "{"
        let start: f64 = m[1].parse().unwrap_or(0.0);
        let dur: f64 = m[2].parse().unwrap_or(0.0);
        let body = &m[3];
        let tags: Vec<regex::Captures> = YRC_WORD_TAG.captures_iter(body).collect();
        let mut raw_words: Vec<Word> = Vec::new();
        for (i, c) in tags.iter().enumerate() {
            let whole = c.get(0).unwrap();
            let text_end = tags.get(i + 1).map(|n| n.get(0).unwrap().start()).unwrap_or(body.len());
            let text = &body[whole.end()..text_end];
            if text.is_empty() {
                continue;
            }
            let ws: f64 = c[1].parse().unwrap_or(0.0);
            let wd: f64 = c[2].parse().unwrap_or(0.0);
            raw_words.push(Word { text: text.to_string(), start: ws, end: ws + wd });
        }
        if raw_words.is_empty() {
            continue;
        }
        if raw_words.iter().any(|w| w.text.chars().any(|c| c.is_ascii_alphabetic())) {
            for w in &mut raw_words {
                w.text = half_width(&w.text);
            }
        }
        // Leading spaces belong to the gap before a word, not to the word itself.
        let n = raw_words.len();
        let words: Vec<Word> = (0..n)
            .map(|i| {
                let w = &raw_words[i];
                let mut t = w.text.trim_start().to_string();
                let next_spaced = raw_words.get(i + 1).is_some_and(|x| x.text.starts_with(char::is_whitespace));
                if !t.is_empty() && next_spaced && !t.ends_with(char::is_whitespace) {
                    t.push(' ');
                }
                Word { text: t, ..w.clone() } // runs of spaces collapse in `from_words`
            })
            .filter(|w| !w.text.trim().is_empty())
            .collect();
        if words.is_empty() {
            continue;
        }
        let text: String = words.iter().map(|w| w.text.as_str()).collect();
        if is_credit(&text) {
            continue;
        }
        lines.push((start, start + dur, respace(words, start, &lrc_lines)));
    }
    parse::from_words(lines, NETEASE)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn yrc_words_spaces_and_credits() {
        let yrc = "{\"t\":0,\"c\":[{\"tx\":\"作词: \"}]}\n\
                   [1000,2000](1000,500,0)Hello(1500,500,0)world\n\
                   [4000,1000](4000,500,0)作词：(4500,500,0)某人";
        let lrc = "[00:01.00]Hello world";
        let f = parse_yrc(yrc, lrc).unwrap();
        assert_eq!(f.rank(), 3);
        assert_eq!(f.lyrics.lines.len(), 1, "the credit line is dropped");
        let line = &f.lyrics.lines[0];
        assert_eq!(line.text, "Hello world", "the space comes from the LRC");
        assert_eq!((line.start, line.end), (1000.0, 3000.0));
        let segs = line.sylls.as_ref().unwrap();
        assert_eq!((segs[1].start, segs[1].end), (1500.0, 2000.0));
        assert!(!segs[0].part_of_word);
    }

    #[test]
    fn full_width_punctuation_in_latin_lines() {
        let f = parse_yrc("[0,1000](0,500,0)（Oh）(500,500,0) yeah", "").unwrap();
        assert_eq!(f.lyrics.lines[0].text, "(Oh) yeah");
    }

    #[test]
    fn search_takes_the_matching_artist() {
        let doc: Value = serde_json::from_str(
            r#"{"result":{"songs":[
                {"id":1,"name":"Phoenix","artists":[{"name":"Stray Kids"}],"duration":182013},
                {"id":2,"name":"Phoenix","artists":[{"name":"League of Legends"},{"name":"Cailin Russo"}],"duration":197633}]}}"#,
        )
        .unwrap();
        let info = TrackInfo {
            title: "Phoenix".into(),
            artist: "League of Legends".into(),
            duration_ms: 184_000.0,
            is_spotify: true,
            source_app: "Spotify.exe".into(),
            ..Default::default()
        };
        assert_eq!(best_song(&doc, &matching::query(&info)), Some(2));
    }
}
