//! The prototype's `Matching.cs` rules, checked against `syllable_core::matching`.
//! Kept apart from the module's own tests (owned by the lyrics port) on purpose.

use syllable_core::matching::*;

fn c(a: &str, t: &str) -> (String, String) {
    (a.to_string(), t.to_string())
}

#[test]
fn clean_title_drops_upload_junk_only() {
    assert_eq!(clean_title("Blinding Lights (Official Video)"), "Blinding Lights");
    assert_eq!(clean_title("Song [HD] (Lyrics)"), "Song");
    assert_eq!(clean_title("Song (Official Music Video) | Vevo"), "Song");
    assert_eq!(clean_title("Song // extra"), "Song");
    assert_eq!(clean_title("Song (Remastered 2011)"), "Song");
    assert_eq!(clean_title("Song (Live at Wembley)"), "Song");
    assert_eq!(clean_title("Song (M/V)"), "Song");
    assert_eq!(clean_title("  - Song -  "), "Song");
    assert_eq!(
        clean_title("Rick Astley - Never Gonna Give You Up (Official Video) (4K Remaster)"),
        "Rick Astley - Never Gonna Give You Up"
    );
    // Meaningful brackets stay; word boundaries are respected.
    assert_eq!(clean_title("Song (Acoustic)"), "Song (Acoustic)");
    assert_eq!(clean_title("Song (feat. X)"), "Song (feat. X)");
    assert_eq!(clean_title("Song (Videos)"), "Song (Videos)");
}

#[test]
fn clean_artist_drops_channel_decorations() {
    assert_eq!(clean_artist("Daft Punk - Topic"), "Daft Punk");
    assert_eq!(clean_artist("Daft Punk - TOPIC"), "Daft Punk");
    assert_eq!(clean_artist("BillieEilishVEVO"), "BillieEilish");
    assert_eq!(clean_artist("  Adele  "), "Adele");
    assert_eq!(clean_artist(""), "");
}

#[test]
fn identifiable_needs_an_artist_or_a_spaced_dash() {
    assert!(is_identifiable("Instagram", "Some Channel"));
    assert!(is_identifiable("Daft Punk - One More Time (Official Video)", ""));
    assert!(!is_identifiable("Instagram", ""));
    assert!(!is_identifiable("Instagram", "VEVO"));
    assert!(!is_identifiable("Jay-Z", ""));
}

#[test]
fn candidates_order_features_and_dedupe() {
    assert_eq!(
        candidates("Daft Punk - One More Time (Official Video)", "DaftPunkVEVO"),
        vec![c("Daft Punk", "One More Time"), c("DaftPunk", "Daft Punk - One More Time"), c("", "Daft Punk - One More Time")]
    );
    assert_eq!(
        candidates("Stay (feat. Justin Bieber)", "The Kid LAROI"),
        vec![c("The Kid LAROI", "Stay"), c("", "Stay (feat. Justin Bieber)")]
    );
    assert_eq!(candidates("Artist - Song ft. Someone Else", "")[0], c("Artist", "Song"));
    assert_eq!(candidates("Song [with Friend]", "Artist")[0], c("Artist", "Song"));
    assert_eq!(candidates("Artist – Song", "")[0], c("Artist", "Song"));
    assert_eq!(candidates("Artist — Song", "")[0], c("Artist", "Song"));
    assert!(candidates("(Official Video)", "").is_empty());
}

#[test]
fn norm_lowercases_and_strips_punctuation() {
    assert_eq!(norm("  Don't   Stop—Me Now! "), "don t stop me now");
    assert_eq!(norm("Пачка Сигарет"), "пачка сигарет");
}

#[test]
fn score_weights_title_artist_and_duration() {
    let t = TrackCandidate {
        id: "x".into(),
        name: "Blinding Lights".into(),
        artists: vec!["The Weeknd".into()],
        duration_ms: 200_040.0,
    };
    let cand = c("The Weeknd", "Blinding Lights");
    let close = |a: f64, b: f64| (a - b).abs() < 1e-9;
    assert!(close(score(&t, &cand, 0.0), 1.0));
    assert!(close(score(&t, &cand, 201_000.0), 1.35));
    assert!(close(score(&t, &cand, 206_000.0), 1.2));
    assert!(close(score(&t, &cand, 220_000.0), 1.0));
    assert!(close(score(&t, &cand, 260_000.0), 0.85));
    let remix = TrackCandidate { name: "Blinding Lights (Remix)".into(), duration_ms: 0.0, ..t.clone() };
    // Token overlap is 2/3, but the squashed strings contain each other, which wins (0.9).
    assert!(close(score(&remix, &c("", "Blinding Lights"), 0.0), 0.9 * 0.6));
    // Space-less channel names count as the same artist (squashed match = 0.9).
    let billie = TrackCandidate { name: "bad guy".into(), artists: vec!["Billie Eilish".into()], ..t };
    let s = score(&billie, &c("BillieEilish", "bad guy"), 0.0);
    assert!(close(s, 0.6 + 0.9 * 0.4), "billie score {s}");
}

#[test]
fn pick_best_needs_the_threshold() {
    let other = TrackCandidate { id: "1".into(), name: "Unrelated".into(), artists: vec!["Someone".into()], duration_ms: 0.0 };
    let right = TrackCandidate { id: "2".into(), name: "One More Time".into(), artists: vec!["Daft Punk".into()], duration_ms: 320_000.0 };
    let cands = candidates("Daft Punk - One More Time (Official Video)", "");
    let results = [other.clone(), right];
    let (best, s) = pick_best(&results, &cands, 321_000.0);
    assert_eq!(best.map(|t| t.id.as_str()), Some("2"));
    assert!(s > 1.3);
    assert!(pick_best(&[other], &cands, 0.0).0.is_none());
}
