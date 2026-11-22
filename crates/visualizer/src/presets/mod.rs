//! The built-in presets. Each lives in its own file; list a new one in `ALL` to
//! make it selectable (by id in `VizPreset`, and in the random rotation).

mod bars;
mod glow;
mod pulse;
mod wave;

use crate::preset::Preset;

pub use bars::Bars;
pub use glow::Glow;
pub use pulse::Pulse;
pub use wave::Wave;

pub static ALL: &[&dyn Preset] = &[
    &Glow { id: "glow-taskbar", name: "Glow — taskbar only" },
    &Bars { id: "equalizer-taskbar", name: "Equalizer — taskbar only", bleed: false },
    &Bars { id: "equalizer-bleed", name: "Equalizer — bleed", bleed: true },
    &Pulse { id: "pulse-bleed", name: "Pulse — bleed" },
    &Wave { id: "wave-bleed", name: "Wave — bleed" },
];

pub const DEFAULT_ID: &str = "equalizer-bleed";

/// The preset with this id, or the default one.
pub fn get(id: &str) -> &'static dyn Preset {
    let find = |id: &str| ALL.iter().copied().find(|p| p.id() == id);
    find(id).or_else(|| find(DEFAULT_ID)).unwrap_or(ALL[0])
}

/// A preset other than `current`, picked with `seed` (any changing number).
pub fn random_other(current: &str, seed: u64) -> &'static dyn Preset {
    let others: Vec<_> = ALL.iter().filter(|p| p.id() != current).collect();
    if others.is_empty() {
        return ALL[0];
    }
    *others[(seed % others.len() as u64) as usize]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::palette::Palette;
    use crate::preset::{Fade, Fill, Frame, Painter, Rect};

    /// Records what a preset draws and checks that groups balance.
    #[derive(Default)]
    struct Recorder {
        depth: i32,
        shapes: usize,
        bounds: Vec<(f32, f32)>, // (top, bottom) of every shape
        /// The highest visible y of every shape: its top, or the fade end masking it.
        visible_tops: Vec<f32>,
        fades: Vec<f32>,
    }

    impl Recorder {
        fn note(&mut self, top: f32) {
            let mask = self.fades.iter().copied().fold(f32::MIN, f32::max);
            self.visible_tops.push(top.max(mask));
        }
    }

    impl Painter for Recorder {
        fn begin_group(&mut self, fade: Option<Fade>, opacity: f32) {
            assert!((0.0..=1.0).contains(&opacity), "opacity {opacity}");
            if let Some(f) = fade {
                assert!(f.fade_top <= f.solid_top && f.solid_top <= f.bottom, "fade ordered: {f:?}");
            }
            self.fades.push(fade.map(|f| f.fade_top).unwrap_or(f32::MIN));
            self.depth += 1;
        }
        fn end_group(&mut self) {
            self.fades.pop();
            self.depth -= 1;
            assert!(self.depth >= 0);
        }
        fn rect(&mut self, r: Rect, _: Fill) {
            self.shapes += 1;
            self.note(r.y);
            self.bounds.push((r.y, r.y + r.h));
        }
        fn rounded_rect(&mut self, r: Rect, _: f32, f: Fill) {
            self.rect(r, f);
        }
        fn area(&mut self, top: &[(f32, f32)], bottom: f32, _: bool, _: Fill) {
            self.shapes += 1;
            let min = top.iter().map(|p| p.1).fold(f32::MAX, f32::min);
            self.note(min);
            self.bounds.push((min, bottom));
        }
    }

    fn frame<'a>(spectrum: &'a [f32], palette: &'a Palette, level: f32) -> Frame<'a> {
        Frame {
            width: 1920.0,
            height: 360.0,
            taskbar_height: 48.0,
            scale: 1.0,
            spectrum,
            level,
            bass: level,
            palette,
            time_sec: 1.0,
            dt_ms: 16.0,
        }
    }

    #[test]
    fn ids_are_unique_and_lookup_falls_back_to_the_default() {
        for (i, p) in ALL.iter().enumerate() {
            assert!(ALL[i + 1..].iter().all(|q| q.id() != p.id()), "duplicate id {}", p.id());
            assert_eq!(get(p.id()).id(), p.id());
        }
        assert_eq!(get("no-such-preset").id(), DEFAULT_ID);
        for seed in 0..20 {
            assert_ne!(random_other("wave-bleed", seed).id(), "wave-bleed");
        }
    }

    #[test]
    fn every_preset_draws_inside_the_surface_with_balanced_groups() {
        let palette = Palette::default();
        let loud: Vec<f32> = (0..64).map(|i| 1.4 - i as f32 / 64.0).collect();
        for p in ALL {
            let mut r = Recorder::default();
            p.render(&mut r, &frame(&loud, &palette, 1.0));
            assert_eq!(r.depth, 0, "{} balances groups", p.id());
            assert!(r.shapes > 0, "{} draws something when loud", p.id());
            for (top, bottom) in &r.bounds {
                assert!(*top >= -0.5 && *bottom <= 360.5, "{} stays inside: {top}..{bottom}", p.id());
            }
            if p.id().ends_with("-taskbar") {
                assert!(r.visible_tops.iter().all(|top| *top >= 360.0 - 48.0 - 0.5), "{} stays in the strip", p.id());
            }
        }
    }

    #[test]
    fn silent_bars_draw_nothing() {
        let palette = Palette::default();
        let silent = vec![0.0; 64];
        let mut r = Recorder::default();
        get("equalizer-bleed").render(&mut r, &frame(&silent, &palette, 0.0));
        assert_eq!(r.shapes, 0);
    }
}
