//! The primary taskbar (`Shell_TrayWnd`): its handle, where it is, its z-band, and
//! whether a full-screen app covers it. All read on the strip thread, which is
//! per-monitor-DPI-aware, so every rectangle here is in physical pixels.

use windows::core::*;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::*;
use windows::Win32::UI::HiDpi::*;
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::layout::{Rect, Screen};

/// `ZBID_DESKTOP`: the normal z-band. Windows 11 lifts the taskbar out of it (to
/// band 6) while the taskbar, Start or the desktop is activated, and while it is
/// lifted refuses to let another process create (or re-parent) a child inside it.
pub const ZBID_DESKTOP: u32 = 1;

pub fn rect(r: RECT) -> Rect {
    Rect::new(r.left, r.top, r.right, r.bottom)
}

pub fn find_taskbar() -> Option<HWND> {
    unsafe { FindWindowW(w!("Shell_TrayWnd"), None).ok().filter(|h| !h.is_invalid()) }
}

pub fn is_window(h: HWND) -> bool {
    !h.is_invalid() && unsafe { IsWindow(Some(h)).as_bool() }
}

pub fn class_name(h: HWND) -> String {
    let mut buf = [0u16; 128];
    let n = unsafe { GetClassNameW(h, &mut buf) };
    String::from_utf16_lossy(&buf[..n.max(0) as usize])
}

pub fn window_rect(h: HWND) -> Option<Rect> {
    let mut r = RECT::default();
    unsafe { GetWindowRect(h, &mut r).ok()? };
    Some(rect(r))
}

/// The taskbar's client area in screen pixels: the origin child windows are placed from.
pub fn client_origin(h: HWND) -> Option<(i32, i32)> {
    let mut p = POINT::default();
    unsafe { ClientToScreen(h, &mut p).ok().ok()? };
    Some((p.x, p.y))
}

/// The undocumented `GetWindowBand`, looked up at run time. `None` when it is not
/// there (older Windows) or fails.
pub fn window_band(h: HWND) -> Option<u32> {
    type GetWindowBand = unsafe extern "system" fn(HWND, *mut u32) -> BOOL;
    static PROC: std::sync::OnceLock<Option<GetWindowBand>> = std::sync::OnceLock::new();
    let f = PROC.get_or_init(|| unsafe {
        let m = GetModuleHandleW(w!("user32.dll")).ok()?;
        let p = GetProcAddress(m, s!("GetWindowBand"))?;
        Some(std::mem::transmute::<_, GetWindowBand>(p))
    });
    let f = (*f)?;
    let mut band = 0;
    unsafe { f(h, &mut band).as_bool().then_some(band) }
}

/// Whether another process may create a child inside the taskbar right now.
pub fn accepts_children(tray: HWND) -> bool {
    window_band(tray).map(|b| b == ZBID_DESKTOP).unwrap_or(true)
}

/// Taskbar, monitor and work area, for `layout::place`.
pub fn screen(tray: Option<HWND>) -> Screen {
    unsafe {
        let mon = match tray {
            Some(t) => MonitorFromWindow(t, MONITOR_DEFAULTTOPRIMARY),
            None => MonitorFromPoint(POINT { x: 0, y: 0 }, MONITOR_DEFAULTTOPRIMARY),
        };
        let mut mi = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
        let _ = GetMonitorInfoW(mon, &mut mi);
        let (mut dx, mut dy) = (96u32, 96u32);
        let _ = GetDpiForMonitor(mon, MDT_EFFECTIVE_DPI, &mut dx, &mut dy);
        Screen {
            monitor: rect(mi.rcMonitor),
            work: rect(mi.rcWork),
            taskbar: tray.and_then(window_rect),
            scale: dx.max(48) as f64 / 96.0,
        }
    }
}

/// True while a full-screen app (video, game, F11 browser) is in front on the
/// taskbar's monitor, the situation in which the shell puts the taskbar away.
pub fn full_screen_app_in_front(tray: HWND) -> bool {
    unsafe {
        let fg = GetForegroundWindow();
        if fg.is_invalid() || fg == tray {
            return false;
        }
        if matches!(
            class_name(fg).as_str(),
            "Progman" | "WorkerW" | "Shell_TrayWnd" | "Shell_SecondaryTrayWnd" | "XamlExplorerHostIslandWindow" | "Windows.UI.Core.CoreWindow"
        ) {
            return false;
        }
        let Some(r) = window_rect(fg) else { return false };
        let mut mi = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
        if !GetMonitorInfoW(MonitorFromWindow(tray, MONITOR_DEFAULTTOPRIMARY), &mut mi).as_bool() {
            return false;
        }
        let m = rect(mi.rcMonitor);
        if r.left > m.left || r.top > m.top || r.right < m.right || r.bottom < m.bottom {
            return false;
        }
        // A maximized, captioned window covers the monitor too when the bar auto-hides.
        let style = GetWindowLongPtrW(fg, GWL_STYLE) as u32;
        !((style & WS_CAPTION.0) == WS_CAPTION.0 && IsZoomed(fg).as_bool())
    }
}

/// The foreground window is one of the shell's own surfaces (or nothing): the
/// states in which the taskbar is lifted out of the normal band.
pub fn shell_has_focus() -> bool {
    let fg = unsafe { GetForegroundWindow() };
    fg.is_invalid() || matches!(class_name(fg).as_str(), "Progman" | "WorkerW" | "Shell_TrayWnd" | "Shell_SecondaryTrayWnd")
}
