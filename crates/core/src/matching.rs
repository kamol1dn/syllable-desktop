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

    #[test]
    fn squashed_names_match() {
        assert!(token_sim("BillieEilish", "Billie Eilish") >= 0.9);
    }
}
