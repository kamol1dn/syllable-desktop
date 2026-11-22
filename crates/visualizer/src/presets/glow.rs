//! A soft gradient glow that stays inside the taskbar strip.

use crate::preset::{spectrum_points, Fade, Fill, Frame, Painter, Preset};

pub struct Glow {
    pub id: &'static str,
    pub name: &'static str,
}

impl Preset for Glow {
    fn id(&self) -> &'static str {
        self.id
    }
    fn name(&self) -> &'static str {
        self.name
    }
    fn render(&self, p: &mut dyn Painter, f: &Frame) {
        let strip = f.taskbar_height;
        if strip < 6.0 {
            return;
        }
        let bottom = f.height;
        let level = f.level.clamp(0.0, 1.0);
        let baseline = bottom - strip * 0.12;
        let max_amp = strip * (0.55 + 0.45 * level);
        let top = spectrum_points(f, baseline, max_amp);
        p.begin_group(Some(Fade { bottom, solid_top: bottom - strip * 0.15, fade_top: bottom - strip }), 0.5 + 0.5 * level);
        p.area(&top, bottom, true, Fill::Palette);
        p.end_group();
    }
}
