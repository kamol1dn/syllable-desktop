//! A flowing spectrum band that ripples up above the taskbar.

use crate::preset::{spectrum_points, Fade, Fill, Frame, Painter, Preset};

pub struct Wave {
    pub id: &'static str,
    pub name: &'static str,
}

impl Preset for Wave {
    fn id(&self) -> &'static str {
        self.id
    }
    fn name(&self) -> &'static str {
        self.name
    }
    fn render(&self, p: &mut dyn Painter, f: &Frame) {
        let max_amp = f.height * 0.75;
        let baseline = f.height - f.taskbar_height * 0.4;
        let top = spectrum_points(f, baseline, max_amp);
        p.begin_group(
            Some(Fade { bottom: f.height, solid_top: f.height - f.taskbar_height, fade_top: f.height - max_amp }),
            0.85,
        );
        p.area(&top, f.height, true, Fill::Palette);
        p.end_group();
    }
}
