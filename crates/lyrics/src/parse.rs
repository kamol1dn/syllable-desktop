//! Provider payloads to `syllable_core::models`. Port of the prototype's
//! `LyricsNormalizer` (Models.cs). All output times are milliseconds.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use syllable_core::models::{BgVocal, LyricLine, Lyrics, Seg, SyncType};

/// How a result is timed. `Static` (unsynced) is parsed and cached but never published.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Kind {
    Syllable,
    Line,
    Static,
}

/// A provider's answer plus what the resolver needs beyond the shared model.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Found {
    pub lyrics: Lyrics,
    pub kind: Kind,
    /// Where the provider got it, for the log: "spicy_lyrics", "apple_music", "spotify" (Spicy API).
    #[serde(default)]
    pub origin: Option<String>,
    /// "uploaded by x", "made by y" (community syncs on the Spicy API).
    #[serde(default)]
    pub credits: Option<Vec<String>>,
}

impl Found {
    fn new(kind: Kind, source: &str) -> Self {
        let sync = if kind == Kind::Syllable { SyncType::Syllable } else { SyncType::Line };
        Self {
            lyrics: Lyrics { sync, source: source.to_string(), ..Lyrics::default() },
            kind,
            origin: None,
            credits: None,
        }
    }

    /// A "nothing found" record for the cache.
    pub fn negative() -> Self {
        Self::new(Kind::Line, "none")
    }

    /// Word timing beats line timing beats unsynced beats nothing.
    pub fn rank(&self) -> u8 {
        if self.lyrics.lines.is_empty() {
            return 0;
        }
        match self.kind {
            Kind::Syllable => 3,
            Kind::Line => 2,
            Kind::Static => 1,
        }
    }

    pub fn source(&self) -> &str {
        &self.lyrics.source
    }

    pub fn kind_name(&self) -> &'static str {
        match self.kind {
            Kind::Syllable => "Syllable",
            Kind::Line => "Line",
            Kind::Static => "Static",
        }
    }

    /// "Line (apple_music), 42 lines", for the log.
    pub fn describe(&self) -> String {
        let origin = self.origin.as_deref().map(|o| format!(" ({o})")).unwrap_or_default();
        format!("{}{origin}, {} lines", self.kind_name(), self.lyrics.lines.len())
    }
}

// ---- JSON helpers ----

fn s<'a>(v: &'a Value, name: &str) -> &'a str {
    v.get(name).and_then(Value::as_str).unwrap_or("")
}

fn num(v: &Value, name: &str) -> f64 {
    v.get(name).and_then(Value::as_f64).unwrap_or(0.0)
}

/// A number, or a string holding one (Spotify sends ms as strings).
fn ms(v: &Value, name: &str) -> f64 {
    match v.get(name) {
        Some(Value::Number(n)) => n.as_f64().unwrap_or(0.0),
        Some(Value::String(t)) => t.trim().parse().unwrap_or(0.0),
        _ => 0.0,
    }
}

fn is_true(v: &Value, name: &str) -> bool {
    v.get(name).and_then(Value::as_bool).unwrap_or(false)
}

fn blank(t: &str) -> bool {
    t.trim().is_empty()
}

// ---- Spicy Lyrics ----

/// A Spicy Lyrics payload (seconds). Takes the developer API's `{"Body": {...}}`
/// envelope or the bare lyrics object. `None` when nothing is usable.
pub fn from_spicy(root: &Value, source: &str) -> Option<Found> {
    let root = match root.get("Body") {
        Some(body) if body.is_object() => body,
        _ => root,
    };
    let kind = match s(root, "Type") {
        "Syllable" => Kind::Syllable,
        "Line" => Kind::Line,
        "Static" => Kind::Static,
        _ => return None,
    };
    let mut f = Found::new(kind, source);
    let origin = s(root, "source");
    if !origin.is_empty() {
        f.origin = Some(origin.to_string());
    }
    f.credits = read_credits(root);

    if kind == Kind::Static {
        for l in root.get("Lines")?.as_array()? {
            let text = s(l, "Text");
            if !blank(text) {
                f.lyrics.lines.push(LyricLine { text: text.trim().to_string(), ..LyricLine::default() });
            }
        }
        return (!f.lyrics.lines.is_empty()).then_some(f);
    }

    f.lyrics.start_time = num(root, "StartTime") * 1000.0;
    let content = root.get("Content")?.as_array()?;
    for item in content {
        let opposite = is_true(item, "OppositeAligned");
        if kind == Kind::Line {
            let text = s(item, "Text").trim().to_string();
            if !text.is_empty() {
                f.lyrics.lines.push(LyricLine {
                    text,
                    start: num(item, "StartTime") * 1000.0,
                    end: num(item, "EndTime") * 1000.0,
                    sylls: None,
                    opposite,
                });
            }
            continue;
        }

        // Syllable: { Lead: {StartTime, EndTime, Syllables[]}, Background?: [...] }
        if let Some(lead) = item.get("Lead") {
            let (text, segs) = read_syllables(lead);
            if let (false, Some(first), Some(last)) = (text.is_empty(), segs.first(), segs.last()) {
                let lead_start = num(lead, "StartTime") * 1000.0;
                f.lyrics.lines.push(LyricLine {
                    text,
                    start: if lead_start > 0.0 { lead_start.min(first.start) } else { first.start },
                    end: (num(lead, "EndTime") * 1000.0).max(last.end),
                    sylls: Some(segs),
                    opposite,
                });
            }
        }
        if let Some(bgs) = item.get("Background").and_then(Value::as_array) {
            for bg in bgs {
                let (text, segs) = read_syllables(bg);
                let (Some(first), Some(last)) = (segs.first(), segs.last()) else { continue };
                if text.is_empty() {
                    continue;
                }
                let bg_start = num(bg, "StartTime") * 1000.0;
                f.lyrics.bg.push(BgVocal {
                    text,
                    start: if bg_start > 0.0 { bg_start } else { first.start },
                    end: (num(bg, "EndTime") * 1000.0).max(last.end),
                    sylls: Some(segs),
                    opposite,
                });
            }
        }
    }

    f.lyrics.lines.sort_by(|a, b| a.start.total_cmp(&b.start));
    f.lyrics.bg.sort_by(|a, b| a.start.total_cmp(&b.start));
    if f.lyrics.lines.is_empty() {
        return None;
    }
    fill_line_ends(&mut f.lyrics.lines);
    if f.lyrics.start_time <= 0.0 {
        f.lyrics.start_time = f.lyrics.lines[0].start;
    }
    Some(f)
}

/// Lines with no usable end run until the next line starts (or 6 s).
pub fn fill_line_ends(lines: &mut [LyricLine]) {
    for i in 0..lines.len() {
        if lines[i].end <= lines[i].start {
            lines[i].end = match lines.get(i + 1) {
                Some(next) if next.start > lines[i].start => next.start,
                _ => lines[i].start + 6000.0,
            };
        }
    }
}

fn read_credits(root: &Value) -> Option<Vec<String>> {
    let a = root.get("UploadAttribution").filter(|a| a.is_object())?;
    let list: Vec<String> = [("Uploader", "uploaded by"), ("Maker", "made by")]
        .iter()
        .filter_map(|(key, label)| {
            let name = a.get(*key).filter(|w| w.is_object()).map(|w| s(w, "username"))?;
            (!name.is_empty()).then(|| format!("{label} {name}"))
        })
        .collect();
    (!list.is_empty()).then_some(list)
}

fn read_syllables(holder: &Value) -> (String, Vec<Seg>) {
    let mut segs = Vec::new();
    let mut text = String::new();
    for syl in holder.get("Syllables").and_then(Value::as_array).into_iter().flatten() {
        let t = s(syl, "Text");
        if t.is_empty() {
            continue;
        }
        let part = is_true(syl, "IsPartOfWord");
        let start = num(syl, "StartTime") * 1000.0;
        let seg = Seg {
            start,
            // A zero-length syllable would never visibly sweep.
            end: (num(syl, "EndTime") * 1000.0).max(start + 40.0),
            text: if part { t.to_string() } else { format!("{t} ") },
            part_of_word: part,
        };
        text.push_str(&seg.text);
        segs.push(seg);
    }
    (text.trim_end().to_string(), segs)
}

// ---- LRC (LRCLIB) ----

/// An LRC string into line-synced lyrics.
pub fn from_lrc(lrc: &str, duration_ms: f64) -> Option<Found> {
    let mut entries: Vec<(f64, String)> = Vec::new();
    for raw in lrc.split('\n') {
        let line = raw.trim_end_matches('\r');
        let (stamps, text) = lrc_stamps(line);
        for ms in stamps {
            entries.push((ms, text.trim().to_string()));
        }
    }
    entries.sort_by(|a, b| a.0.total_cmp(&b.0)); // stable: same stamps keep file order
    let mut f = Found::new(Kind::Line, "lrclib");
    for (i, (start, text)) in entries.iter().enumerate() {
        if blank(text) {
            continue; // empty stamps mark gaps
        }
        let end = match entries.get(i + 1) {
            Some((next, _)) => *next,
            None if duration_ms > *start => duration_ms,
            None => start + 6000.0,
        };
        f.lyrics.lines.push(LyricLine { text: text.clone(), start: *start, end, sylls: None, opposite: false });
    }
    if f.lyrics.lines.is_empty() {
        return None;
    }
    f.lyrics.start_time = f.lyrics.lines[0].start;
    Some(f)
}

/// Every `[mm:ss(.xx)]` stamp anywhere on the line, and the text after the last one.
/// Lines without a stamp give no stamps.
fn lrc_stamps(line: &str) -> (Vec<f64>, &str) {
    let b = line.as_bytes();
    let mut stamps = Vec::new();
    let mut after_last = 0;
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'[' {
            if let Some((ms, len)) = parse_stamp(&b[i..]) {
                stamps.push(ms);
                i += len;
                after_last = i;
                continue;
            }
        }
        i += 1;
    }
    let text = if stamps.is_empty() { "" } else { &line[after_last..] };
    (stamps, text)
}

/// `[12:34]`, `[12:34.56]`, `[12:34:567]` at the start of `b`: (ms, bytes consumed).
fn parse_stamp(b: &[u8]) -> Option<(f64, usize)> {
    fn digits(b: &[u8], from: usize) -> (usize, usize) {
        let mut end = from;
        while end < b.len() && b[end].is_ascii_digit() {
            end += 1;
        }
        (from, end)
    }
    fn value(b: &[u8], (from, to): (usize, usize)) -> Option<f64> {
        std::str::from_utf8(&b[from..to]).ok()?.parse::<f64>().ok()
    }
    let min = digits(b, 1);
    if min.0 == min.1 || b.get(min.1) != Some(&b':') {
        return None;
    }
    let sec = digits(b, min.1 + 1);
    if sec.0 == sec.1 {
        return None;
    }
    let mut ms = value(b, min)? * 60_000.0 + value(b, sec)? * 1000.0;
    let mut end = sec.1;
    if matches!(b.get(end), Some(b'.') | Some(b':')) {
        let frac = digits(b, end + 1);
        if frac.0 < frac.1 {
            let f = std::str::from_utf8(&b[frac.0..frac.1]).ok()?;
            ms += if f.len() == 2 {
                f.parse::<f64>().ok()? * 10.0
            } else {
                format!("{f:0<3}")[..3].parse::<f64>().ok()?
            };
            end = frac.1;
        }
        // "[01:02.]" falls through to the bracket check below, as the regex does not match it.
    }
    (b.get(end) == Some(&b']')).then_some((ms, end + 1))
}

// ---- Spotify (spclient color-lyrics v2) ----

/// Spotify's own lyrics. Times are strings of ms; syllable entries carry a start
/// time and either their text or a character count into the line. `UNSYNCED`
/// comes back as `Kind::Static`.
pub fn from_spotify(root: &Value) -> Option<Found> {
    let ly = root.get("lyrics").filter(|l| l.is_object())?;
    let arr = ly.get("lines")?.as_array()?;
    let sync = s(ly, "syncType");

    struct Raw<'a> {
        start: f64,
        end: f64,
        text: &'a str,
        syl: Option<&'a Vec<Value>>,
    }
    let raws: Vec<Raw> = arr
        .iter()
        .filter_map(|o| {
            let text = s(o, "words");
            if blank(text) || text.trim() == "♪" {
                return None;
            }
            let syl = o.get("syllables").and_then(Value::as_array).filter(|a| !a.is_empty());
            Some(Raw { start: ms(o, "startTimeMs"), end: ms(o, "endTimeMs"), text, syl })
        })
        .collect();
    if raws.is_empty() {
        return None;
    }

    if sync == "UNSYNCED" {
        let mut f = Found::new(Kind::Static, "spotify");
        for r in &raws {
            f.lyrics.lines.push(LyricLine { text: r.text.trim().to_string(), ..LyricLine::default() });
        }
        return Some(f);
    }

    let mut lines = Vec::with_capacity(raws.len());
    let mut any_word = false;
    for (i, r) in raws.iter().enumerate() {
        let next = raws.get(i + 1).map(|n| n.start).unwrap_or(r.start + 6000.0);
        let end = if r.end > r.start { r.end } else { next };
        let mut line = LyricLine { text: r.text.trim().to_string(), start: r.start, end, sylls: None, opposite: false };
        if let Some(segs) = r.syl.and_then(|sy| spotify_syllables(r.text, r.start, end, sy)) {
            any_word = true;
            line.text = segs.iter().map(|x| x.text.as_str()).collect::<String>().trim().to_string();
            line.end = end.max(segs.last().map(|x| x.end).unwrap_or(end));
            line.sylls = Some(segs);
        }
        lines.push(line);
    }
    let mut f = Found::new(if any_word { Kind::Syllable } else { Kind::Line }, "spotify");
    f.lyrics.start_time = lines[0].start;
    f.lyrics.lines = lines;
    Some(f)
}

fn spotify_syllables(text: &str, line_start: f64, line_end: f64, arr: &[Value]) -> Option<Vec<Seg>> {
    // Character counts are counts of UTF-16 units in the original; chars are the
    // closest safe unit here and agree for everything outside the astral planes.
    let chars: Vec<char> = text.chars().collect();
    let mut pieces: Vec<(String, f64, f64)> = Vec::new();
    let mut cursor = 0usize;
    for syl in arr {
        if !syl.is_object() {
            return None;
        }
        let start = ms(syl, "startTimeMs");
        let explicit = [s(syl, "words"), s(syl, "text"), s(syl, "syllable")]
            .into_iter()
            .find(|t| !t.is_empty())
            .unwrap_or("");
        let count = ms(syl, "numChars").max(ms(syl, "charsCount")) as i64;
        let piece: String = if !explicit.is_empty() {
            explicit.to_string()
        } else if count > 0 {
            let from = cursor.min(chars.len());
            chars[from..(from + count as usize).min(chars.len())].iter().collect()
        } else {
            return None;
        };
        cursor += piece.chars().count();
        pieces.push((piece, start, ms(syl, "endTimeMs")));
    }
    if pieces.is_empty() {
        return None;
    }
    // Remaining text (trailing punctuation) sticks to the last syllable.
    if cursor < chars.len() {
        let rest: String = chars[cursor..].iter().collect();
        pieces.last_mut().unwrap().0.push_str(&rest);
    }
    if pieces[0].1 < line_start - 5000.0 {
        return None;
    }

    let mut segs = Vec::with_capacity(pieces.len());
    for i in 0..pieces.len() {
        let (pt, ps, mut pe) = pieces[i].clone();
        if pe <= ps {
            pe = pieces.get(i + 1).map(|n| n.1).unwrap_or(line_end).max(ps + 80.0);
        }
        // A space on either side of the boundary closes the word; otherwise the
        // piece runs into the next one ("beau" + "ti" + "ful").
        let ends_word = i == pieces.len() - 1 || pt.ends_with(' ') || pieces[i + 1].0.starts_with(' ');
        let pt = pt.trim();
        if pt.is_empty() {
            continue;
        }
        segs.push(Seg {
            start: ps,
            end: pe,
            text: if ends_word { format!("{pt} ") } else { pt.to_string() },
            part_of_word: !ends_word,
        });
    }
    (!segs.is_empty()).then_some(segs)
}

#[cfg(test)]
mod tests {
    //! Ported from TaskbarLyrics.Tests/ParsingTests.cs. Spicy payloads follow the
    //! shapes in the Spicy Lyrics API reference.
    use super::*;

    fn spicy(json: &str, source: &str) -> Option<Found> {
        from_spicy(&serde_json::from_str(json).unwrap(), source)
    }

    fn spotify(json: &str) -> Option<Found> {
        from_spotify(&serde_json::from_str(json).unwrap())
    }

    fn close(a: f64, b: f64) {
        assert!((a - b).abs() < 1e-3, "{a} != {b}");
    }

    #[test]
    fn syllable_sync_with_background_and_duet() {
        let l = spicy(
            r#"{"Body":{"Type":"Syllable","StartTime":7.3,"EndTime":20.0,"id":"x","ids":{"spotify":"x"},
              "source":"spicy_lyrics",
              "UploadAttribution":{"Uploader":{"id":"1","username":"spikerko","url":"https://spicylyrics.org/uid/1"},
                                   "Maker":{"id":"2","username":"gc","url":"https://spicylyrics.org/uid/2"}},
              "Content":[
                {"Type":"Vocal","OppositeAligned":false,
                 "Lead":{"StartTime":7.3,"EndTime":9.0,"Syllables":[
                   {"Text":"Wan","StartTime":7.3,"EndTime":7.6,"IsPartOfWord":true},
                   {"Text":"na","StartTime":7.6,"EndTime":7.9,"IsPartOfWord":false},
                   {"Text":"dance","StartTime":7.9,"EndTime":9.0,"IsPartOfWord":false}]},
                 "Background":[{"StartTime":8.5,"EndTime":9.6,"Syllables":[
                   {"Text":"(oh","StartTime":8.5,"EndTime":9.0,"IsPartOfWord":false},
                   {"Text":"yeah)","StartTime":9.0,"EndTime":9.6,"IsPartOfWord":false}]}]},
                {"Type":"Vocal","OppositeAligned":true,
                 "Lead":{"StartTime":9.4,"EndTime":11.0,"Syllables":[
                   {"Text":"Me","StartTime":9.4,"EndTime":9.8,"IsPartOfWord":false},
                   {"Text":"too","StartTime":9.8,"EndTime":11.0,"IsPartOfWord":false}]}}
              ]},"Status":200,"Type":"object"}"#,
            "spicy-api",
        )
        .unwrap();

        assert_eq!(l.kind, Kind::Syllable);
        assert_eq!(l.lyrics.sync, SyncType::Syllable);
        assert_eq!(l.rank(), 3);
        assert_eq!(l.source(), "spicy-api");
        assert_eq!(l.origin.as_deref(), Some("spicy_lyrics"));
        assert_eq!(l.credits, Some(vec!["uploaded by spikerko".to_string(), "made by gc".to_string()]));
        close(l.lyrics.start_time, 7300.0);
        assert_eq!(l.lyrics.lines.len(), 2);

        let first = &l.lyrics.lines[0];
        assert_eq!(first.text, "Wanna dance");
        let texts: Vec<&str> = first.sylls.as_ref().unwrap().iter().map(|s| s.text.as_str()).collect();
        assert_eq!(texts, ["Wan", "na ", "dance "]);
        assert!(first.sylls.as_ref().unwrap()[0].part_of_word);
        close(first.start, 7300.0);
        close(first.end, 9000.0);
        assert!(!first.opposite);
        assert!(l.lyrics.lines[1].opposite);

        // Background vocals are flattened into bg, keeping their own timing.
        assert_eq!(l.lyrics.bg.len(), 1);
        let bg = &l.lyrics.bg[0];
        assert_eq!(bg.text, "(oh yeah)");
        close(bg.start, 8500.0);
        close(bg.end, 9600.0);
        assert!(!bg.opposite);
    }

    #[test]
    fn line_sync_from_apple_music() {
        let l = spicy(
            r#"{"Body":{"Type":"Line","id":"x","ids":{"spotify":"x"},"source":"apple_music","Content":[
              {"Type":"Vocal","Text":"First line here","StartTime":1.0,"EndTime":3.5,"OppositeAligned":false},
              {"Type":"Vocal","Text":"Second","StartTime":4.0,"EndTime":5.0,"OppositeAligned":true}]},"Status":200,"Type":"object"}"#,
            "spicy-api",
        )
        .unwrap();
        assert_eq!(l.kind, Kind::Line);
        assert_eq!(l.rank(), 2);
        assert_eq!(l.origin.as_deref(), Some("apple_music"));
        assert_eq!(l.credits, None);
        assert_eq!(l.lyrics.lines[0].text, "First line here");
        assert!(l.lyrics.lines[0].sylls.is_none());
        close(l.lyrics.lines[0].start, 1000.0);
        close(l.lyrics.lines[0].end, 3500.0);
        assert!(l.lyrics.lines[1].opposite);
    }

    #[test]
    fn line_without_end_runs_to_next_line() {
        let l = spicy(
            r#"{"Body":{"Type":"Line","Content":[
              {"Text":"a","StartTime":1.0},{"Text":"b","StartTime":4.0,"EndTime":0},{"Text":"   ","StartTime":5.0}]}}"#,
            "spicy-api",
        )
        .unwrap();
        assert_eq!(l.lyrics.lines.len(), 2); // blank line dropped
        close(l.lyrics.lines[0].end, 4000.0);
        close(l.lyrics.lines[1].end, 10000.0); // last line: +6 s
    }

    #[test]
    fn static_lyrics_parse_but_rank_below_synced() {
        let l = spicy(
            r#"{"Body":{"Type":"Static","id":"x","ids":{"spotify":"x"},"source":"spotify",
                "Lines":[{"Text":"One"},{"Text":"Two"}]},"Status":200,"Type":"object"}"#,
            "spicy-api",
        )
        .unwrap();
        assert_eq!(l.kind, Kind::Static);
        assert_eq!(l.rank(), 1);
        let texts: Vec<&str> = l.lyrics.lines.iter().map(|x| x.text.as_str()).collect();
        assert_eq!(texts, ["One", "Two"]);
    }

    #[test]
    fn payload_without_envelope_still_parses() {
        let l = spicy(
            r#"{"Type":"Syllable","StartTime":1,"Content":[{"Lead":{"StartTime":1,"EndTime":2,"Syllables":[
              {"Text":"Hi","StartTime":1,"EndTime":1,"IsPartOfWord":false}]}}]}"#,
            "spicy",
        )
        .unwrap();
        assert_eq!(l.source(), "spicy");
        assert_eq!(l.lyrics.lines[0].text, "Hi");
        // Zero-length syllables get a minimum so they still visibly sweep.
        close(l.lyrics.lines[0].sylls.as_ref().unwrap()[0].end, 1040.0);
    }

    #[test]
    fn unknown_or_empty_is_none() {
        assert!(spicy(r#"{"Body":{"Type":"Syllable","Content":[]}}"#, "spicy-api").is_none());
        assert!(spicy(r#"{"Body":{"Type":"Weird"}}"#, "spicy-api").is_none());
        assert!(spicy(r#"{"Body":{"Type":"Static","Lines":[]}}"#, "spicy-api").is_none());
    }

    #[test]
    fn spotify_line_synced() {
        let l = spotify(
            r#"{"lyrics":{"syncType":"LINE_SYNCED","lines":[
              {"startTimeMs":"1000","words":"Hello there","syllables":[],"endTimeMs":"0"},
              {"startTimeMs":"3500","words":"♪","syllables":[],"endTimeMs":"0"},
              {"startTimeMs":"5000","words":"General Kenobi","syllables":[],"endTimeMs":"0"}]}}"#,
        )
        .unwrap();
        assert_eq!(l.kind, Kind::Line);
        assert_eq!(l.source(), "spotify");
        let texts: Vec<&str> = l.lyrics.lines.iter().map(|x| x.text.as_str()).collect();
        assert_eq!(texts, ["Hello there", "General Kenobi"]);
        close(l.lyrics.lines[0].end, 5000.0);
        close(l.lyrics.lines[1].end, 11000.0);
    }

    #[test]
    fn spotify_syllables_by_character_count() {
        let l = spotify(
            r#"{"lyrics":{"syncType":"SYLLABLE_SYNCED","lines":[
              {"startTimeMs":"1000","endTimeMs":"3000","words":"Beautiful day!","syllables":[
                {"startTimeMs":"1000","numChars":"4"},{"startTimeMs":"1300","numChars":"2"},
                {"startTimeMs":"1500","numChars":"3"},{"startTimeMs":"2000","numChars":"4"}]}]}}"#,
        )
        .unwrap();
        assert_eq!(l.kind, Kind::Syllable);
        let segs = l.lyrics.lines[0].sylls.as_ref().unwrap();
        let texts: Vec<&str> = segs.iter().map(|s| s.text.as_str()).collect();
        assert_eq!(texts, ["Beau", "ti", "ful ", "day! "]);
        assert!(segs[0].part_of_word);
        assert!(!segs[2].part_of_word);
        close(segs[0].end, 1300.0); // no end: runs to the next syllable
        close(segs[3].end, 3000.0); // last: runs to the line end
        assert_eq!(l.lyrics.lines[0].text, "Beautiful day!");
    }

    #[test]
    fn spotify_character_counts_on_non_ascii_text() {
        let l = spotify(
            r#"{"lyrics":{"syncType":"SYLLABLE_SYNCED","lines":[
              {"startTimeMs":"1000","endTimeMs":"3000","words":"Привет мир","syllables":[
                {"startTimeMs":"1000","numChars":"3"},{"startTimeMs":"1300","numChars":"4"},
                {"startTimeMs":"1500","numChars":"9"}]}]}}"#,
        )
        .unwrap();
        let texts: Vec<String> =
            l.lyrics.lines[0].sylls.as_ref().unwrap().iter().map(|s| s.text.clone()).collect();
        assert_eq!(texts, ["При", "вет ", "мир "]);
    }

    #[test]
    fn spotify_unsynced() {
        let l = spotify(r#"{"lyrics":{"syncType":"UNSYNCED","lines":[{"startTimeMs":"0","words":"Just text"}]}}"#).unwrap();
        assert_eq!(l.kind, Kind::Static);
        assert_eq!(l.rank(), 1);
    }

    #[test]
    fn lrc_lines_stamps_and_gaps() {
        let l = from_lrc(
            "[ar: Someone]\n[00:01.50]First\r\n[00:04.00]\n[00:05.123][00:20.5]Twice\nno stamp\n[01:00:25]Last",
            70_000.0,
        )
        .unwrap();
        let got: Vec<(f64, f64, &str)> = l.lyrics.lines.iter().map(|x| (x.start, x.end, x.text.as_str())).collect();
        assert_eq!(
            got,
            [
                (1500.0, 4000.0, "First"),
                (5123.0, 20500.0, "Twice"),
                (20500.0, 60250.0, "Twice"),
                (60250.0, 70000.0, "Last"),
            ]
        );
        assert_eq!(l.kind, Kind::Line);
        assert_eq!(l.source(), "lrclib");
        close(l.lyrics.start_time, 1500.0);
        assert!(from_lrc("[00:01.00]\nplain", 0.0).is_none());
        // Last line with no known duration: +6 s.
        close(from_lrc("[00:02.00]Only", 0.0).unwrap().lyrics.lines[0].end, 8000.0);
    }

    #[test]
    fn found_round_trips_through_json() {
        let f = from_lrc("[00:01.00]Hello", 0.0).unwrap();
        let back: Found = serde_json::from_str(&serde_json::to_string(&f).unwrap()).unwrap();
        assert_eq!(back, f);
    }
}
