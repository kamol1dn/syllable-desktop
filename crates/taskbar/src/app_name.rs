//! The active-app name strip, and the foreground watcher behind it.
//!
//! Event driven: a WinEvent hook fires only when focus moves, so sitting in one app
//! costs nothing. Names are resolved on a worker thread (reading another process's
//! version info can block, and the strip thread shares an input queue with Explorer).

use std::sync::mpsc::{channel, Receiver, Sender};
use std::time::Instant;

use syllable_core::{log, Config};
use windows::core::*;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Direct2D::*;
use windows::Win32::Graphics::DirectWrite::*;
use windows::Win32::Storage::FileSystem::*;
use windows::Win32::System::Threading::*;
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::dock::Dock;
use crate::gfx::{self, Canvas, Factories, TextLayout};
use crate::layout::{self, Screen, StripSpec};
use crate::shell;

pub fn spec(cfg: &Config) -> StripSpec {
    StripSpec {
        placement: cfg.app_name_placement.clone(),
        v_pos: cfg.app_name_v_pos.clone(),
        align: cfg.app_name_align.clone(),
        x_offset: cfg.app_name_x_offset,
        width: cfg.app_name_width,
        preferred_height: (cfg.app_name_font_px * 2.1).max(22.0),
    }
}

/// What a foreground window turned out to be.
#[derive(Clone, Debug, PartialEq)]
pub struct Foreground {
    pub name: String,
    /// The desktop itself (wallpaper surface), not the taskbar.
    pub desktop: bool,
}

/// Resolves foreground windows to app names off the strip thread. `post` wakes the
/// strip thread when an answer is ready.
pub struct Resolver {
    tx: Sender<(u64, isize)>,
    pub results: Receiver<(u64, Foreground)>,
    token: u64,
}

impl Resolver {
    pub fn new(post: impl Fn() + Send + 'static) -> Self {
        let (tx, rx) = channel::<(u64, isize)>();
        let (rtx, results) = channel();
        std::thread::Builder::new()
            .name("taskbar-appname".into())
            .spawn(move || {
                while let Ok(mut job) = rx.recv() {
                    // Only the newest switch matters.
                    while let Ok(newer) = rx.try_recv() {
                        job = newer;
                    }
                    if let Some(fg) = describe(HWND(job.1 as *mut _)) {
                        if rtx.send((job.0, fg)).is_err() {
                            break;
                        }
                        post();
                    }
                }
            })
            .expect("spawn app-name resolver");
        Self { tx, results, token: 0 }
    }

    pub fn resolve(&mut self, hwnd: HWND) {
        if hwnd.is_invalid() {
            return;
        }
        self.token += 1;
        let _ = self.tx.send((self.token, hwnd.0 as isize));
    }

    /// Latest answer, ignoring ones overtaken by a newer switch.
    pub fn take(&self) -> Option<Foreground> {
        let mut last = None;
        while let Ok((tok, fg)) = self.results.try_recv() {
            if tok == self.token {
                last = Some(fg);
            }
        }
        last
    }
}

fn describe(hwnd: HWND) -> Option<Foreground> {
    let mut pid = 0;
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
    if pid == 0 {
        return None;
    }
    let class = shell::class_name(hwnd);
    let path = process_path(pid)?;
    let exe = std::path::Path::new(&path).file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();

    // The desktop and the shell's own surfaces read as "Desktop".
    if exe.eq_ignore_ascii_case("explorer")
        && matches!(class.as_str(), "Progman" | "WorkerW" | "Shell_TrayWnd" | "Shell_SecondaryTrayWnd")
    {
        return Some(Foreground { name: "Desktop".into(), desktop: matches!(class.as_str(), "Progman" | "WorkerW") });
    }
    // Store apps are hosted by ApplicationFrameHost; the app owns a child window.
    if exe.eq_ignore_ascii_case("ApplicationFrameHost") {
        if let Some(path) = hosted_child_pid(hwnd, pid).and_then(process_path) {
            return Some(Foreground { name: friendly_name(&path), desktop: false });
        }
    }
    Some(Foreground { name: friendly_name(&path), desktop: false })
}

fn process_path(pid: u32) -> Option<String> {
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut buf = [0u16; 1024];
        let mut len = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(h, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut len);
        let _ = CloseHandle(h);
        ok.ok()?;
        Some(String::from_utf16_lossy(&buf[..len as usize]))
    }
}

/// The FileDescription ("Google Chrome") when there is one, else the exe name.
fn friendly_name(path: &str) -> String {
    if let Some(desc) = file_description(path) {
        let desc = desc.trim();
        // Some apps (modern Notepad) put the bare file name there.
        let desc = desc.strip_suffix(".exe").or_else(|| desc.strip_suffix(".EXE")).unwrap_or(desc).trim();
        if !desc.is_empty() {
            return desc.to_string();
        }
    }
    let stem = std::path::Path::new(path).file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let mut c = stem.chars();
    match c.next() {
        Some(first) => first.to_uppercase().chain(c).collect(),
        None => stem,
    }
}

fn file_description(path: &str) -> Option<String> {
    let wpath = HSTRING::from(path);
    unsafe {
        let size = GetFileVersionInfoSizeW(&wpath, None);
        if size == 0 {
            return None;
        }
        let mut data = vec![0u8; size as usize];
        GetFileVersionInfoW(&wpath, None, size, data.as_mut_ptr().cast()).ok()?;
        let mut ptr = std::ptr::null_mut();
        let mut len = 0u32;
        // First translation (language, code page) the file declares.
        let lang = if VerQueryValueW(data.as_ptr().cast(), w!("\\VarFileInfo\\Translation"), &mut ptr, &mut len).as_bool()
            && len >= 4
        {
            let pair = std::slice::from_raw_parts(ptr as *const u16, 2);
            format!("{:04x}{:04x}", pair[0], pair[1])
        } else {
            "040904b0".to_string()
        };
        let key = HSTRING::from(format!("\\StringFileInfo\\{lang}\\FileDescription"));
        if !VerQueryValueW(data.as_ptr().cast(), &key, &mut ptr, &mut len).as_bool() || len == 0 {
            return None;
        }
        let s = std::slice::from_raw_parts(ptr as *const u16, len as usize);
        let s = String::from_utf16_lossy(s);
        Some(s.trim_end_matches('\0').to_string())
    }
}

fn hosted_child_pid(host: HWND, host_pid: u32) -> Option<u32> {
    struct Find {
        host_pid: u32,
        found: u32,
    }
    unsafe extern "system" fn cb(child: HWND, l: LPARAM) -> BOOL {
        let f = &mut *(l.0 as *mut Find);
        let mut pid = 0;
        GetWindowThreadProcessId(child, Some(&mut pid));
        if pid != 0 && pid != f.host_pid {
            f.found = pid;
            return FALSE;
        }
        TRUE
    }
    let mut f = Find { host_pid, found: 0 };
    unsafe {
        let _ = EnumChildWindows(Some(host), Some(cb), LPARAM(&mut f as *mut Find as isize));
    }
    (f.found != 0).then_some(f.found)
}

// ---- the strip ----

pub struct AppNameStrip {
    dock: Dock,
    canvas: Option<Canvas>,
    brush: Option<ID2D1SolidColorBrush>,
    format: Option<IDWriteTextFormat>,
    fmt_scale: f64,
    text: Option<(String, TextLayout)>,
    name: Option<String>,
    wanted: bool,
    dirty: bool,
    opacity: f32,
    last_frame: Instant,
}

impl AppNameStrip {
    pub fn new() -> Self {
        Self {
            dock: Dock::new("app-name strip"),
            canvas: None,
            brush: None,
            format: None,
            fmt_scale: 0.0,
            text: None,
            name: None,
            wanted: false,
            dirty: true,
            opacity: 0.0,
            last_frame: Instant::now(),
        }
    }

    pub fn is_docked(&self) -> bool {
        self.dock.is_docked()
    }

    pub fn dock_refused(&self) -> bool {
        (self.wanted || self.opacity > 0.0) && self.dock.refused()
    }

    /// Returns whether it changed.
    pub fn set_wanted(&mut self, on: bool) -> bool {
        let changed = on != self.wanted;
        if changed {
            self.wanted = on;
            self.dirty = true;
        }
        changed
    }

    pub fn set_name(&mut self, name: Option<String>) {
        if name != self.name {
            self.name = name;
            self.text = None;
            self.dirty = true;
        }
    }

    pub fn config_changed(&mut self) {
        self.format = None;
        self.brush = None;
        self.text = None;
        self.dirty = true;
    }

    /// Something changed that has not been drawn yet.
    pub fn redraw_pending(&self) -> bool {
        self.dirty && self.dock.has_target() && (self.wanted || self.opacity > 0.0)
    }

    pub fn animating(&self) -> bool {
        self.dock.has_target() && (self.opacity - if self.wanted { 1.0 } else { 0.0 }).abs() > 0.001
    }

    pub fn poll_interval(&self, cfg: &Config, screen: &Screen) -> Option<std::time::Duration> {
        (self.wanted || self.opacity > 0.0).then(|| self.dock.poll_interval(&layout::place(&spec(cfg), screen)))
    }

    pub fn frame(&mut self, f: &Factories, cfg: &Config, tray: Option<HWND>, screen: &Screen, relayout: bool) {
        let now = Instant::now();
        let dt = crate::timeline::frame_dt_ms((now - self.last_frame).as_secs_f32() * 1000.0);
        self.last_frame = now;
        let before = self.opacity;
        self.opacity = crate::timeline::approach(self.opacity, if self.wanted { 1.0 } else { 0.0 }, dt, 180.0);
        if self.opacity <= 0.0 && !self.wanted {
            self.dock.hide();
            return;
        }
        let place = layout::place(&spec(cfg), screen);
        let Some(target) = self.dock.target(tray, place, relayout) else { return };
        let resized = self.canvas.as_ref().is_some_and(|c| (c.width, c.height) != (target.width, target.height));
        if !(self.dirty || target.fresh || resized || before != self.opacity || self.canvas.is_none()) {
            return;
        }
        if let Err(e) = self.draw(f, cfg, screen.scale, target.hwnd, target.width, target.height) {
            log!("taskbar: app-name strip: draw failed ({e}); rebuilding");
            self.canvas = None;
            self.brush = None;
            return;
        }
        self.dirty = false;
    }

    fn draw(&mut self, f: &Factories, cfg: &Config, scale: f64, hwnd: HWND, w: i32, h: i32) -> Result<()> {
        match &mut self.canvas {
            Some(c) => {
                if (c.width, c.height) != (w, h) {
                    c.resize(w, h)?;
                    self.text = None; // ellipsis depends on width
                }
            }
            None => {
                self.canvas = Some(Canvas::new(f, w, h)?);
                self.brush = None;
            }
        }
        let canvas = self.canvas.as_ref().unwrap();
        let rt: &ID2D1RenderTarget = &canvas.rt;
        if self.brush.is_none() {
            self.brush = Some(gfx::solid(rt, gfx::white(1.0))?);
        }
        if self.format.is_none() || self.fmt_scale != scale {
            let fmt = f.text_format(&cfg.font_family, DWRITE_FONT_WEIGHT_BOLD, cfg.app_name_font_px as f32 * scale as f32)?;
            unsafe {
                // Character ellipsis when the name is wider than the strip.
                let sign = f.dwrite.CreateEllipsisTrimmingSign(&fmt)?;
                let trim = DWRITE_TRIMMING { granularity: DWRITE_TRIMMING_GRANULARITY_CHARACTER, delimiter: 0, delimiterCount: 0 };
                fmt.SetTrimming(&trim, &sign)?;
            }
            self.format = Some(fmt);
            self.fmt_scale = scale;
            self.text = None;
        }
        let s = scale as f32;
        let pad = 2.0 * s;
        if self.text.is_none() {
            if let Some(name) = self.name.clone().filter(|n| !n.is_empty()) {
                let mut l = f.layout(&name, self.format.as_ref().unwrap())?;
                let max_w = (w as f32 - 2.0 * pad).max(20.0);
                unsafe {
                    l.layout.SetMaxWidth(max_w)?;
                    let mut m = DWRITE_TEXT_METRICS::default();
                    l.layout.GetMetrics(&mut m)?;
                    l.width = m.width.min(max_w);
                    l.height = m.height;
                }
                self.text = Some((name, l));
            }
        }
        let brush = self.brush.as_ref().unwrap();
        canvas.begin();
        if let Some((_, l)) = &self.text {
            let x = layout::align_x(&cfg.app_name_align, l.width, w as f32, pad);
            let y = (h as f32 - l.height) / 2.0;
            unsafe {
                if cfg.text_shadow {
                    brush.SetColor(&gfx::black(0.5));
                    rt.DrawTextLayout(gfx::v2(x, y + s), &l.layout, brush, D2D1_DRAW_TEXT_OPTIONS_NONE);
                }
                brush.SetColor(&gfx::white(cfg.app_name_alpha as f32 / 255.0));
                rt.DrawTextLayout(gfx::v2(x, y), &l.layout, brush, D2D1_DRAW_TEXT_OPTIONS_NONE);
            }
        }
        canvas.end()?;
        canvas.present(hwnd, self.opacity, None);
        if let Some(dir) = crate::diag::dump_dir() {
            let name = self.name.as_deref().unwrap_or("").chars().filter(|c| c.is_alphanumeric()).collect::<String>();
            let _ = canvas.save_bmp(&dir.join(format!("appname-{name}.bmp")));
            log!("taskbar: app-name strip drew {:?}", self.name);
        }
        Ok(())
    }
}
