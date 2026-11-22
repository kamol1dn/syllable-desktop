//! A bass-pumping gradient slab that swells out of the taskbar on every kick,
//! with a bright core that flares on the beat.

use crate::palette::Rgb;
use crate::preset::{Fade, Fill, Frame, Painter, Preset, Rect, Rgba};

pub struct Pulse {
    pub id: &'static str,
    pub name: &'static str,
}

impl Preset for Pulse {
    fn id(&self) -> &'static str {
        self.id
    }
    fn name(&self) -> &'static str {
        self.name
    }
    fn render(&self, p: &mut dyn Painter, f: &Frame) {
        let bottom = f.height;
        let strip = f.taskbar_height;
        let energy = (0.12 + f.bass * 0.95 + f.level * 0.25).clamp(0.0, 1.0);
        let h = strip + (f.height - strip) * energy;
        let top = bottom - h;

        p.begin_group(Some(Fade { bottom, solid_top: bottom - strip * 0.5, fade_top: top }), 0.45 + 0.55 * energy);
        p.rect(Rect::new(0.0, top, f.width, h), Fill::Palette);
        p.end_group();

        let core_h = strip * (0.5 + f.bass * 0.9);
        let color = Rgba { rgb: Rgb(255, 240, 245), alpha: 110.0 / 255.0 * energy };
        p.rect(
            Rect::new(0.0, bottom - core_h, f.width, core_h),
            Fill::Radial { cx: f.width / 2.0, cy: bottom, rx: f.width * 0.75, ry: core_h, color },
        );
    }
}
