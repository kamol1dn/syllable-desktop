//! Three gradient colours pulled from the album cover (port of `AlbumPalette.cs`).
//! `a` is the dominant colourful hue, `b` and `c` the next ones. All are kept
//! saturated but deliberately deep, so the visualizer stays a backdrop.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rgb(pub u8, pub u8, pub u8);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Palette {
    pub a: Rgb,
    pub b: Rgb,
    pub c: Rgb,
}

impl Default for Palette {
    /// Deep purple, blue, magenta: used before a cover arrives or when it has no colour.
    fn default() -> Self {
        Self { a: Rgb(0x4E, 0x1A, 0x82), b: Rgb(0x14, 0x52, 0x99), c: Rgb(0x8F, 0x22, 0x50) }
    }
}

const HUE_BINS: usize = 18;
/// Sample at most about this many pixels along the longer side.
const GRID: u32 = 48;

impl Palette {
    /// Extract a palette from RGBA pixels (row-major, 4 bytes each). Never fails:
    /// a grey, black or empty image gives the default palette.
    pub fn from_rgba(pixels: &[u8], width: u32, height: u32) -> Self {
        if width == 0 || height == 0 || pixels.len() < (width * height * 4) as usize {
            return Self::default();
        }
        let step = (width.max(height) / GRID).max(1);
        let mut w = [0f64; HUE_BINS];
        let mut r = [0f64; HUE_BINS];
        let mut g = [0f64; HUE_BINS];
        let mut b = [0f64; HUE_BINS];
        for y in (0..height).step_by(step as usize) {
            for x in (0..width).step_by(step as usize) {
                let i = ((y * width + x) * 4) as usize;
                let (pr, pg, pb) = (pixels[i], pixels[i + 1], pixels[i + 2]);
                let (h, s, v) = rgb_to_hsv(pr, pg, pb);
                if v < 0.16 || s < 0.18 {
                    continue; // near-black or grey
                }
                let weight = s * v; // favour colourful and bright
                let bin = ((h / 360.0 * HUE_BINS as f64) as usize).min(HUE_BINS - 1);
                w[bin] += weight;
                r[bin] += pr as f64 * weight;
                g[bin] += pg as f64 * weight;
                b[bin] += pb as f64 * weight;
            }
        }
        let mut ranked: Vec<usize> = (0..HUE_BINS).filter(|&i| w[i] > 0.0).collect();
        ranked.sort_by(|&x, &y| w[y].total_cmp(&w[x]));
        let colours: Vec<Rgb> = ranked
            .iter()
            .take(3)
            .map(|&i| vibrant((r[i] / w[i]) as u8, (g[i] / w[i]) as u8, (b[i] / w[i]) as u8))
            .collect();
        let Some(&a) = colours.first() else {
            return Self::default();
        };
        Self {
            a,
            b: colours.get(1).copied().unwrap_or_else(|| shift(a, 0.5)),
            c: colours.get(2).copied().unwrap_or_else(|| shift(a, -0.5)),
        }
    }
}

/// Keep a colour richly saturated but deep and dim, so it reads as a backdrop.
fn vibrant(r: u8, g: u8, b: u8) -> Rgb {
    let (h, s, v) = rgb_to_hsv(r, g, b);
    let s = (s * 1.2 + 0.1).clamp(0.0, 1.0);
    let v = (v * 0.72).clamp(0.14, 0.62);
    hsv_to_rgb(h, s, v)
}

/// Rotate a colour's hue by `turns` sixths of the wheel (for synthetic stops).
fn shift(c: Rgb, turns: f64) -> Rgb {
    let (h, s, v) = rgb_to_hsv(c.0, c.1, c.2);
    hsv_to_rgb((h + turns * 60.0 + 360.0) % 360.0, s, v)
}

pub fn rgb_to_hsv(r: u8, g: u8, b: u8) -> (f64, f64, f64) {
    let (rd, gd, bd) = (r as f64 / 255.0, g as f64 / 255.0, b as f64 / 255.0);
    let max = rd.max(gd).max(bd);
    let min = rd.min(gd).min(bd);
    let d = max - min;
    let s = if max <= 0.0 { 0.0 } else { d / max };
    if d <= 0.0 {
        return (0.0, s, max);
    }
    let mut h = if max == rd {
        60.0 * (((gd - bd) / d) % 6.0)
    } else if max == gd {
        60.0 * ((bd - rd) / d + 2.0)
    } else {
        60.0 * ((rd - gd) / d + 4.0)
    };
    if h < 0.0 {
        h += 360.0;
    }
    (h, s, max)
}

pub fn hsv_to_rgb(h: f64, s: f64, v: f64) -> Rgb {
    let c = v * s;
    let x = c * (1.0 - ((h / 60.0) % 2.0 - 1.0).abs());
    let m = v - c;
    let (r, g, b) = match h {
        h if h < 60.0 => (c, x, 0.0),
        h if h < 120.0 => (x, c, 0.0),
        h if h < 180.0 => (0.0, c, x),
        h if h < 240.0 => (0.0, x, c),
        h if h < 300.0 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    let to = |v: f64| ((v + m) * 255.0).clamp(0.0, 255.0) as u8;
    Rgb(to(r), to(g), to(b))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn image(w: u32, h: u32, colour_at: impl Fn(u32, u32) -> (u8, u8, u8)) -> Vec<u8> {
        let mut px = Vec::with_capacity((w * h * 4) as usize);
        for y in 0..h {
            for x in 0..w {
                let (r, g, b) = colour_at(x, y);
                px.extend_from_slice(&[r, g, b, 255]);
            }
        }
        px
    }

    fn hue(c: Rgb) -> f64 {
        rgb_to_hsv(c.0, c.1, c.2).0
    }

    #[test]
    fn hsv_round_trips() {
        for &(r, g, b) in &[(255, 0, 0), (12, 200, 90), (40, 40, 200), (250, 250, 10), (128, 0, 128)] {
            let (h, s, v) = rgb_to_hsv(r, g, b);
            let Rgb(r2, g2, b2) = hsv_to_rgb(h, s, v);
            assert!(r.abs_diff(r2) <= 1 && g.abs_diff(g2) <= 1 && b.abs_diff(b2) <= 1, "{r},{g},{b}");
        }
    }

    #[test]
    fn grey_black_and_empty_images_give_the_default() {
        assert_eq!(Palette::from_rgba(&image(64, 64, |_, _| (128, 128, 128)), 64, 64), Palette::default());
        assert_eq!(Palette::from_rgba(&image(64, 64, |_, _| (5, 0, 10)), 64, 64), Palette::default());
        assert_eq!(Palette::from_rgba(&[], 0, 0), Palette::default());
        assert_eq!(Palette::from_rgba(&[1, 2, 3], 10, 10), Palette::default(), "short buffer");
    }

    #[test]
    fn dominant_hue_comes_first_and_colours_are_deep() {
        // Two thirds red, one third blue, a grey stripe that must be ignored.
        let px = image(300, 300, |x, _| match x {
            0..=199 => (220, 30, 30),
            200..=279 => (30, 60, 220),
            _ => (128, 128, 128),
        });
        let p = Palette::from_rgba(&px, 300, 300);
        assert!(hue(p.a) < 15.0 || hue(p.a) > 345.0, "a is red: {:?}", p.a);
        assert!((200.0..250.0).contains(&hue(p.b)), "b is blue: {:?}", p.b);
        for c in [p.a, p.b, p.c] {
            let (_, s, v) = rgb_to_hsv(c.0, c.1, c.2);
            assert!(v <= 0.63 && v >= 0.13, "deep: {c:?}");
            assert!(s > 0.5, "saturated: {c:?}");
        }
    }

    #[test]
    fn a_single_hue_gets_rotated_companions() {
        let p = Palette::from_rgba(&image(50, 50, |_, _| (20, 180, 60)), 50, 50);
        let (ha, hb, hc) = (hue(p.a), hue(p.b), hue(p.c));
        assert!((ha - 135.0).abs() < 15.0, "green: {ha}");
        assert!(((hb - ha + 360.0) % 360.0 - 30.0).abs() < 3.0, "b is +30 degrees: {hb}");
        assert!(((ha - hc + 360.0) % 360.0 - 30.0).abs() < 3.0, "c is -30 degrees: {hc}");
    }
}
