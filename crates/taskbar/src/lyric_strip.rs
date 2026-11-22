//! The lyrics strip: the current line with a soft-edged karaoke sweep, background
//! vocals as a smaller second row, interlude dots, and lines that rise into place.
//!
//! Motion:
//! - The sweep is a gradient edge about one em wide that moves linearly through each
//!   syllable (as the prototype does), so runs of syllables flow without stalling.
//! - A new line rises in from below and fades up (ease-out cubic, 320 ms) while the
//!   outgoing one keeps rising and fades away, so the strip reads like a scroll.
//! - A background vocal lifts the main line up and slides in under it.
//! - Interlude dots fill one by one, breathe gently, and shrink away just before the
//!   next line, so the line's entrance lands on the beat.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::Arc;
use std::time::{Duration, Instant};

use syllable_core::{log, Config, Lyrics};
use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Direct2D::Common::*;
use windows::Win32::Graphics::Direct2D::*;
use windows::Win32::Graphics::DirectWrite::*;

use crate::dock::Dock;
use crate::gfx::{self, Canvas, Factories, TextLayout};
use crate::layout::{self, Placement, Screen, StripSpec};
use crate::timeline::{self, SegSpan, Show};

const LINE_IN: f32 = 320.0;
const BG_IN: f32 = 240.0;
const FADE: f32 = 180.0;
const DOTS_EXIT_MS: f64 = 380.0;

pub fn spec(cfg: &Config) -> StripSpec {
    StripSpec {
        placement: cfg.placement.clone(),
        v_pos: cfg.v_pos.clone(),
        align: cfg.align.clone(),
        x_offset: cfg.x_offset,
        width: cfg.width,
        preferred_height: 48.0,
    }
}

/// What occupies the main row.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Key {
    Empty,
    Line(usize),
    Dots { start: f64, end: f64 },
}

struct LineVis {
    layout: TextLayout,
    spans: Option<Vec<SegSpan>>,
    start: f64,
    end: f64,
    /// The line rasterised once as an alpha mask; each frame only fills it with the
    /// sweep gradient (much cheaper than laying glyphs out again every frame).
    mask: std::cell::RefCell<Option<ID2D1Bitmap>>,
}

/// Margin around a line's mask, in pixels.
const MASK_PAD: f32 = 2.0;

impl LineVis {
    fn mask(&self, rt: &ID2D1RenderTarget, white: &ID2D1SolidColorBrush) -> Option<ID2D1Bitmap> {
        if let Some(m) = &*self.mask.borrow() {
            return Some(m.clone());
        }
        let size = D2D_SIZE_F {
            width: (self.layout.width + 2.0 * MASK_PAD).ceil(),
            height: (self.layout.height + 2.0 * MASK_PAD).ceil(),
        };
        let m = unsafe {
            let brt = rt
                .CreateCompatibleRenderTarget(Some(&size), None, None, D2D1_COMPATIBLE_RENDER_TARGET_OPTIONS_NONE)
                .ok()?;
            brt.SetTextAntialiasMode(D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE);
            brt.BeginDraw();
            brt.Clear(Some(&gfx::black(0.0)));
            white.SetColor(&gfx::white(1.0));
            brt.DrawTextLayout(gfx::v2(MASK_PAD, MASK_PAD), &self.layout.layout, white, D2D1_DRAW_TEXT_OPTIONS_NONE);
            brt.EndDraw(None, None).ok()?;
            brt.GetBitmap().ok()?
        };
        *self.mask.borrow_mut() = Some(m.clone());
        Some(m)
    }
}

struct Row {
    key: Key,
    line: Option<LineVis>,
    since: Instant,
    /// How far (0..1, eased) the row was lifted for a background vocal when it was
    /// retired: an outgoing line keeps that height instead of snapping back to centre.
    lift: f32,
}

impl Row {
    fn new(key: Key, now: Instant) -> Self {
        Row { key, line: None, since: now, lift: 0.0 }
    }
}

struct Res {
    main: ID2D1LinearGradientBrush,
    bg: ID2D1LinearGradientBrush,
    solid: ID2D1SolidColorBrush,
}

pub struct LyricStrip {
    dock: Dock,
    canvas: Option<Canvas>,
    res: Option<Res>,
    main_fmt: Option<IDWriteTextFormat>,
    bg_fmt: Option<IDWriteTextFormat>,
    fmt_scale: f64,
    lyrics: Option<Arc<Lyrics>>,
    cur: Row,
    prev: Option<Row>,
    bg: Option<(usize, LineVis)>,
    bg_anim: f32,
    /// 0..1 strip opacity, and where it is heading.
    opacity: f32,
    wanted: bool,
    last_sig: u64,
    last_heads_q: (i32, i32),
    /// Where the last frame put each row (window x of the text, sweep head): what a
    /// sweep-only frame has to repaint.
    drawn_heads: (Option<(f32, f32)>, Option<(f32, f32)>),
    force: bool,
    last_frame: Instant,
    last_dump: Instant,
    dumps: u32,
    covered: bool,
    size: (i32, i32),
}

impl LyricStrip {
    pub fn new() -> Self {
        let now = Instant::now();
        Self {
            dock: Dock::new("lyrics strip"),
            canvas: None,
            res: None,
            main_fmt: None,
            bg_fmt: None,
            fmt_scale: 0.0,
            lyrics: None,
            cur: Row::new(Key::Empty, now),
            prev: None,
            bg: None,
            bg_anim: 0.0,
            opacity: 0.0,
            wanted: false,
            last_sig: 0,
            last_heads_q: (0, 0),
            drawn_heads: (None, None),
            force: true,
            last_frame: now,
            last_dump: now,
            dumps: 0,
            covered: false,
            size: (0, 0),
        }
    }

    pub fn is_docked(&self) -> bool {
        self.dock.is_docked()
    }

    pub fn dock_refused(&self) -> bool {
        (self.wanted || self.opacity > 0.0) && self.dock.refused()
    }

    pub fn set_lyrics(&mut self, lyrics: Option<Arc<Lyrics>>) {
        let lyrics = lyrics.filter(|l| !l.is_empty());
        // The same lyrics again (a re-resolve, another provider with identical timing):
        // keep the line on screen instead of replaying its entrance.
        let same = match (&self.lyrics, &lyrics) {
            (Some(a), Some(b)) => Arc::ptr_eq(a, b) || (a.lines == b.lines && a.bg == b.bg),
            (None, None) => true,
            _ => false,
        };
        if same {
            self.lyrics = lyrics;
            return;
        }
        self.lyrics = lyrics;
        // Layouts index into the old lyrics: drop them, and let the old line leave.
        self.retire_current();
        self.bg = None;
        self.bg_anim = 0.0;
        self.force = true;
    }

    /// Fonts, colours or layout changed: rebuild what depends on them.
    pub fn config_changed(&mut self) {
        self.main_fmt = None;
        self.bg_fmt = None;
        self.res = None;
        self.cur.line = None;
        self.prev = None;
        self.bg = None;
        self.force = true;
    }

    /// Returns whether it changed.
    pub fn set_wanted(&mut self, on: bool) -> bool {
        let changed = on != self.wanted;
        if changed {
            self.wanted = on;
            self.force = true;
        }
        changed
    }

    fn retire_current(&mut self) {
        let now = Instant::now();
        let lift = timeline::smoothstep(self.bg_anim);
        let mut old = std::mem::replace(&mut self.cur, Row::new(Key::Empty, now));
        // An empty row has nothing to animate out: keep whatever is still leaving
        // (after a track change the old line is retired first, then the empty row).
        if old.key == Key::Empty {
            return;
        }
        old.since = now;
        old.lift = lift;
        self.prev = Some(old);
    }

    /// Whether frames will keep changing without an event (drives the frame clock).
    /// Something changed that has not been drawn yet.
    pub fn redraw_pending(&self) -> bool {
        self.force && !self.covered && self.dock.has_target() && (self.wanted || self.opacity > 0.0)
    }

    /// Re-check whether a full-screen app hides the (docked) strip; while it does,
    /// frames are not drawn at all.
    pub fn check_covered(&mut self) {
        // (The debug frame dump wants frames even while nothing is visible.)
        let covered = self.dock.covered() && crate::diag::dump_dir().is_none();
        if covered != self.covered {
            log!("taskbar: lyrics strip {}", if covered { "covered by a full-screen app, pausing frames" } else { "uncovered" });
            self.covered = covered;
            self.force = true;
        }
    }

    pub fn animating(&self, playing: bool) -> bool {
        // Nothing to draw into (hidden behind a full-screen app, waiting for a
        // taskbar): the dock's poll brings it back, no frames meanwhile.
        if self.covered || !self.dock.has_target() {
            return false;
        }
        let fading = (self.opacity - if self.wanted { 1.0 } else { 0.0 }).abs() > 0.001;
        let transitioning = self.prev.is_some() || self.cur.since.elapsed().as_secs_f32() * 1000.0 < LINE_IN;
        let bg_moving = self.bg_anim > 0.0 && self.bg_anim < 1.0;
        fading || transitioning || bg_moving || (self.opacity > 0.0 && playing && self.cur.key != Key::Empty)
            || (playing && self.wanted && self.lyrics.is_some())
    }

    /// Wake-up period when paused: catches seeks, which arrive with no event.
    pub fn idle_poll(&self) -> Option<Duration> {
        (self.wanted && self.lyrics.is_some()).then_some(Duration::from_millis(250))
    }

    pub fn poll_interval(&self, cfg: &Config, screen: &Screen) -> Option<Duration> {
        (self.wanted || self.opacity > 0.0).then(|| self.dock.poll_interval(&layout::place(&spec(cfg), screen)))
    }

    /// Place, update and (if anything visible changed) draw one frame.
    pub fn frame(&mut self, f: &Factories, cfg: &Config, tray: Option<HWND>, screen: &Screen, t_ms: Option<f64>, relayout: bool) {
        let now = Instant::now();
        let dt = timeline::frame_dt_ms((now - self.last_frame).as_secs_f32() * 1000.0);
        self.last_frame = now;

        self.opacity = timeline::approach(self.opacity, if self.wanted { 1.0 } else { 0.0 }, dt, FADE);
        if self.opacity <= 0.0 && !self.wanted {
            self.dock.hide();
            return;
        }

        let place: Placement = layout::place(&spec(cfg), screen);
        let Some(target) = self.dock.target(tray, place, relayout) else { return };
        if self.covered {
            return;
        }
        if target.fresh || self.size != (target.width, target.height) {
            self.force = true;
        }
        if self.size != (target.width, target.height) {
            self.size = (target.width, target.height);
            // Widths changed: layouts may need a different fit.
            self.cur.line = None;
            self.prev = None;
            self.bg = None;
        }
        if let Err(e) = self.ensure_resources(f, cfg, screen.scale, target.width, target.height) {
            log!("taskbar: lyrics strip: drawing resources failed: {e}");
            return;
        }

        let s = screen.scale as f32;
        let (w, h) = (target.width as f32, target.height as f32);
        let pad = 4.0 * s;
        let avail = (w - 2.0 * pad).max(10.0);

        // ---- what should show ----
        let shown = match (&self.lyrics, t_ms) {
            (Some(ly), Some(t)) => {
                let fr = timeline::frame(ly, t, cfg.interlude_gap_ms as f64);
                let key = match fr.show {
                    Show::Nothing => Key::Empty,
                    Show::Line(i) => Key::Line(i),
                    Show::Dots { start, end } => Key::Dots { start, end },
                };
                Some((key, fr.bg, t))
            }
            _ => None,
        };
        let (key, bg_idx, t) = shown.unwrap_or((Key::Empty, None, 0.0));

        if key != self.cur.key {
            self.retire_current();
            self.cur = Row::new(key, now);
        }
        if let Some(p) = &self.prev {
            if p.since.elapsed().as_secs_f32() * 1000.0 >= LINE_IN {
                self.prev = None;
            }
        }
        if let (Key::Line(i), None) = (self.cur.key, &self.cur.line) {
            self.cur.line = self.build_line(f, i, false, avail);
        }

        // Background vocal row.
        match (bg_idx, &self.bg) {
            (Some(i), Some((j, _))) if i == *j => {}
            (Some(i), _) => {
                if let Some(l) = self.build_line(f, i, true, avail * 0.9) {
                    self.bg = Some((i, l));
                }
            }
            _ => {}
        }
        self.bg_anim = timeline::approach(self.bg_anim, if bg_idx.is_some() { 1.0 } else { 0.0 }, dt, BG_IN);
        if self.bg_anim <= 0.0 && bg_idx.is_none() {
            self.bg = None;
        }

        // ---- has anything visible changed? ----
        let cur_p = (self.cur.since.elapsed().as_secs_f32() * 1000.0 / LINE_IN).min(1.0);
        let prev_p = self.prev.as_ref().map(|p| (p.since.elapsed().as_secs_f32() * 1000.0 / LINE_IN).min(1.0));
        let main_sweep = self.cur.line.as_ref().map(|l| sweep_head(l, t, s, cfg.main_font_px as f32)).unwrap_or(0.0);
        let bg_sweep = self.bg.as_ref().map(|(_, l)| sweep_head(l, t, s, cfg.bg_font_px as f32)).unwrap_or(0.0);
        let dots = if let Key::Dots { start, end } = self.cur.key { dots_state(start, end, t) } else { [0.0; 4] };
        let main_scroll = self.cur.line.as_ref().filter(|l| l.layout.width > avail).map(|l| scroll_point(l, t, s, cfg.main_font_px as f32));
        let bg_scroll = self.bg.as_ref().filter(|(_, l)| l.layout.width > avail).map(|(_, l)| scroll_point(l, t, s, cfg.bg_font_px as f32));
        // Everything but the sweep heads: when only those moved, just the column around
        // them is redrawn and handed to DWM.
        let mut hs = DefaultHasher::new();
        (
            format!("{:?}", self.cur.key),
            bg_idx,
            q(cur_p, 64.0),
            prev_p.map(|p| q(p, 64.0)),
            q(self.bg_anim, 64.0),
            dots.map(|d| q(d, 64.0)),
            q(self.opacity, 64.0),
            main_scroll.map(|x| q(x, 1.0)),
            bg_scroll.map(|x| q(x, 1.0)),
        )
            .hash(&mut hs);
        let layout_sig = hs.finish();
        let heads = (q(main_sweep, 4.0), q(bg_sweep, 4.0));
        if layout_sig == self.last_sig && heads == self.last_heads_q && !self.force {
            return;
        }
        let fe_main = feather(s, cfg.main_font_px as f32);
        let fe_bg = feather(s, cfg.bg_font_px as f32);
        let dirty = if layout_sig == self.last_sig && !self.force {
            // Only sweeps moved. Old and new head positions (window x) per row.
            let mut span: Option<(f32, f32)> = None;
            let mut add = |old: Option<(f32, f32)>, new_head: f32, fe: f32| {
                if let Some((x, old_head)) = old {
                    if (old_head - new_head).abs() > 0.01 {
                        let lo = x + old_head.min(new_head) - fe - 2.0;
                        let hi = x + old_head.max(new_head) + 2.0;
                        span = Some(span.map_or((lo, hi), |(a, b)| (a.min(lo), b.max(hi))));
                    }
                }
            };
            add(self.drawn_heads.0, main_sweep, fe_main);
            add(self.drawn_heads.1, bg_sweep, fe_bg);
            span.map(|(lo, hi)| (lo.floor().max(0.0), hi.ceil().min(w)))
        } else {
            None
        };
        if layout_sig == self.last_sig && !self.force && dirty.is_none() {
            self.last_heads_q = heads;
            return;
        }

        // ---- draw ----
        let canvas = self.canvas.as_ref().unwrap();
        let res = self.res.as_ref().unwrap();
        let rt: &ID2D1RenderTarget = &canvas.rt;
        let t_draw = Instant::now();
        match dirty {
            Some((lo, hi)) => canvas.begin_partial(lo, hi),
            None => canvas.begin(),
        }
        unsafe {
            rt.PushAxisAlignedClip(
                &D2D_RECT_F { left: pad * 0.5, top: 0.0, right: w - pad * 0.5, bottom: h },
                D2D1_ANTIALIAS_MODE_PER_PRIMITIVE,
            );
        }
        let bg_ease = timeline::smoothstep(self.bg_anim);
        let bg_h = self.bg.as_ref().map(|(_, l)| l.layout.height).unwrap_or(cfg.bg_font_px as f32 * 1.35 * s);
        let rise = h * 0.45;

        let ctx = DrawCtx { rt, res, cfg, s, w, h, pad, avail, t, bg_ease, bg_h, drawn: Default::default() };
        if let Some(p) = &self.prev {
            let e = timeline::ease_out_cubic(prev_p.unwrap_or(1.0));
            ctx.row(p, -rise * 0.55 * e, (1.0 - e).powi(2), false);
        }
        let main_drawn = {
            // Only the current row's position may drive the next sweep-only repaint.
            ctx.drawn.set(None);
            let e = timeline::ease_out_cubic(cur_p);
            ctx.row(&self.cur, rise * 0.5 * (1.0 - e), e, true);
            ctx.drawn.take()
        };
        let mut bg_drawn = None;
        if let Some((_, l)) = &self.bg {
            if bg_ease > 0.01 {
                // Interlude dots make room like a line does, so the vocal sits under
                // them instead of across them.
                let main_h = match (&self.cur.line, self.cur.key) {
                    (Some(m), _) => m.layout.height,
                    (None, Key::Dots { .. }) => dots_height(s),
                    _ => 0.0,
                };
                let y = if main_h > 0.0 {
                    ((h - main_h - l.layout.height) / 2.0).max(1.0) + main_h + 1.0 * s
                } else {
                    (h - l.layout.height) / 2.0 + cfg.main_font_px as f32 * 0.55 * s
                } + (1.0 - bg_ease) * 7.0 * s;
                ctx.line(l, &res.bg, y, bg_ease, cfg.bg_font_px as f32);
                bg_drawn = ctx.drawn.take();
            }
        }
        unsafe { rt.PopAxisAlignedClip() };
        if dirty.is_some() {
            unsafe { rt.PopAxisAlignedClip() };
        }

        let ended = canvas.end();
        let t_end = Instant::now();
        match ended {
            Ok(()) => {
                canvas.present(target.hwnd, self.opacity, dirty);
                crate::diag::frame(t_end - t_draw, t_end.elapsed());
                if let Some(dir) = crate::diag::dump_dir() {
                    if self.last_dump.elapsed() >= Duration::from_millis(250) {
                        self.last_dump = Instant::now();
                        self.dumps += 1;
                        let path = dir.join(format!("lyrics-{:04}.bmp", self.dumps));
                        let _ = self.canvas.as_ref().unwrap().save_bmp(&path);
                    }
                }
                self.last_sig = layout_sig;
                self.last_heads_q = heads;
                self.drawn_heads = (main_drawn, bg_drawn);
                self.force = false;
            }
            Err(e) => {
                log!("taskbar: lyrics strip: EndDraw failed ({e}); rebuilding the render target");
                self.canvas = None;
                self.res = None;
                self.force = true;
            }
        }
    }

    fn ensure_resources(&mut self, f: &Factories, cfg: &Config, scale: f64, w: i32, h: i32) -> windows::core::Result<()> {
        match &mut self.canvas {
            Some(c) => c.resize(w, h)?,
            None => {
                self.canvas = Some(Canvas::new(f, w, h)?);
                self.res = None;
                // Masks belong to the old render target.
                self.cur.line = None;
                self.prev = None;
                self.bg = None;
            }
        }
        let rt: &ID2D1RenderTarget = &self.canvas.as_ref().unwrap().rt;
        if self.res.is_none() {
            let a = |v: u8| v as f32 / 255.0;
            self.res = Some(Res {
                main: gfx::sweep_brush(rt, gfx::white(a(cfg.sung_alpha)), gfx::white(a(cfg.unsung_alpha)))?,
                bg: gfx::sweep_brush(rt, gfx::white(a(cfg.bg_line_alpha)), gfx::white(a(cfg.bg_line_alpha) * 0.5))?,
                solid: gfx::solid(rt, gfx::white(1.0))?,
            });
        }
        if self.main_fmt.is_none() || self.bg_fmt.is_none() || self.fmt_scale != scale {
            let s = scale as f32;
            self.main_fmt = Some(f.text_format(&cfg.font_family, DWRITE_FONT_WEIGHT_SEMI_BOLD, cfg.main_font_px as f32 * s)?);
            self.bg_fmt = Some(f.text_format(&cfg.font_family, DWRITE_FONT_WEIGHT_SEMI_BOLD, cfg.bg_font_px as f32 * s)?);
            self.fmt_scale = scale;
            self.cur.line = None;
            self.prev = None;
            self.bg = None;
        }
        Ok(())
    }

    /// Lay out main line `i` (or background vocal `i`), shrinking it to fit `avail`
    /// down to a floor; anything still wider scrolls with the sweep.
    fn build_line(&self, f: &Factories, i: usize, bg: bool, avail: f32) -> Option<LineVis> {
        let ly = self.lyrics.as_ref()?;
        let (text, sylls, start, end) = if bg {
            let b = ly.bg.get(i)?;
            (&b.text, b.sylls.as_deref(), b.start, b.end)
        } else {
            let l = ly.lines.get(i)?;
            (&l.text, l.sylls.as_deref(), l.start, l.end)
        };
        let fmt = if bg { self.bg_fmt.as_ref()? } else { self.main_fmt.as_ref()? };
        let shown = timeline::display_text(text, sylls);
        let mut layout = f.layout(&shown, fmt).ok()?;
        if layout.width > avail {
            let size = unsafe { fmt.GetFontSize() };
            let floor = 9.0 * self.fmt_scale as f32;
            let fitted = (size * avail / layout.width * 0.98).max(floor.min(size));
            unsafe {
                let range = DWRITE_TEXT_RANGE { startPosition: 0, length: u32::MAX };
                let _ = layout.layout.SetFontSize(fitted, range);
                let mut m = DWRITE_TEXT_METRICS::default();
                if layout.layout.GetMetrics(&mut m).is_ok() {
                    layout.width = m.widthIncludingTrailingWhitespace;
                    layout.height = m.height;
                }
            }
        }
        let spans = sylls.filter(|s| !s.is_empty()).map(|segs| {
            let ends = timeline::seg_end_offsets(segs);
            let mut x0 = 0.0;
            segs.iter()
                .zip(ends)
                .map(|(seg, end)| {
                    let x1 = layout.x_after(end).max(x0);
                    let span = SegSpan { start: seg.start, end: seg.end, x0, x1 };
                    x0 = x1;
                    span
                })
                .collect()
        });
        Some(LineVis { layout, spans, start, end, mask: Default::default() })
    }
}

fn q(v: f32, steps: f32) -> i32 {
    (v * steps).round() as i32
}

/// Sweep head in layout pixels, stretched so 0 is "nothing lit" and the full width
/// is "everything lit" with the one-em soft edge included.
fn sweep_head(l: &LineVis, t: f64, s: f32, font_px: f32) -> f32 {
    let w = l.layout.width.max(1.0);
    let x = timeline::sweep_x(l.spans.as_deref(), l.start, w, t);
    x / w * (w + feather(s, font_px))
}

/// The point a too-wide row scrolls to keep in view, in layout pixels: the sweep
/// head with syllable timing; without it the whole line lights at once, so follow
/// the line's own progress instead of jumping straight to its end.
fn scroll_point(l: &LineVis, t: f64, s: f32, font_px: f32) -> f32 {
    let w = l.layout.width;
    match l.spans {
        Some(_) => sweep_head(l, t, s, font_px).min(w),
        None => w * timeline::line_progress(l.start, l.end, t),
    }
}

fn feather(s: f32, font_px: f32) -> f32 {
    font_px * s * 0.9
}

/// The height the interlude dots take up (largest dot, with a little air), for
/// stacking a background vocal under them.
fn dots_height(s: f32) -> f32 {
    2.0 * 3.2 * 1.18 * 1.12 * s + 2.0 * s
}

/// Per dot fill 0..1 (three dots) and an overall scale for the exit.
fn dots_state(start: f64, end: f64, t: f64) -> [f32; 4] {
    let p = timeline::gap_progress(start, end, t);
    let fill = |i: f32| (p * 3.0 - i).clamp(0.0, 1.0);
    let exit = ((end - t) / DOTS_EXIT_MS).clamp(0.0, 1.0) as f32;
    // Breathing, quantised by the signature; slow enough to stay calm.
    let breathe = 1.0 + 0.12 * ((t / 1400.0 * std::f64::consts::TAU).sin() as f32);
    [fill(0.0), fill(1.0), fill(2.0), timeline::ease_out_cubic(exit) * breathe]
}

struct DrawCtx<'a> {
    rt: &'a ID2D1RenderTarget,
    res: &'a Res,
    cfg: &'a Config,
    s: f32,
    w: f32,
    h: f32,
    pad: f32,
    avail: f32,
    t: f64,
    bg_ease: f32,
    bg_h: f32,
    /// Set by `line`: (window x of the text, sweep head) of the row just drawn.
    drawn: std::cell::Cell<Option<(f32, f32)>>,
}

impl DrawCtx<'_> {
    fn row(&self, row: &Row, dy: f32, alpha: f32, current: bool) {
        if alpha <= 0.005 {
            return;
        }
        match row.key {
            Key::Empty => {}
            Key::Line(_) => {
                let Some(l) = &row.line else { return };
                let center = (self.h - l.layout.height) / 2.0;
                // Only the current line makes room for a background vocal.
                let lifted = ((self.h - l.layout.height - self.bg_h) / 2.0).max(1.0);
                let lift = if current { self.bg_ease } else { row.lift };
                let y = center + (lifted - center) * lift;
                self.line(l, &self.res.main, y + dy, alpha, self.cfg.main_font_px as f32);
            }
            Key::Dots { start, end } => {
                let dh = dots_height(self.s);
                let center = self.h / 2.0;
                let lifted = ((self.h - dh - self.bg_h) / 2.0).max(1.0) + dh / 2.0;
                let lift = if current { self.bg_ease } else { row.lift };
                self.dots(start, end, (lifted - center) * lift + dy, alpha)
            }
        }
    }

    fn line(&self, l: &LineVis, brush: &ID2D1LinearGradientBrush, y: f32, alpha: f32, font_px: f32) {
        let lw = l.layout.width;
        let head = sweep_head(l, self.t, self.s, font_px);
        let x = if lw > self.avail {
            self.pad - layout::follow_scroll(lw, self.avail, scroll_point(l, self.t, self.s, font_px))
        } else {
            layout::align_x(&self.cfg.align, lw, self.w, self.pad)
        };
        // Whole pixels: the mask is drawn 1:1, never resampled.
        let (x, y) = (x.round(), y.round());
        self.drawn.set(Some((x, head)));
        let Some(mask) = l.mask(self.rt, &self.res.solid) else { return };
        let size = unsafe { mask.GetSize() };
        let dest = |dy: f32| D2D_RECT_F {
            left: x - MASK_PAD,
            top: y - MASK_PAD + dy,
            right: x - MASK_PAD + size.width,
            bottom: y - MASK_PAD + dy + size.height,
        };
        let fe = feather(self.s, font_px);
        unsafe {
            // FillOpacityMask requires aliased mode; the mask itself is anti-aliased.
            self.rt.SetAntialiasMode(D2D1_ANTIALIAS_MODE_ALIASED);
            if self.cfg.text_shadow {
                self.res.solid.SetColor(&gfx::black(0.42 * alpha));
                self.rt.FillOpacityMask(&mask, &self.res.solid, D2D1_OPACITY_MASK_CONTENT_GRAPHICS, Some(&dest(self.s.round().max(1.0))), None);
            }
            brush.SetStartPoint(gfx::v2(x + head - fe, 0.0));
            brush.SetEndPoint(gfx::v2(x + head, 0.0));
            brush.SetOpacity(alpha);
            self.rt.FillOpacityMask(&mask, brush, D2D1_OPACITY_MASK_CONTENT_GRAPHICS, Some(&dest(0.0)), None);
            self.rt.SetAntialiasMode(D2D1_ANTIALIAS_MODE_PER_PRIMITIVE);
        }
    }

    fn dots(&self, start: f64, end: f64, dy: f32, alpha: f32) {
        let [f0, f1, f2, scale] = dots_state(start, end, self.t);
        let (r, gap) = (3.2 * self.s, 15.0 * self.s);
        let x0 = layout::align_x(&self.cfg.align, 2.0 * gap + 2.0 * r, self.w, self.pad) + r;
        let cy = self.h / 2.0 + dy;
        let unsung = self.cfg.unsung_alpha as f32 / 255.0;
        let sung = self.cfg.sung_alpha as f32 / 255.0;
        for (i, fill) in [f0, f1, f2].into_iter().enumerate() {
            // Each dot swells a touch as it fills.
            let rr = r * scale * (1.0 + 0.18 * timeline::smoothstep(fill));
            if rr <= 0.1 {
                continue;
            }
            let e = D2D1_ELLIPSE { point: gfx::v2(x0 + i as f32 * gap, cy), radiusX: rr, radiusY: rr };
            let a = unsung + (sung - unsung) * fill;
            unsafe {
                self.res.solid.SetColor(&gfx::white(a * alpha));
                self.rt.FillEllipse(&e, &self.res.solid);
            }
        }
    }
}
