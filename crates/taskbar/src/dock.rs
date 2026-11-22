//! A click-through, per-pixel-alpha strip docked to the taskbar.
//!
//! Why not simply a topmost window: on Windows 11 the taskbar leaves the normal
//! z-band for a higher one whenever it, Start or the desktop is activated, and no
//! ordinary window (topmost, re-raised, or owned by the taskbar) can be above it
//! there. So a strip placed *on* the taskbar is a layered **child window of the
//! taskbar**: part of its surface, it moves, hides (auto-hide, full-screen apps) and
//! changes band together with it. A strip floating *beside* the bar is a topmost
//! window owned by the taskbar.
//!
//! Windows refuses to create a child inside the taskbar while it is lifted, so until
//! that is allowed the strip floats (and the module tries to get the taskbar back
//! into the normal band, see `lib.rs`). Either window dies with the taskbar when
//! Explorer restarts, so this type owns its window rather than being one, and
//! rebuilds it on the new taskbar.

use std::time::{Duration, Instant};

use syllable_core::log;
use windows::core::*;
use windows::Win32::Foundation::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::GetCurrentProcessId;
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::layout::{Placement, Rect};
use crate::shell;

/// How long to wait for a taskbar before showing un-docked (custom shells).
const FALLBACK_AFTER: Duration = Duration::from_secs(6);
const CLASS: PCWSTR = w!("Syllable.TaskbarStrip");

unsafe extern "system" fn strip_proc(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    match msg {
        WM_NCHITTEST => LRESULT(HTTRANSPARENT as isize),
        WM_MOUSEACTIVATE => LRESULT(MA_NOACTIVATE as isize),
        _ => DefWindowProcW(hwnd, msg, w, l),
    }
}

pub fn register_class() -> Result<()> {
    unsafe {
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(strip_proc),
            hInstance: GetModuleHandleW(None)?.into(),
            lpszClassName: CLASS,
            hCursor: LoadCursorW(None, IDC_ARROW)?,
            ..Default::default()
        };
        if RegisterClassExW(&wc) == 0 {
            let e = Error::from_thread();
            if e.code() != HRESULT::from_win32(ERROR_CLASS_ALREADY_EXISTS.0) {
                return Err(e);
            }
        }
    }
    Ok(())
}

struct Surface {
    hwnd: HWND,
    child: bool,
    /// The taskbar it is attached to (child) or owned by (floating).
    tray: Option<HWND>,
    /// Bounds last applied: parent-client pixels for a child, screen pixels floating.
    applied: Option<Rect>,
    shown: bool,
}

/// What the caller can draw into after `ensure`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Target {
    pub hwnd: HWND,
    pub width: i32,
    pub height: i32,
    /// The window was (re)created, moved or resized: everything must be redrawn.
    pub fresh: bool,
}

pub struct Dock {
    name: &'static str,
    surface: Option<Surface>,
    no_taskbar_since: Option<Instant>,
    dock_failures: u32,
    retry_at: Option<Instant>,
    /// Last state logged, so the log shows transitions, not every tick.
    logged: String,
    /// The target the last layout pass produced, reused by frames in between.
    last: Option<Target>,
    /// The last attempt to dock was refused by Windows (not just predicted to be).
    refused: bool,
    /// The taskbar was in the normal band at the last pass.
    accepted: bool,
}

impl Dock {
    pub fn new(name: &'static str) -> Self {
        Self { name, surface: None, no_taskbar_since: None, dock_failures: 0, retry_at: None, logged: String::new(), last: None, refused: false, accepted: false }
    }

    /// The last layout pass produced a window to draw into (nothing is drawn while
    /// waiting for a taskbar or hidden behind a full-screen app).
    pub fn has_target(&self) -> bool {
        self.last.is_some()
    }

    /// Docked, but the taskbar is put away behind a full-screen app: nothing drawn
    /// would be seen.
    pub fn covered(&self) -> bool {
        self.surface.as_ref().is_some_and(|s| s.child && s.tray.is_some_and(shell::full_screen_app_in_front))
    }

    /// Windows refused the last attempt to dock (and the strip floats meanwhile).
    pub fn refused(&self) -> bool {
        self.refused && !self.is_docked()
    }

    pub fn is_docked(&self) -> bool {
        self.surface.as_ref().is_some_and(|s| s.child)
    }

    /// Placed on the taskbar but still floating, waiting to be allowed to dock.
    pub fn wants_dock(&self, place: &Placement) -> bool {
        place.on_taskbar && !self.is_docked()
    }

    /// How soon the dock wants another `ensure` (safety net for changes that arrive
    /// without an event: work-area or DPI changes, a window going full screen).
    pub fn poll_interval(&self, place: &Placement) -> Duration {
        if self.wants_dock(place) {
            Duration::from_millis(250)
        } else if self.is_docked() {
            Duration::from_secs(2)
        } else {
            Duration::from_millis(500)
        }
    }

    fn note(&mut self, state: String) {
        if state != self.logged {
            log!("taskbar: {} {}", self.name, state);
            self.logged = state;
        }
    }

    /// The window to draw into: the last layout pass's result, or a new pass when
    /// `relayout` is set (an event or the maintenance poll asked for one).
    pub fn target(&mut self, tray: Option<HWND>, place: Placement, relayout: bool) -> Option<Target> {
        if !relayout {
            if let Some(t) = self.last {
                if shell::is_window(t.hwnd) {
                    return Some(Target { fresh: false, ..t });
                }
            }
        }
        self.last = self.ensure(tray, place);
        self.last
    }

    /// Bring the window into the wanted state for `place`: the right kind, attached
    /// to the current taskbar, placed and shown. `None` when nothing can be shown now.
    fn ensure(&mut self, tray: Option<HWND>, place: Placement) -> Option<Target> {
        self.reap();
        let now = Instant::now();
        let want_child = place.on_taskbar && tray.is_some();
        // The band is only a hint for *when* to retry quickly: whether docking works
        // is decided by trying (and a real refusal is what the unblock reacts to).
        let accepts = tray.is_some_and(shell::accepts_children);
        if accepts && !self.accepted {
            self.retry_at = None; // the taskbar just came back to the normal band
        }
        self.accepted = accepts;

        if let Some(s) = &self.surface {
            let tray_replaced = tray.is_some() && s.tray != tray;
            if tray_replaced || (s.child && !want_child) {
                // The taskbar was replaced, or placement moved off the bar.
                self.destroy();
            }
        }

        if want_child && !self.is_docked() && self.retry_at.is_none_or(|t| now >= t) {
            match self.create(tray, true) {
                Ok(hwnd) => {
                    // Docked: the interim floating window (if any) goes.
                    self.destroy();
                    self.surface = Some(Surface { hwnd, child: true, tray, applied: None, shown: false });
                    self.logged.clear();
                    self.dock_failures = 0;
                    self.refused = false;
                    self.retry_at = None;
                }
                Err(e) => {
                    // Back off instead of hammering CreateWindowEx; a return to the
                    // normal band retries at once (above).
                    self.dock_failures = (self.dock_failures + 1).min(5);
                    self.retry_at = Some(now + Duration::from_millis(250 << self.dock_failures));
                    self.refused = true;
                    let band = tray.and_then(shell::window_band);
                    let fg = shell::class_name(unsafe { GetForegroundWindow() });
                    self.note(format!("dock refused: {:?} (band {band:?}, foreground {fg})", e.code()));
                }
            }
        }

        if self.surface.is_none() {
            if tray.is_none() {
                // Explorer is (re)starting: wait for it rather than appear un-docked
                // and jump. Give up after a while for shells with no taskbar.
                let since = *self.no_taskbar_since.get_or_insert(now);
                if now - since < FALLBACK_AFTER {
                    self.note("waiting for a taskbar".into());
                    return None;
                }
            }
            match self.create(tray, false) {
                Ok(hwnd) => {
                    self.surface = Some(Surface { hwnd, child: false, tray, applied: None, shown: false });
                    self.logged.clear();
                }
                Err(e) => {
                    self.note(format!("window creation failed: {e}"));
                    return None;
                }
            }
        }
        if tray.is_some() {
            self.no_taskbar_since = None;
        }

        let s = self.surface.as_mut()?;
        let mut fresh = false;
        let state: String;
        if s.child {
            let tray = s.tray?;
            let (ox, oy) = shell::client_origin(tray)?;
            let r = place.bounds.relative_to(&Rect::new(ox, oy, ox, oy));
            // Keep it above the taskbar's own content, which must never draw over it.
            if s.applied != Some(r) || !s.shown || !above_siblings(s.hwnd) {
                let ok = unsafe {
                    SetWindowPos(s.hwnd, Some(HWND_TOP), r.left, r.top, r.width(), r.height(), SWP_NOACTIVATE | SWP_SHOWWINDOW)
                };
                match ok {
                    // Remember the bounds only once applied, so a refused move is retried.
                    Ok(()) => {
                        fresh |= s.applied != Some(r) || !s.shown;
                        s.applied = Some(r);
                        s.shown = true;
                    }
                    Err(e) => log!("taskbar: {}: could not place the docked strip: {e}", self.name),
                }
            }
            let band = shell::window_band(tray);
            state = format!("docked in taskbar {:?} (band {band:?})", tray.0);
        } else {
            // A docked child hides with the taskbar by construction; a floating window
            // follows the taskbar's fate by hand.
            if let Some(tray) = s.tray {
                if shell::full_screen_app_in_front(tray) {
                    if s.shown {
                        unsafe {
                            let _ = ShowWindow(s.hwnd, SW_HIDE);
                        }
                        s.shown = false;
                    }
                    self.last = None;
                    self.note("floating, hidden behind a full-screen app".into());
                    return None;
                }
            }
            let r = place.bounds;
            let flags = SWP_NOACTIVATE | SWP_SHOWWINDOW;
            let moved = s.applied != Some(r) || !s.shown;
            unsafe {
                let _ = if moved {
                    SetWindowPos(s.hwnd, Some(HWND_TOPMOST), r.left, r.top, r.width(), r.height(), flags)
                } else {
                    SetWindowPos(s.hwnd, Some(HWND_TOPMOST), 0, 0, 0, 0, flags | SWP_NOMOVE | SWP_NOSIZE)
                };
            }
            fresh |= moved;
            s.applied = Some(r);
            s.shown = true;
            let why = match (place.on_taskbar, tray) {
                (_, None) => "floating un-docked (no taskbar)".to_string(),
                (true, Some(t)) => format!(
                    "floating over the taskbar until it accepts children (band {:?}, foreground {})",
                    shell::window_band(t),
                    shell::class_name(unsafe { GetForegroundWindow() })
                ),
                (false, Some(_)) => "floating beside the taskbar".to_string(),
            };
            state = why;
        }
        let r = s.applied?;
        let target = Target { hwnd: s.hwnd, width: r.width().max(1), height: r.height().max(1), fresh };
        self.note(state);
        Some(target)
    }

    pub fn hide(&mut self) {
        self.last = None;
        if let Some(s) = &mut self.surface {
            if s.shown {
                unsafe {
                    let _ = ShowWindow(s.hwnd, SW_HIDE);
                }
                s.shown = false;
            }
        }
    }

    /// Notice a window destroyed with its taskbar (Explorer exit or crash).
    fn reap(&mut self) {
        if let Some(s) = &self.surface {
            if !shell::is_window(s.hwnd) {
                log!("taskbar: {}: window destroyed with its taskbar, waiting for the new one", self.name);
                self.surface = None;
                self.no_taskbar_since = Some(Instant::now());
                self.logged.clear();
            }
        }
    }

    fn create(&self, tray: Option<HWND>, child: bool) -> Result<HWND> {
        let name = HSTRING::from(self.name);
        let hwnd = unsafe {
            let inst = GetModuleHandleW(None)?;
            if child {
                CreateWindowExW(
                    WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_NOACTIVATE,
                    CLASS,
                    &name,
                    WS_CHILD | WS_CLIPSIBLINGS,
                    0,
                    0,
                    1,
                    1,
                    tray,
                    None,
                    Some(inst.into()),
                    None,
                )?
            } else {
                CreateWindowExW(
                    WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW | WS_EX_TOPMOST,
                    CLASS,
                    &name,
                    WS_POPUP,
                    0,
                    0,
                    1,
                    1,
                    tray, // owner: drops behind full-screen apps with the taskbar
                    None,
                    Some(inst.into()),
                    None,
                )?
            }
        };
        Ok(hwnd)
    }

    /// Tear down the current window on purpose.
    pub fn destroy(&mut self) {
        self.last = None;
        if let Some(s) = self.surface.take() {
            unsafe {
                let _ = DestroyWindow(s.hwnd);
            }
        }
    }
}

impl Drop for Dock {
    fn drop(&mut self) {
        self.destroy();
    }
}

/// True when every sibling above `hwnd` belongs to this process (our other strip).
fn above_siblings(hwnd: HWND) -> bool {
    let me = unsafe { GetCurrentProcessId() };
    let mut h = unsafe { GetWindow(hwnd, GW_HWNDPREV) }.unwrap_or_default();
    while !h.is_invalid() {
        let mut pid = 0;
        unsafe { GetWindowThreadProcessId(h, Some(&mut pid)) };
        if pid != me {
            return false;
        }
        h = unsafe { GetWindow(h, GW_HWNDPREV) }.unwrap_or_default();
    }
    true
}
