//! Where a strip goes. Pure geometry in physical pixels, so it can be tested without
//! a taskbar: the dock feeds it the live taskbar, monitor and work-area rectangles and
//! the monitor's scale, and config values (which are in DIPs) are scaled here.

/// A rectangle in physical screen pixels.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Rect {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}

impl Rect {
    pub const fn new(left: i32, top: i32, right: i32, bottom: i32) -> Self {
        Self { left, top, right, bottom }
    }
    pub fn width(&self) -> i32 {
        self.right - self.left
    }
    pub fn height(&self) -> i32 {
        self.bottom - self.top
    }
    pub fn is_empty(&self) -> bool {
        self.width() <= 0 || self.height() <= 0
    }
    /// Same rectangle, expressed relative to `origin`'s top-left corner.
    pub fn relative_to(&self, origin: &Rect) -> Rect {
        Rect::new(self.left - origin.left, self.top - origin.top, self.right - origin.left, self.bottom - origin.top)
    }
}

/// What the layout needs to know about the screen.
#[derive(Clone, Copy, Debug)]
pub struct Screen {
    /// The monitor the taskbar is on.
    pub monitor: Rect,
    /// That monitor's work area (the monitor minus docked, non-auto-hide bars).
    pub work: Rect,
    /// The taskbar window's current bounds, when there is one. While an auto-hide
    /// bar is tucked away this is mostly off-screen, which is what a strip docked on
    /// it should follow.
    pub taskbar: Option<Rect>,
    /// Physical pixels per DIP (1.0 at 96 dpi).
    pub scale: f64,
}

/// The settings one strip is placed by (DIPs where they are lengths).
#[derive(Clone, Debug, PartialEq)]
pub struct StripSpec {
    /// "taskbar" (on the bar) or anything else (floating just inside the work area).
    pub placement: String,
    /// "top" | "bottom"
    pub v_pos: String,
    /// "left" | "center" | "right"
    pub align: String,
    pub x_offset: i32,
    pub width: i32,
    pub preferred_height: f64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Placement {
    /// Screen pixels.
    pub bounds: Rect,
    /// The strip sits inside the taskbar window itself, so it must be hosted as a
    /// child of it (see `dock`).
    pub on_taskbar: bool,
}

pub fn norm_align(v: &str) -> &'static str {
    match v {
        "left" => "left",
        "right" => "right",
        _ => "center",
    }
}

pub fn norm_v_pos(v: &str) -> &'static str {
    if v == "top" {
        "top"
    } else {
        "bottom"
    }
}

/// Where a strip described by `spec` goes on `screen`.
pub fn place(spec: &StripSpec, screen: &Screen) -> Placement {
    let s = screen.scale.max(0.5);
    let px = |dip: f64| (dip * s).round() as i32;
    let (mon, wa) = (screen.monitor, screen.work);
    let at_top = norm_v_pos(&spec.v_pos) == "top";
    let on_bar_wanted = spec.placement == "taskbar";
    let pref_h = px(spec.preferred_height.max(8.0));
    let min_bar = px(24.0);
    // Thickness of a bar docked on the edge we hug (0 when it is elsewhere or auto-hides).
    let band_h = if at_top { wa.top - mon.top } else { mon.bottom - wa.bottom };

    let (h, top, on_taskbar) = match screen.taskbar {
        Some(tb)
            if on_bar_wanted
                && tb.width() > tb.height()
                && tb.height() >= min_bar
                && ((tb.top + tb.height() / 2) < (mon.top + mon.height() / 2)) == at_top =>
        {
            // Inside the actual taskbar window, wherever it is right now.
            let h = (tb.height() - px(2.0)).min(pref_h);
            (h, tb.top + (tb.height() - h) / 2, true)
        }
        _ if on_bar_wanted && band_h >= min_bar => {
            // A bar band on this edge, but not one we can dock into: float over it.
            let h = (band_h - px(2.0)).min(pref_h);
            let top = if at_top { mon.top + (band_h - h) / 2 } else { wa.bottom + (band_h - h) / 2 };
            (h, top, false)
        }
        _ => {
            // Floating strip just inside the work area on this edge.
            let top = if at_top { wa.top + px(6.0) } else { wa.bottom - pref_h - px(6.0) };
            (pref_h, top, false)
        }
    };

    let w = px(spec.width.max(40) as f64).min((mon.width() as f64 * 0.62) as i32).max(1);
    let xoff = px(spec.x_offset.clamp(0, 4000) as f64);
    let left = match norm_align(&spec.align) {
        "left" => mon.left + xoff,
        "right" => mon.right - w - xoff,
        _ => mon.left + (mon.width() - w) / 2,
    };
    Placement { bounds: Rect::new(left, top, left + w, top + h.max(1)), on_taskbar }
}

/// X of a `content_w`-wide row inside a strip `avail_w` wide, honouring the alignment.
/// `pad` is the edge inset, and the floor when the content is wider than the strip.
pub fn align_x(align: &str, content_w: f32, avail_w: f32, pad: f32) -> f32 {
    match norm_align(align) {
        "left" => pad,
        "right" => (avail_w - content_w - pad).max(pad),
        _ => ((avail_w - content_w) / 2.0).max(pad),
    }
}

/// Horizontal scroll for a row wider than the strip: keeps the sweep point in view
/// (about 60% across) and never scrolls past either end. 0 when the row fits.
pub fn follow_scroll(content_w: f32, avail_w: f32, sweep_x: f32) -> f32 {
    if content_w <= avail_w {
        return 0.0;
    }
    (sweep_x - avail_w * 0.6).clamp(0.0, content_w - avail_w)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(placement: &str, v_pos: &str, align: &str) -> StripSpec {
        StripSpec {
            placement: placement.into(),
            v_pos: v_pos.into(),
            align: align.into(),
            x_offset: 16,
            width: 620,
            preferred_height: 48.0,
        }
    }

    /// 1920x1080 at 100%, 48 px taskbar at the bottom.
    fn bottom_bar(scale: f64) -> Screen {
        let bar = (48.0 * scale) as i32;
        let (w, h) = ((1920.0 * scale) as i32, (1080.0 * scale) as i32);
        Screen {
            monitor: Rect::new(0, 0, w, h),
            work: Rect::new(0, 0, w, h - bar),
            taskbar: Some(Rect::new(0, h - bar, w, h)),
            scale,
        }
    }

    #[test]
    fn docks_inside_the_taskbar_centered() {
        let p = place(&spec("taskbar", "bottom", "center"), &bottom_bar(1.0));
        assert!(p.on_taskbar);
        assert_eq!(p.bounds.width(), 620);
        assert_eq!(p.bounds.left, (1920 - 620) / 2);
        // 46 px tall (bar minus 2), centred in the 48 px bar.
        assert_eq!(p.bounds.height(), 46);
        assert_eq!(p.bounds.top, 1080 - 48 + 1);
    }

    #[test]
    fn scales_dips_to_pixels() {
        let p = place(&spec("taskbar", "bottom", "left"), &bottom_bar(1.5));
        assert!(p.on_taskbar);
        assert_eq!(p.bounds.width(), 930);
        assert_eq!(p.bounds.left, 24); // 16 DIP inset
        assert_eq!(p.bounds.height(), 69); // 72 - 3, below the 72 px preferred height
    }

    #[test]
    fn right_alignment_insets_from_the_right_edge() {
        let p = place(&spec("taskbar", "bottom", "right"), &bottom_bar(1.0));
        assert_eq!(p.bounds.right, 1920 - 16);
    }

    #[test]
    fn follows_an_auto_hide_bar_off_screen() {
        // Auto-hide: the work area is the whole monitor; the bar is tucked away
        // with 2 px left showing.
        let mut s = bottom_bar(1.0);
        s.work = s.monitor;
        s.taskbar = Some(Rect::new(0, 1078, 1920, 1126));
        let p = place(&spec("taskbar", "bottom", "center"), &s);
        assert!(p.on_taskbar);
        assert_eq!(p.bounds.top, 1078 + 1);
    }

    #[test]
    fn floats_when_asked_or_when_the_bar_is_on_another_edge() {
        let s = bottom_bar(1.0);
        let above = place(&spec("above", "bottom", "center"), &s);
        assert!(!above.on_taskbar);
        assert_eq!(above.bounds.bottom, 1080 - 48 - 6);

        // "top" while the bar is at the bottom: nothing to dock into, float at the top.
        let top = place(&spec("taskbar", "top", "center"), &s);
        assert!(!top.on_taskbar);
        assert_eq!(top.bounds.top, 6);
    }

    #[test]
    fn floats_over_the_band_while_the_taskbar_is_unknown() {
        let mut s = bottom_bar(1.0);
        s.taskbar = None;
        let p = place(&spec("taskbar", "bottom", "center"), &s);
        assert!(!p.on_taskbar);
        assert_eq!(p.bounds.top, 1080 - 48 + 1);
    }

    #[test]
    fn vertical_taskbars_are_not_docked_into() {
        let s = Screen {
            monitor: Rect::new(0, 0, 1920, 1080),
            work: Rect::new(0, 0, 1860, 1080),
            taskbar: Some(Rect::new(1860, 0, 1920, 1080)),
            scale: 1.0,
        };
        assert!(!place(&spec("taskbar", "bottom", "center"), &s).on_taskbar);
    }

    #[test]
    fn width_is_capped_to_the_monitor() {
        let mut sp = spec("taskbar", "bottom", "center");
        sp.width = 5000;
        let p = place(&sp, &bottom_bar(1.0));
        assert_eq!(p.bounds.width(), (1920.0 * 0.62) as i32);
    }

    #[test]
    fn secondary_monitor_coordinates_are_respected() {
        let s = Screen {
            monitor: Rect::new(-1280, 0, 0, 1024),
            work: Rect::new(-1280, 0, 0, 984),
            taskbar: Some(Rect::new(-1280, 984, 0, 1024)),
            scale: 1.0,
        };
        let p = place(&spec("taskbar", "bottom", "left"), &s);
        assert_eq!(p.bounds.left, -1280 + 16);
        assert!(p.on_taskbar);
    }

    #[test]
    fn align_and_scroll() {
        assert_eq!(align_x("left", 100.0, 500.0, 2.0), 2.0);
        assert_eq!(align_x("right", 100.0, 500.0, 2.0), 398.0);
        assert_eq!(align_x("center", 100.0, 500.0, 2.0), 200.0);
        assert_eq!(align_x("center", 600.0, 500.0, 2.0), 2.0);
        assert_eq!(follow_scroll(400.0, 500.0, 300.0), 0.0);
        assert_eq!(follow_scroll(800.0, 500.0, 100.0), 0.0);
        assert_eq!(follow_scroll(800.0, 500.0, 500.0), 200.0);
        assert_eq!(follow_scroll(800.0, 500.0, 800.0), 300.0);
    }
}
