//! What the lyric strip should show at a given song time. Pure functions of the
//! lyrics and the time, so they are tested without a window.

use syllable_core::{BgVocal, LyricLine, Lyrics, Seg};

/// How long after the last line ends the strip keeps showing it.
pub const OUTRO_HOLD_MS: f64 = 4000.0;
/// A background vocal is shown this long either side of its own timing.
pub const BG_MARGIN_MS: f64 = 400.0;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Show {
    /// Nothing: before the song (and no long intro), or well after the last line.
    Nothing,
    /// Line `idx` is current (it may have ended, with too short a gap for dots).
    Line(usize),
    /// An instrumental gap from `start` to `end`: the interlude dots.
    Dots { start: f64, end: f64 },
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Frame {
    pub show: Show,
    /// Index into `Lyrics::bg` of the background vocal to show, if any.
    pub bg: Option<usize>,
}

/// Index of the last line that has started at `t`, if any.
pub fn current_line(lines: &[LyricLine], t: f64) -> Option<usize> {
    // Lines are sorted by start; partition_point finds the first line after `t`.
    let n = lines.partition_point(|l| l.start <= t);
    n.checked_sub(1)
}

pub fn frame(lyrics: &Lyrics, t: f64, interlude_gap_ms: f64) -> Frame {
    let lines = &lyrics.lines;
    let nothing = Frame { show: Show::Nothing, bg: None };
    if lines.is_empty() {
        return nothing;
    }
    let show = match current_line(lines, t) {
        None => {
            // Before the first line: dots for a long intro, otherwise nothing yet.
            let first = lines[0].start;
            if t < -500.0 || first < interlude_gap_ms {
                Show::Nothing
            } else {
                Show::Dots { start: 0.0, end: first }
            }
        }
        Some(i) if t > lines[i].end => match lines.get(i + 1) {
            Some(next) if next.start - lines[i].end >= interlude_gap_ms => {
                Show::Dots { start: lines[i].end, end: next.start }
            }
            Some(_) => Show::Line(i),
            None if t > lines[i].end + OUTRO_HOLD_MS => Show::Nothing,
            None => Show::Line(i),
        },
        Some(i) => Show::Line(i),
    };
    if show == Show::Nothing {
        return nothing;
    }
    Frame { show, bg: active_bg(&lyrics.bg, t) }
}

/// The background vocal around `t`, if any. `bg` is sorted by start.
pub fn active_bg(bg: &[BgVocal], t: f64) -> Option<usize> {
    for (i, b) in bg.iter().enumerate() {
        if b.start - BG_MARGIN_MS > t {
            break;
        }
        if t <= b.end + BG_MARGIN_MS {
            return Some(i);
        }
    }
    None
}

/// The display text of a line: the segments joined when there are any (so the
/// segment offsets index into exactly this text), else the line text.
pub fn display_text(text: &str, sylls: Option<&[Seg]>) -> String {
    match sylls {
        Some(s) if !s.is_empty() => s.iter().map(|s| s.text.as_str()).collect::<String>().trim_end().to_string(),
        _ => text.trim().to_string(),
    }
}

/// A segment's horizontal span in the laid-out line.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SegSpan {
    pub start: f64,
    pub end: f64,
    pub x0: f32,
    pub x1: f32,
}

/// UTF-16 offsets of each segment's end in `display_text`, for hit-testing the layout.
pub fn seg_end_offsets(sylls: &[Seg]) -> Vec<u32> {
    let mut at = 0u32;
    sylls
        .iter()
        .map(|s| {
            at += s.text.encode_utf16().count() as u32;
            at
        })
        .collect()
}

/// How far the sung colour has reached at `t`, in layout pixels.
/// Without segments (line timing) the whole line lights when it starts.
pub fn sweep_x(spans: Option<&[SegSpan]>, line_start: f64, full_w: f32, t: f64) -> f32 {
    let Some(spans) = spans else {
        return if t >= line_start { full_w } else { 0.0 };
    };
    let mut x = 0.0;
    for s in spans {
        if t >= s.end {
            x = s.x1;
        } else if t >= s.start {
            let p = ((t - s.start) / (s.end - s.start).max(1.0)) as f32;
            // Ease the sweep through each syllable a little: it reads as sung rather
            // than wiped, and never lags the syllable's own timing at the ends.
            return s.x0 + (s.x1 - s.x0) * ease_in_out_sine(p);
        } else {
            break;
        }
    }
    x
}

/// Which segment is being sung at `t` (for the active-syllable lift), if any.
pub fn active_seg(spans: &[SegSpan], t: f64) -> Option<usize> {
    spans.iter().position(|s| t >= s.start && t < s.end)
}

/// Interlude progress 0..1 through a gap.
pub fn gap_progress(start: f64, end: f64, t: f64) -> f32 {
    ((t - start) / (end - start).max(1.0)).clamp(0.0, 1.0) as f32
}

// ---- easing ----

pub fn ease_out_cubic(p: f32) -> f32 {
    let p = p.clamp(0.0, 1.0);
    1.0 - (1.0 - p).powi(3)
}

pub fn ease_in_out_sine(p: f32) -> f32 {
    let p = p.clamp(0.0, 1.0);
    0.5 - 0.5 * (std::f32::consts::PI * p).cos()
}

pub fn smoothstep(p: f32) -> f32 {
    let p = p.clamp(0.0, 1.0);
    p * p * (3.0 - 2.0 * p)
}

/// Move `value` toward `target` at `rate` per ms (a linear approach, eased when drawn).
pub fn approach(value: f32, target: f32, dt_ms: f32, duration_ms: f32) -> f32 {
    let step = dt_ms / duration_ms.max(1.0);
    if target > value {
        (value + step).min(target)
    } else {
        (value - step).max(target)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use syllable_core::SyncType;

    fn line(start: f64, end: f64, text: &str) -> LyricLine {
        LyricLine { start, end, text: text.into(), sylls: None, opposite: false }
    }

    fn lyrics(lines: Vec<LyricLine>, bg: Vec<BgVocal>) -> Lyrics {
        Lyrics { sync: SyncType::Line, start_time: 0.0, lines, bg, source: "test".into() }
    }

    #[test]
    fn picks_the_current_line_and_holds_it_through_short_gaps() {
        let l = lyrics(vec![line(1000.0, 2000.0, "a"), line(2500.0, 3000.0, "b")], vec![]);
        assert_eq!(frame(&l, 500.0, 2500.0).show, Show::Nothing);
        assert_eq!(frame(&l, 1500.0, 2500.0).show, Show::Line(0));
        // 500 ms gap: shorter than the interlude threshold, keep the line.
        assert_eq!(frame(&l, 2200.0, 2500.0).show, Show::Line(0));
        assert_eq!(frame(&l, 2600.0, 2500.0).show, Show::Line(1));
        assert_eq!(frame(&l, 3000.0 + OUTRO_HOLD_MS - 1.0, 2500.0).show, Show::Line(1));
        assert_eq!(frame(&l, 3000.0 + OUTRO_HOLD_MS + 1.0, 2500.0).show, Show::Nothing);
    }

    #[test]
    fn dots_for_long_intros_and_interludes() {
        let l = lyrics(vec![line(8000.0, 9000.0, "a"), line(15000.0, 16000.0, "b")], vec![]);
        assert_eq!(frame(&l, 100.0, 2500.0).show, Show::Dots { start: 0.0, end: 8000.0 });
        assert_eq!(frame(&l, 10000.0, 2500.0).show, Show::Dots { start: 9000.0, end: 15000.0 });
        // A longer threshold turns the interlude back into a held line.
        assert_eq!(frame(&l, 10000.0, 7000.0).show, Show::Line(0));
    }

    #[test]
    fn background_vocals_have_a_margin() {
        let bg = vec![BgVocal { start: 1000.0, end: 1500.0, text: "yeah".into(), sylls: None, opposite: false }];
        assert_eq!(active_bg(&bg, 500.0), None);
        assert_eq!(active_bg(&bg, 700.0), Some(0));
        assert_eq!(active_bg(&bg, 1800.0), Some(0));
        assert_eq!(active_bg(&bg, 2000.0), None);
    }

    #[test]
    fn sweep_moves_through_segments() {
        let spans = [
            SegSpan { start: 0.0, end: 100.0, x0: 0.0, x1: 50.0 },
            SegSpan { start: 200.0, end: 300.0, x0: 50.0, x1: 120.0 },
        ];
        assert_eq!(sweep_x(Some(&spans), 0.0, 120.0, -1.0), 0.0);
        assert_eq!(sweep_x(Some(&spans), 0.0, 120.0, 50.0), 25.0); // eased midpoint is the midpoint
        assert_eq!(sweep_x(Some(&spans), 0.0, 120.0, 150.0), 50.0); // between segments: hold
        assert_eq!(sweep_x(Some(&spans), 0.0, 120.0, 400.0), 120.0);
        assert_eq!(sweep_x(None, 10.0, 120.0, 5.0), 0.0);
        assert_eq!(sweep_x(None, 10.0, 120.0, 10.0), 120.0);
        assert_eq!(active_seg(&spans, 250.0), Some(1));
        assert_eq!(active_seg(&spans, 150.0), None);
    }

    #[test]
    fn segment_text_and_offsets() {
        let segs = vec![
            Seg { start: 0.0, end: 1.0, text: "Hel".into(), part_of_word: true },
            Seg { start: 1.0, end: 2.0, text: "lo ".into(), part_of_word: false },
            Seg { start: 2.0, end: 3.0, text: "wörld ".into(), part_of_word: false },
        ];
        assert_eq!(display_text("ignored", Some(&segs)), "Hello wörld");
        assert_eq!(seg_end_offsets(&segs), vec![3, 6, 12]);
        assert_eq!(display_text("  plain ", None), "plain");
    }

    #[test]
    fn easing_ends_are_exact() {
        for f in [ease_out_cubic, ease_in_out_sine, smoothstep] {
            assert_eq!(f(0.0), 0.0);
            assert!((f(1.0) - 1.0).abs() < 1e-6);
        }
        assert_eq!(approach(0.0, 1.0, 50.0, 100.0), 0.5);
        assert_eq!(approach(0.9, 1.0, 50.0, 100.0), 1.0);
        assert_eq!(approach(0.3, 0.0, 50.0, 100.0), 0.0);
    }
}
