//! Title/artist matching: cleans messy media metadata (YouTube video titles
//! especially) into (artist, title) candidates and scores catalog search results
//! against them. Port of the prototype's `Matching.cs`.

use std::collections::HashSet;
use std::sync::LazyLock;

use regex::Regex;

/// A catalog search result (Spotify, or any provider with the same shape).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TrackCandidate {
    pub id: String,
    pub name: String,
    pub artists: Vec<String>,
    pub duration_ms: f64,
}

fn re(pattern: &str) -> Regex {
    Regex::new(pattern).expect("static regex")
}

static JUNK_GROUPS: LazyLock<Regex> = LazyLock::new(|| {
    re(r"(?i)[(\[][^)\]]*\b(official|video|audio|lyrics?|visuali[sz]er|m/?v|hd|hq|4k|remaster(ed)?|live|color coded|performance|explicit|clean|full)\b[^)\]]*[)\]]")
});
static TRAILING_PIPE: LazyLock<Regex> = LazyLock::new(|| re(r"\s*(\||//).*$"));
static FEAT_PART: LazyLock<Regex> = LazyLock::new(|| {
    re(r"(?i)\s*[(\[]\b(ft\.?|feat\.?|featuring|with)\b[^)\]]*[)\]]|\s*\b(ft\.?|feat\.?|featuring)\b.*$")
});
static PUNCT: LazyLock<Regex> = LazyLock::new(|| re(r"[^\w\s]"));
static SPACES: LazyLock<Regex> = LazyLock::new(|| re(r"\s+"));
static VEVO: LazyLock<Regex> = LazyLock::new(|| re(r"(?i)VEVO$"));
/// "Artist - Title" inside a title.
static DASHED: LazyLock<Regex> = LazyLock::new(|| re(r"^(.{1,60}?)\s*[-–—]\s+(.+)$"));

pub fn clean_title(title: &str) -> String {
    let t = JUNK_GROUPS.replace_all(title, " ");
    let t = TRAILING_PIPE.replace_all(&t, "");
    SPACES
        .replace_all(&t, " ")
        .trim_matches(|c| matches!(c, ' ' | '-' | '–' | '—'))
        .to_string()
}

pub fn clean_artist(artist: &str) -> String {
    let mut a = artist.trim();
    const TOPIC: &str = " - topic"; // YouTube auto-generated channels
    if a.len() >= TOPIC.len() {
        let cut = a.len() - TOPIC.len();
        if a.is_char_boundary(cut) && a[cut..].eq_ignore_ascii_case(TOPIC) {
            a = &a[..cut];
        }
    }
    VEVO.replace(a, "").trim().to_string()
}

/// False for media with nothing to identify a song by: no artist and no
/// "Artist - Title" in the title. Browser tabs look like this (an Instagram reel
/// reports just "Instagram"), and a title-only search then matches an unrelated
/// song that happens to share the page's name.
pub fn is_identifiable(raw_title: &str, raw_artist: &str) -> bool {
    !clean_artist(raw_artist).is_empty() || DASHED.is_match(&clean_title(raw_title))
}

/// Best-first list of (artist, title) interpretations of the metadata.
pub fn candidates(raw_title: &str, raw_artist: &str) -> Vec<(String, String)> {
    let title = clean_title(raw_title);
    let artist = clean_artist(raw_artist);
    let mut list = Vec::new();

    // "Artist - Title" embedded in the video title beats the channel name.
    if let Some(m) = DASHED.captures(&title) {
        list.push((m[1].trim().to_string(), FEAT_PART.replace_all(&m[2], " ").trim().to_string()));
    }
    if !artist.is_empty() {
        list.push((artist.clone(), FEAT_PART.replace_all(&title, " ").trim().to_string()));
    }
    list.push((String::new(), title));

    let mut seen = HashSet::new();
    list.into_iter().filter(|c| !c.1.is_empty() && seen.insert(c.clone())).collect()
}

/// Lower-case, punctuation to spaces, single spaces.
pub fn norm(s: &str) -> String {
    let lower = s.to_lowercase();
    let no_punct = PUNCT.replace_all(&lower, " ");
    SPACES.replace_all(&no_punct, " ").trim().to_string()
}

fn token_sim(a: &str, b: &str) -> f64 {
    let na = norm(a);
    let nb = norm(b);
    let ta: HashSet<&str> = na.split(' ').filter(|s| !s.is_empty()).collect();
    let tb: HashSet<&str> = nb.split(' ').filter(|s| !s.is_empty()).collect();
    if ta.is_empty() || tb.is_empty() {
        return 0.0;
    }
    let inter = ta.intersection(&tb).count() as f64;
    let mut sim = inter / ta.len().max(tb.len()) as f64;

    // Squashed comparison rescues space-less names: "BillieEilish" (VEVO channel)
    // vs "Billie Eilish" share no token but are the same string without spaces.
    if sim < 1.0 {
        let sa = na.replace(' ', "");
        let sb = nb.replace(' ', "");
        if sa.chars().count() > 2 && sb.chars().count() > 2 && (sa == sb || sa.contains(&sb) || sb.contains(&sa)) {
            sim = sim.max(0.9);
        }
    }
    sim
}

/// Score a search result against a metadata candidate, roughly 0..1.35.
pub fn score(track: &TrackCandidate, cand: &(String, String), duration_ms: f64) -> f64 {
    let (artist, title) = cand;
    let mut title_sim = token_sim(&track.name, title);
    // Containment: "Blinding Lights" inside "Blinding Lights (Remix)".
    let n_name = norm(&track.name);
    let n_title = norm(title);
    if !n_name.is_empty() && !n_title.is_empty() && (n_name.contains(&n_title) || n_title.contains(&n_name)) {
        title_sim = title_sim.max(0.85);
    }

    let artist_sim = if artist.is_empty() {
        0.0
    } else {
        track.artists.iter().map(|a| token_sim(a, artist)).fold(0.0, f64::max)
    };

    // Duration boosts when it agrees but only mildly penalises when it doesn't:
    // music videos carry intro/outro padding.
    let mut dur_bonus = 0.0;
    if duration_ms > 1000.0 && track.duration_ms > 0.0 {
        let diff = (track.duration_ms - duration_ms).abs();
        dur_bonus = if diff < 3000.0 {
            0.35
        } else if diff < 8000.0 {
            0.2
        } else if diff < 25000.0 {
            0.0
        } else {
            -0.15
        };
    }
    title_sim * 0.6 + artist_sim * 0.4 + dur_bonus
}

/// The best result and its score; the result is `None` below the 0.55 threshold.
pub fn pick_best<'a>(
    results: &'a [TrackCandidate],
    cands: &[(String, String)],
    duration_ms: f64,
) -> (Option<&'a TrackCandidate>, f64) {
    let mut best = None;
    let mut best_score = 0.0;
    for track in results {
        for cand in cands {
            let s = score(track, cand, duration_ms);
            if s > best_score {
                best_score = s;
                best = Some(track);
            }
        }
    }
    (if best_score >= 0.55 { best } else { None }, best_score)
}

// ---------------------------------------------------------------------------
// The Android app's matching (`TextMatch.kt`), which the lyrics providers use:
// one cleaned (title, artist) query per song, and a score that rejects results
// that are clearly another song instead of falling back to a title-only search.
// ---------------------------------------------------------------------------

static NOISE_IN_BRACKETS: LazyLock<Regex> = LazyLock::new(|| {
    re(r"(?i)\s*[(\[][^)\]]*(official|video|audio|lyric|visuali[sz]er|remaster|live at|explicit|clean|hd|hq|4k|mv|m/v|from .*soundtrack)[^)\]]*[)\]]")
});
static FEAT_IN_BRACKETS: LazyLock<Regex> = LazyLock::new(|| re(r"(?i)\s*[(\[](feat\.?|ft\.?|featuring|with)\s[^)\]]*[)\]]"));
static FEAT_TAIL: LazyLock<Regex> = LazyLock::new(|| re(r"(?i)\s+(feat\.?|ft\.?|featuring)\s.*$"));
/// " - Remastered 2011", " - Radio Edit", " - New Version": the same song.
static DASH_SUFFIX: LazyLock<Regex> =
    LazyLock::new(|| re(r"(?i)\s+-\s+(.*(remaster|version|edit|mix|mono|stereo|live|acoustic|radio).*)$"));
static ARTIST_NOISE: LazyLock<Regex> = LazyLock::new(|| re(r"(?i)(\s*-\s*topic$)|(vevo$)|(\s*official$)"));
static PRIMARY_SPLIT: LazyLock<Regex> = LazyLock::new(|| re(r"(?i)\s*(,|&| x | and |;|/| feat\.? | ft\.? )\s*"));

/// How much earlier than the catalog length Spotify desktop's media session may
/// end a track. Its end time is where playback moves on, not the track length:
/// seen 4 s to 31 s short (crossfade/automix pull it in), never longer.
pub const SPOTIFY_EARLY_END_MAX_MS: f64 = 40_000.0;

/// What the providers search for.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Query {
    pub title: String,
    pub artist: String,
    pub album: String,
    /// 0 when unknown.
    pub duration_ms: f64,
    /// The duration is where the player moves on (Spotify desktop), so the song
    /// itself may be up to `SPOTIFY_EARLY_END_MAX_MS` longer.
    pub may_end_early: bool,
}

impl Query {
    /// The duration to send to a service that matches on it strictly, or `None`
    /// when it can't be trusted.
    pub fn strict_duration_ms(&self) -> Option<f64> {
        (self.duration_ms > 1000.0 && !self.may_end_early).then_some(self.duration_ms)
    }
}

/// The Android app's query for a track, plus one desktop rule: outside Spotify,
/// an "Artist - Title" in the title beats the media session's artist, which is a
/// browser tab's channel name.
pub fn query(info: &crate::TrackInfo) -> Query {
    let (mut title, mut artist) = clean(&info.title, &info.artist);
    if !info.is_spotify {
        if let Some(m) = DASHED.captures(&title) {
            let (a, t) = (m[1].trim().to_string(), m[2].trim().to_string());
            if !a.is_empty() && !t.is_empty() {
                (artist, title) = (a, t);
            }
        }
    }
    Query {
        title,
        artist,
        album: info.album.trim().to_string(),
        duration_ms: info.duration_ms,
        may_end_early: info.source_app.to_lowercase().contains("spotify"),
    }
}

pub fn clean_artist_android(artist: &str) -> String {
    ARTIST_NOISE.replace_all(artist.trim(), "").trim().to_string()
}

/// Main artist only ("A, B & C" -> "A").
pub fn primary_artist(artist: &str) -> String {
    match PRIMARY_SPLIT.split(artist).next().map(str::trim) {
        Some(first) if !first.is_empty() => first.to_string(),
        _ => artist.to_string(),
    }
}

/// (title, artist) cleaned. "Artist - Title" in the title is split only when the
/// left half names the artist (or there is no artist): on Spotify the dash
/// usually starts a version ("SLAVA FUNK! - Slowed").
pub fn clean(title: &str, artist: &str) -> (String, String) {
    let mut a = clean_artist_android(artist);
    let t = NOISE_IN_BRACKETS.replace_all(title.trim(), "");
    let t = FEAT_IN_BRACKETS.replace_all(&t, "");
    let mut t = DASH_SUFFIX.replace_all(&t, "").into_owned();
    if let Some(dash) = t.find(" - ").filter(|&d| d > 0) {
        let left = t[..dash].trim().to_string();
        let right = t[dash + 3..].trim().to_string();
        let (nl, na) = (normalize(&left), normalize(&a));
        if !right.is_empty() && (na.is_empty() || (!nl.is_empty() && (nl.contains(&na) || na.contains(&nl)))) {
            a = left;
            t = right;
        }
    }
    let t = FEAT_TAIL.replace_all(&t, "");
    let t = SPACES.replace_all(&t, " ");
    let t = t.trim().trim_matches(|c| matches!(c, '"' | '\'' | '“' | '”'));
    (t.to_string(), a)
}

/// Lower-case letters and digits only, "&" as "and". (Android also strips
/// accents; LCS similarity keeps "Beyonce" close to "Beyoncé" without that.)
pub fn normalize(s: &str) -> String {
    s.to_lowercase().replace('&', "and").chars().filter(|c| c.is_alphanumeric()).collect()
}

/// 0..1, from the longest common subsequence of the normalised strings.
pub fn similarity(a: &str, b: &str) -> f64 {
    let x: Vec<char> = normalize(a).chars().collect();
    let y: Vec<char> = normalize(b).chars().collect();
    if x.is_empty() || y.is_empty() {
        return 0.0;
    }
    if x == y {
        return 1.0;
    }
    let (sx, sy): (String, String) = (x.iter().collect(), y.iter().collect());
    if sx.contains(&sy) || sy.contains(&sx) {
        return 0.85 + 0.15 * x.len().min(y.len()) as f64 / x.len().max(y.len()) as f64;
    }
    let mut dp = vec![0usize; y.len() + 1];
    for xc in &x {
        let mut prev = 0;
        for j in 1..=y.len() {
            let tmp = dp[j];
            dp[j] = if *xc == y[j - 1] { prev + 1 } else { dp[j].max(dp[j - 1]) };
            prev = tmp;
        }
    }
    2.0 * dp[y.len()] as f64 / (x.len() + y.len()) as f64
}

/// How well a result (`artist` may list several, comma-separated) matches the
/// query; -1 when it is clearly another song.
pub fn match_score(q: &Query, title: &str, artist: &str, duration_ms: f64) -> f64 {
    let ts = similarity(&q.title, title);
    let artist_score = similarity(&q.artist, artist).max(similarity(&primary_artist(&q.artist), &primary_artist(artist)));
    if ts < 0.6 || artist_score < 0.45 {
        return -1.0;
    }
    let mut s = ts * 0.6 + artist_score * 0.4;
    if q.duration_ms > 0.0 && duration_ms > 0.0 {
        let diff = (q.duration_ms - duration_ms).abs();
        let ended_early = q.may_end_early && duration_ms > q.duration_ms && diff <= SPOTIFY_EARLY_END_MAX_MS;
        s += match diff {
            d if d <= 2_500.0 => 0.15,
            d if d <= 6_000.0 => 0.0,
            _ if ended_early => 0.0,
            d if d <= 15_000.0 => -0.2,
            _ => return -1.0,
        };
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(id: &str, name: &str, artists: &[&str], dur: f64) -> TrackCandidate {
        TrackCandidate { id: id.into(), name: name.into(), artists: artists.iter().map(|s| s.to_string()).collect(), duration_ms: dur }
    }

    #[test]
    fn cleans_youtube_titles() {
        assert_eq!(clean_title("The Weeknd - Blinding Lights (Official Video)"), "The Weeknd - Blinding Lights");
        assert_eq!(clean_title("Song [HD] | Some Channel"), "Song");
        assert_eq!(clean_artist("Billie Eilish - Topic"), "Billie Eilish");
        assert_eq!(clean_artist("BillieEilishVEVO"), "BillieEilish");
    }

    #[test]
    fn candidates_prefer_the_dashed_title() {
        let c = candidates("The Weeknd - Blinding Lights (feat. Nobody)", "SomeChannel");
        assert_eq!(c[0], ("The Weeknd".to_string(), "Blinding Lights".to_string()));
        assert_eq!(c[1].0, "SomeChannel");
        assert_eq!(c.last().unwrap().0, "");
    }

    #[test]
    fn identifiable_needs_an_artist_or_a_dash() {
        assert!(!is_identifiable("Instagram", ""));
        assert!(is_identifiable("A - B", ""));
        assert!(is_identifiable("Instagram", "someone"));
    }

    #[test]
    fn norm_handles_unicode() {
        assert_eq!(norm("  Beyoncé — Halo!! "), "beyoncé halo");
        assert_eq!(norm("Привет, мир"), "привет мир");
    }

    #[test]
    fn picks_the_matching_track() {
        let results = vec![
            t("1", "Blinding Lights (Remix)", &["Someone"], 250_000.0),
            t("2", "Blinding Lights", &["The Weeknd"], 200_040.0),
        ];
        let cands = candidates("Blinding Lights", "The Weeknd");
        let (best, score) = pick_best(&results, &cands, 200_000.0);
        assert_eq!(best.unwrap().id, "2");
        assert!(score > 1.0);
        let other = [t("3", "Other", &["X"], 0.0)];
        let (none, _) = pick_best(&other, &cands, 0.0);
        assert!(none.is_none());
    }

    fn spotify(title: &str, artist: &str, secs: f64) -> crate::TrackInfo {
        crate::TrackInfo {
            title: title.into(),
            artist: artist.into(),
            duration_ms: secs * 1000.0,
            is_spotify: true,
            source_app: "Spotify.exe".into(),
            ..Default::default()
        }
    }

    #[test]
    fn android_clean() {
        let c = |t: &str, a: &str| clean(t, a);
        let p = |t: &str, a: &str| (t.to_string(), a.to_string());
        assert_eq!(c("Blinding Lights (Official Video)", "The Weeknd - Topic"), p("Blinding Lights", "The Weeknd"));
        assert_eq!(c("Brother Louie '98 - New Version", "Modern Talking"), p("Brother Louie '98", "Modern Talking"));
        assert_eq!(c("Brother Louie Mix '98 (feat. Eric Singleton) - Radio Edit", "Modern Talking"), p("Brother Louie Mix '98", "Modern Talking"));
        // A version after the dash is not "Artist - Title".
        assert_eq!(c("SLAVA FUNK! - Slowed", "MVSTERIOUS"), p("SLAVA FUNK! - Slowed", "MVSTERIOUS"));
        // The left half names the artist, or there is none.
        assert_eq!(c("Daft Punk - One More Time", "Daft Punk"), p("One More Time", "Daft Punk"));
        assert_eq!(c("Daft Punk - One More Time", ""), p("One More Time", "Daft Punk"));
        assert_eq!(c("Song feat. Someone", "A"), p("Song", "A"));
        assert_eq!(primary_artist("Miyagi & Andy Panda"), "Miyagi");
        assert_eq!(primary_artist("Calvin Harris feat. Rihanna"), "Calvin Harris");
        assert_eq!(primary_artist("Simon and Garfunkel"), "Simon");
    }

    #[test]
    fn query_rules() {
        let q = query(&spotify("Phoenix", "League of Legends", 184.0));
        assert_eq!((q.title.as_str(), q.artist.as_str()), ("Phoenix", "League of Legends"));
        assert!(q.may_end_early);
        assert_eq!(q.strict_duration_ms(), None);
        // A browser tab: the dashed title beats the channel name.
        let tab = crate::TrackInfo {
            title: "The Weeknd - Blinding Lights (Official Video)".into(),
            artist: "SomeChannel".into(),
            duration_ms: 200_000.0,
            source_app: "Chrome".into(),
            ..Default::default()
        };
        let q = query(&tab);
        assert_eq!((q.title.as_str(), q.artist.as_str()), ("Blinding Lights", "The Weeknd"));
        assert_eq!(q.strict_duration_ms(), Some(200_000.0));
    }

    #[test]
    fn similarity_is_lcs_based() {
        assert_eq!(similarity("Phoenix", "phoenix!"), 1.0);
        assert!(similarity("Beyonce", "Beyoncé") > 0.8);
        assert!((similarity("Phoenix", "Phoenix (From \"League of Legends\")") - (0.85 + 0.15 * 7.0 / 26.0)).abs() < 1e-9);
        assert_eq!(similarity("", "x"), 0.0);
    }

    /// "League of Legends - Phoenix" (Spotify reports 184 s for the 197.6 s song)
    /// once resolved to Stray Kids' "Phoenix" through a title-only search.
    #[test]
    fn phoenix_is_league_of_legends_not_stray_kids() {
        let q = query(&spotify("Phoenix", "League of Legends", 184.0));
        assert!(match_score(&q, "Phoenix", "Stray Kids", 182_013.0) < 0.0);
        assert!(match_score(&q, "Phoenix", "Marshmello, Portugal. The Man", 144_501.0) < 0.0);
        let right = match_score(&q, "Phoenix", "League of Legends, Cailin Russo, Chrissy Costanza", 197_633.0);
        assert!(right > 0.9, "{right}");
        // The same gap from a player whose duration is the real length: Android's -0.2.
        let strict = Query { may_end_early: false, ..q.clone() };
        assert!((match_score(&strict, "Phoenix", "League of Legends", 197_633.0) - 0.8).abs() < 1e-9);
    }

    /// "Miyagi & Andy Panda - Yamakasi" (Spotify reports 233 s for the 263.6 s song)
    /// once resolved to a jazz "Yamakasi" through a title-only search.
    #[test]
    fn yamakasi_is_miyagi_not_the_getdown() {
        let q = query(&spotify("Yamakasi", "Miyagi & Andy Panda", 233.0));
        assert!(match_score(&q, "Yamakasi", "The Getdown, Laurent Coulondre, Arnaud Dolmen, Grégory Privat", 226_651.0) < 0.0);
        assert!(match_score(&q, "Yamakasi", "Sticky M.A., Lusi", 146_000.0) < 0.0);
        assert!(match_score(&q, "Yamakasi", "Miyagi & Andy Panda", 263_597.0) > 0.9);
        assert!(match_score(&q, "Yamakasi", "Miyagi", 264_000.0) > 0.9);
        // Longer than an early end can explain: another cut.
        assert!(match_score(&q, "Yamakasi", "Miyagi & Andy Panda", 383_000.0) < 0.0);
        // Spotify never reports more than the song: shorter results follow Android's rule.
        assert!(match_score(&q, "Yamakasi", "Miyagi & Andy Panda", 210_000.0) < 0.0);
    }

    #[test]
    fn squashed_names_match() {
        assert!(token_sim("BillieEilish", "Billie Eilish") >= 0.9);
    }
}
