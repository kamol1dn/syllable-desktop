//! The preset contract. A preset is a small, safe piece of code that paints one
//! frame through a [`Painter`]; it never touches Direct2D or Win32. To add one,
//! write a type implementing [`Preset`] (see `presets/`) and list it in
//! `presets::ALL`. The painter is a trait so presets can be tested without a GPU.

use crate::palette::{Palette, Rgb};

/// Everything a preset needs to paint one frame, in physical pixels.
///
/// The surface is a strip along the taskbar edge. Presets always draw as if the
/// taskbar were at the bottom: y = 0 is the edge farthest from the taskbar and
/// y = `height` is the screen edge. When the visualizer sits on the top edge the
/// window flips the surface, with no change to the preset.
pub struct Frame<'a> {
    pub width: f32,
    pub height: f32,
    /// The bottom `taskbar_height` pixels lie behind the taskbar; everything above
    /// "bleeds" over the desktop (and is hidden by any app window in front).
    pub taskbar_height: f32,
    /// Pixels per 96-dpi unit: multiply design sizes (gaps, radii) by this.
    pub scale: f32,
    /// Spectrum, bass first, each about 0..1.4.
    pub spectrum: &'a [f32],
    pub level: f32,
    pub bass: f32,
    pub palette: &'a Palette,
    /// Seconds since the visualizer started, for presets that animate on their own.
    pub time_sec: f64,
    pub dt_ms: f64,
}

impl Frame<'_> {
    /// Band `i` clamped to 0..1.
    pub fn amp(&self, i: usize) -> f32 {
        self.spectrum.get(i).copied().unwrap_or(0.0).clamp(0.0, 1.0)
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Rect {
    pub fn new(x: f32, y: f32, w: f32, h: f32) -> Self {
        Self { x, y, w, h }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rgba {
    pub rgb: Rgb,
    /// 0..1
    pub alpha: f32,
}

/// How a shape is filled.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Fill {
    /// The album gradient running left to right across the whole surface (a → b → c).
    Palette,
    Solid(Rgba),
    /// An elliptical glow centred at (`cx`, `cy`) fading from `color` to transparent
    /// at radii `rx`, `ry`.
    Radial { cx: f32, cy: f32, rx: f32, ry: f32, color: Rgba },
}

/// A vertical opacity mask: fully opaque from `bottom` up to `solid_top`, then
/// fading to transparent at `fade_top`. Bleed presets dissolve into the desktop with it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Fade {
    pub bottom: f32,
    pub solid_top: f32,
    pub fade_top: f32,
}

/// Drawing operations a preset may use. Groups nest: everything drawn between
/// `begin_group` and `end_group` is composited together through the fade and opacity.
pub trait Painter {
    fn begin_group(&mut self, fade: Option<Fade>, opacity: f32);
    fn end_group(&mut self);
    fn rect(&mut self, r: Rect, fill: Fill);
    fn rounded_rect(&mut self, r: Rect, radius: f32, fill: Fill);
    /// Fill the shape under a curve: `top` points from left to right, closed down to
    /// `bottom` at both ends. With `smooth`, the curve is drawn through midpoints.
    fn area(&mut self, top: &[(f32, f32)], bottom: f32, smooth: bool, fill: Fill);
}

pub trait Preset: Sync {
    /// Stable id stored in the config (`VizPreset`).
    fn id(&self) -> &'static str;
    /// Label for menus.
    fn name(&self) -> &'static str;
    fn render(&self, p: &mut dyn Painter, f: &Frame);
}

/// Points along the top of the spectrum: band i at x = width·i/(n-1),
/// y = baseline − amp·max_amp.
pub fn spectrum_points(f: &Frame, baseline: f32, max_amp: f32) -> Vec<(f32, f32)> {
    let n = f.spectrum.len().max(2);
    (0..f.spectrum.len()).map(|i| (f.width * i as f32 / (n - 1) as f32, baseline - f.amp(i) * max_amp)).collect()
}
