//! Classic equalizer bars: either clipped to the taskbar strip, or rising out of
//! it into the lower part of the screen and fading as they climb.

use crate::preset::{Fade, Fill, Frame, Painter, Preset, Rect};

pub struct Bars {
    pub id: &'static str,
    pub name: &'static str,
    /// Rise above the taskbar (true) or stay inside it (false).
    pub bleed: bool,
}

impl Preset for Bars {
    fn id(&self) -> &'static str {
        self.id
    }
    fn name(&self) -> &'static str {
        self.name
    }
    fn render(&self, p: &mut dyn Painter, f: &Frame) {
        let strip = f.taskbar_height;
        let (max_amp, solid_top, fade_top) = if self.bleed {
            let max_amp = f.height * 0.9;
            (max_amp, f.height - strip, f.height - max_amp)
        } else {
            if strip < 6.0 {
                return;
            }
            (strip * 0.92, f.height - strip, f.height - strip)
        };
        draw_bars(p, f, f.height, max_amp, Fade { bottom: f.height, solid_top, fade_top });
    }
}

pub fn draw_bars(p: &mut dyn Painter, f: &Frame, baseline: f32, max_amp: f32, fade: Fade) {
    let n = f.spectrum.len();
    if n == 0 {
        return;
    }
    let slot = f.width / n as f32;
    let gap = (slot * 0.28).min(3.0 * f.scale);
    let bw = (slot - gap).max(1.0);
    let radius = (bw / 2.0).min(3.0 * f.scale);
    p.begin_group(Some(fade), 1.0);
    for i in 0..n {
        let h = f.amp(i) * max_amp;
        if h < 1.0 {
            continue;
        }
        p.rounded_rect(Rect::new(i as f32 * slot + gap / 2.0, baseline - h, bw, h), radius, Fill::Palette);
    }
    p.end_group();
}
