//! The audio visualizer: a click-through, per-pixel-alpha strip along the
//! taskbar edge of the primary screen, drawn with Direct2D.
//!
//! Like the prototype it is a top-level window kept at the very bottom of the
//! z-order, so it only shows over the desktop and through a transparent taskbar;
//! the taskbar's icons and the lyrics strip (a child of the taskbar) stay above it,
//! and any app window in front hides it.
//!
//! Live settings (re-read on `Event::ConfigChanged`): `VizEnabled`, `VizPreset`,
//! `VizRandomizeOnTrack`, `VizEdge`, `VizHeightFraction`, `VizOpacity`. Colours
//! come from the cover on `Event::Artwork`.
//!
//! Cost: while enabled it asks `syllable-audio` for audio (`audio::demand`) and
//! repaints at up to 45 fps only when a new audio frame arrived; when the music is
//! silent it polls the audio slot 20 times a second and paints nothing. When
//! disabled the window and GPU resources are destroyed, demand is released and the
//! thread sleeps in `MsgWaitForMultipleObjects` until the next bus event.
//!
//! Debug builds read two environment variables, for testing without the media
//! module or a settings UI:
//! - `SYLLABLE_DEBUG_COVER=<image file>`: use this cover's colours at start (and ignore
//!   `Event::Artwork` while set).
//! - `SYLLABLE_DEBUG_VIZ_CYCLE=<seconds>`: every N seconds change one setting through
//!   `ConfigStore::update` (so it exercises `ConfigChanged`): each preset, opacity,
//!   height, edge top/bottom, off, on; then restore the original values and stop.
//! - `SYLLABLE_DEBUG_VIZ_DUMP=<folder>`: every 2.5 s, save the frame just drawn as a PNG
//!   (composited over grey) and log the audio level: lets a test check the output
//!   without screen capture.

mod decode;
#[cfg(debug_assertions)]
mod png;
pub mod palette;
pub mod preset;
pub mod presets;
mod render;

use std::cell::Cell;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};
use syllable_core::audio::{self, AudioFrame};
use syllable_core::{log, Config, Context, Event};
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{GetMonitorInfoW, MonitorFromPoint, MONITORINFO, MONITOR_DEFAULTTOPRIMARY};
use windows::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::{
    CreateWaitableTimerExW, GetCurrentThreadId, SetWaitableTimer, CREATE_WAITABLE_TIMER_HIGH_RESOLUTION,
    TIMER_ALL_ACCESS, INFINITE,
};
use windows::Win32::UI::HiDpi::{
    GetDpiForMonitor, SetThreadDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, MDT_EFFECTIVE_DPI,
};
use windows::Win32::UI::WindowsAndMessaging::*;

pub use palette::Palette;
pub use preset::{Frame, Painter, Preset};

const FRAME_INTERVAL: Duration = Duration::from_micros(1_000_000 / 45);
/// How often to look for a new audio frame while the music is silent.
const IDLE_POLL: Duration = Duration::from_millis(50);
const MAINTAIN_TIMER: usize = 1;
const MAINTAIN_MS: u32 = 2000;
/// Posted to the UI thread when a bus event or a decoded palette is waiting.
const WM_APP_WAKE: u32 = WM_APP + 1;
const DEMAND_NAME: &str = "visualizer";

pub fn start(ctx: Context) -> Result<()> {
    let bus_events = ctx.bus.subscribe();
    std::thread::Builder::new().name("visualizer".into()).spawn(move || {
        if let Err(e) = run(ctx, bus_events) {
            log!("visualizer: stopped: {e:#}");
        }
        audio::demand(DEMAND_NAME, false);
    })?;
    Ok(())
}

/// What the UI thread is woken for.
enum Wake {
    Bus(Event),
    Palette(u64, Palette),
}

thread_local! {
    /// Set by the window procedure when the screen layout changed.
    static GEOMETRY_DIRTY: Cell<bool> = const { Cell::new(false) };
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct Geometry {
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    taskbar: f32,
    scale: f32,
}

struct Settings {
    enabled: bool,
    preset_id: String,
    randomize: bool,
    top: bool,
    height_fraction: f64,
    opacity: f32,
}

impl Settings {
    fn from(c: &Config) -> Self {
        Self {
            enabled: c.viz_enabled,
            preset_id: c.viz_preset.clone(),
            randomize: c.viz_randomize_on_track,
            top: c.viz_edge == "top",
            height_fraction: c.viz_height_fraction,
            opacity: c.viz_opacity.clamp(0.05, 1.0) as f32,
        }
    }
}

struct Viz {
    ctx: Context,
    wake_tx: Sender<Wake>,
    settings: Settings,
    preset: &'static dyn Preset,
    palette: Palette,
    /// Bumped on every cover, so a slow decode of an old cover is ignored.
    cover_seq: u64,
    hwnd: Option<HWND>,
    renderer: Option<render::Renderer>,
    geometry: Option<Geometry>,
    last_audio: Option<Arc<AudioFrame>>,
    /// Paint the next frame even if the audio did not change (settings, colours, size).
    dirty: bool,
    started: Instant,
    last_paint: Instant,
    track_key: Option<String>,
    #[cfg(debug_assertions)]
    debug_cycle: Option<(Duration, Instant)>,
    #[cfg(debug_assertions)]
    debug_step: usize,
    #[cfg(debug_assertions)]
    debug_original: Option<Config>,
    /// Debug: folder to dump a rendered frame into every few seconds, and when we last did.
    #[cfg(debug_assertions)]
    debug_dump: Option<(std::path::PathBuf, Instant, u32)>,
}

fn run(ctx: Context, bus_events: Receiver<Event>) -> Result<()> {
    unsafe {
        let _ = SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        // Make sure this thread has a message queue before anyone posts to it.
        let mut msg = MSG::default();
        let _ = PeekMessageW(&mut msg, None, WM_USER, WM_USER, PM_NOREMOVE);
    }
    register_class()?;

    let thread_id = unsafe { GetCurrentThreadId() };
    let (wake_tx, wake_rx) = channel::<Wake>();
    {
        let tx = wake_tx.clone();
        std::thread::Builder::new().name("visualizer-bus".into()).spawn(move || {
            for event in bus_events {
                let quit = matches!(event, Event::Quit);
                if tx.send(Wake::Bus(event)).is_err() {
                    break;
                }
                unsafe {
                    let _ = PostThreadMessageW(thread_id, WM_APP_WAKE, WPARAM(0), LPARAM(0));
                }
                if quit {
                    break;
                }
            }
        })?;
    }

    let timer = unsafe {
        CreateWaitableTimerExW(None, PCWSTR::null(), CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, TIMER_ALL_ACCESS.0)
            .or_else(|_| CreateWaitableTimerExW(None, PCWSTR::null(), 0, TIMER_ALL_ACCESS.0))
            .context("frame timer")?
    };

    let settings = Settings::from(&ctx.config.read());
    let preset = presets::get(&settings.preset_id);
    let mut viz = Viz {
        ctx,
        wake_tx,
        settings,
        preset,
        palette: Palette::default(),
        cover_seq: 0,
        hwnd: None,
        renderer: None,
        geometry: None,
        last_audio: None,
        dirty: true,
        started: Instant::now(),
        last_paint: Instant::now(),
        track_key: None,
        #[cfg(debug_assertions)]
        debug_cycle: None,
        #[cfg(debug_assertions)]
        debug_step: 0,
        #[cfg(debug_assertions)]
        debug_original: None,
        #[cfg(debug_assertions)]
        debug_dump: None,
    };
    #[cfg(debug_assertions)]
    viz.debug_setup();
    viz.apply_enabled();

    loop {
        let wait = viz.next_wait();
        unsafe {
            match wait {
                Some(d) if d >= IDLE_POLL => {
                    // Coarse waits: a plain timeout. The high-resolution timer costs CPU
                    // inside the wait itself, so it is kept for frame pacing only.
                    MsgWaitForMultipleObjectsEx(None, d.as_millis() as u32, QS_ALLINPUT, MWMO_INPUTAVAILABLE);
                }
                Some(d) => {
                    let due = -((d.as_nanos() / 100).max(1) as i64); // relative, 100-ns units
                    let _ = SetWaitableTimer(timer, &due, 0, None, None, false);
                    MsgWaitForMultipleObjectsEx(Some(&[timer]), INFINITE, QS_ALLINPUT, MWMO_INPUTAVAILABLE);
                }
                None => {
                    MsgWaitForMultipleObjectsEx(None, INFINITE, QS_ALLINPUT, MWMO_INPUTAVAILABLE);
                }
            }
            let mut msg = MSG::default();
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                if msg.message == WM_QUIT {
                    viz.disable();
                    return Ok(());
                }
                if msg.message == WM_TIMER && msg.wParam.0 == MAINTAIN_TIMER {
                    GEOMETRY_DIRTY.set(true);
                }
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
        while let Ok(wake) = wake_rx.try_recv() {
            match wake {
                Wake::Bus(Event::Quit) => {
                    viz.disable();
                    return Ok(());
                }
                Wake::Bus(event) => viz.on_event(event),
                Wake::Palette(seq, palette) => {
                    if seq == viz.cover_seq {
                        viz.palette = palette;
                        viz.dirty = true;
                    }
                }
            }
        }
        if GEOMETRY_DIRTY.replace(false) {
            viz.place();
        }
        #[cfg(debug_assertions)]
        viz.debug_tick();
        viz.tick();
    }
}

impl Viz {
    fn on_event(&mut self, event: Event) {
        match event {
            Event::ConfigChanged => {
                let s = Settings::from(&self.ctx.config.read());
                let geometry_changed = s.top != self.settings.top || s.height_fraction != self.settings.height_fraction;
                if s.preset_id != self.settings.preset_id {
                    self.preset = presets::get(&s.preset_id);
                }
                self.settings = s;
                self.apply_enabled();
                if geometry_changed {
                    self.place();
                }
                self.dirty = true;
            }
            Event::Artwork(bytes) => {
                #[cfg(debug_assertions)]
                if std::env::var_os("SYLLABLE_DEBUG_COVER").is_some() {
                    return; // the debug cover stays
                }
                self.set_cover(bytes)
            }
            Event::TrackChanged(track) => {
                let key = track.map(|t| t.key());
                if key.is_some() && key != self.track_key && self.track_key.is_some() && self.settings.randomize {
                    let seed = self.started.elapsed().as_nanos() as u64;
                    self.preset = presets::random_other(self.preset.id(), seed.wrapping_mul(0x9E37_79B9_7F4A_7C15));
                    log!("visualizer: random preset {}", self.preset.id());
                    self.dirty = true;
                }
                if key.is_some() {
                    self.track_key = key;
                }
            }
            _ => {}
        }
    }

    /// Decode the cover off the UI thread; the result comes back as `Wake::Palette`.
    fn set_cover(&mut self, bytes: Option<Arc<Vec<u8>>>) {
        self.cover_seq += 1;
        let seq = self.cover_seq;
        let Some(bytes) = bytes else {
            self.palette = Palette::default();
            self.dirty = true;
            return;
        };
        let tx = self.wake_tx.clone();
        let thread_id = unsafe { GetCurrentThreadId() };
        let _ = std::thread::Builder::new().name("visualizer-cover".into()).spawn(move || {
            let palette = decode::palette_from_image(&bytes);
            if tx.send(Wake::Palette(seq, palette)).is_ok() {
                unsafe {
                    let _ = PostThreadMessageW(thread_id, WM_APP_WAKE, WPARAM(0), LPARAM(0));
                }
            }
        });
    }

    fn apply_enabled(&mut self) {
        if self.settings.enabled && self.hwnd.is_none() {
            if let Err(e) = self.enable() {
                log!("visualizer: could not start: {e:#}");
                self.disable();
            }
        } else if !self.settings.enabled && self.hwnd.is_some() {
            self.disable();
        }
    }

    fn enable(&mut self) -> Result<()> {
        let g = compute_geometry(self.settings.top, self.settings.height_fraction);
        let hwnd = create_window(g)?;
        self.hwnd = Some(hwnd);
        self.geometry = Some(g);
        self.renderer = Some(render::Renderer::new(hwnd, g.w as u32, g.h as u32)?);
        unsafe {
            let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
            send_to_bottom(hwnd);
            SetTimer(Some(hwnd), MAINTAIN_TIMER, MAINTAIN_MS, None);
        }
        audio::demand(DEMAND_NAME, true);
        self.dirty = true;
        log!("visualizer: on ({}, {}x{} at {},{}, taskbar {}px)", self.preset.id(), g.w, g.h, g.x, g.y, g.taskbar);
        Ok(())
    }

    fn disable(&mut self) {
        audio::demand(DEMAND_NAME, false);
        self.renderer = None;
        self.last_audio = None;
        if let Some(hwnd) = self.hwnd.take() {
            unsafe {
                let _ = KillTimer(Some(hwnd), MAINTAIN_TIMER);
                let _ = DestroyWindow(hwnd);
            }
            log!("visualizer: off");
        }
    }

    /// Follow the screen: size, position, DPI, work area; and stay at the bottom.
    fn place(&mut self) {
        let Some(hwnd) = self.hwnd else { return };
        let g = compute_geometry(self.settings.top, self.settings.height_fraction);
        if Some(g) != self.geometry {
            unsafe {
                let _ = SetWindowPos(hwnd, Some(HWND_BOTTOM), g.x, g.y, g.w, g.h, SWP_NOACTIVATE);
            }
            if let Some(r) = &mut self.renderer {
                if let Err(e) = r.resize(g.w as u32, g.h as u32) {
                    log!("visualizer: resize failed: {e:#}");
                    self.renderer = None;
                }
            }
            self.geometry = Some(g);
            self.dirty = true;
        }
        unsafe { send_to_bottom(hwnd) };
    }

    /// How long to sleep: `None` = until a message arrives (disabled).
    fn next_wait(&self) -> Option<Duration> {
        if self.hwnd.is_none() {
            #[cfg(debug_assertions)]
            if let Some((every, last)) = self.debug_cycle {
                return Some(every.saturating_sub(last.elapsed()).max(Duration::from_millis(1)));
            }
            return None;
        }
        let since = self.last_paint.elapsed();
        if self.dirty || self.has_new_audio() {
            Some(FRAME_INTERVAL.saturating_sub(since).max(Duration::from_millis(1)))
        } else {
            Some(IDLE_POLL)
        }
    }

    fn has_new_audio(&self) -> bool {
        audio_changed(&audio::latest(), &self.last_audio)
    }

    fn tick(&mut self) {
        if self.hwnd.is_none() || self.last_paint.elapsed() < FRAME_INTERVAL {
            return;
        }
        let latest = audio::latest();
        let changed = audio_changed(&latest, &self.last_audio);
        if !changed && !self.dirty {
            return;
        }
        if self.renderer.is_none() {
            // Lost device, or a failed resize: build a fresh one.
            let (Some(hwnd), Some(g)) = (self.hwnd, self.geometry) else { return };
            match render::Renderer::new(hwnd, g.w as u32, g.h as u32) {
                Ok(r) => self.renderer = Some(r),
                Err(e) => {
                    log!("visualizer: renderer failed: {e:#}");
                    self.last_paint = Instant::now() + Duration::from_secs(1); // back off
                    return;
                }
            }
        }
        let Some(g) = self.geometry else { return };
        let dt_ms = self.last_paint.elapsed().as_secs_f64().min(0.1) * 1000.0;
        let empty = AudioFrame::default();
        let a = latest.as_deref().unwrap_or(&empty);
        let frame = Frame {
            width: g.w as f32,
            height: g.h as f32,
            taskbar_height: g.taskbar.min(g.h as f32),
            scale: g.scale,
            spectrum: &a.bands,
            level: a.level,
            bass: a.bass,
            palette: &self.palette,
            time_sec: self.started.elapsed().as_secs_f64(),
            dt_ms,
        };
        // With no audio frame at all, clear the surface instead of drawing a resting preset.
        let preset = latest.as_ref().map(|_| self.preset);
        let renderer = self.renderer.as_mut().unwrap();
        #[cfg(debug_assertions)]
        let dumping = match &mut self.debug_dump {
            Some((_, last, _)) if last.elapsed() >= Duration::from_millis(2500) => {
                *last = Instant::now();
                renderer.capture_next = true;
                log!("visualizer: level {:.2} bass {:.2} preset {}", a.level, a.bass, self.preset.id());
                true
            }
            _ => false,
        };
        if let Err(e) = renderer.draw(preset, &frame, self.settings.top, self.settings.opacity) {
            log!("visualizer: draw failed, rebuilding: {e:#}");
            self.renderer = None;
        } else {
            renderer.commit();
        }
        #[cfg(debug_assertions)]
        if dumping {
            self.write_dump();
        }
        self.last_audio = latest;
        self.dirty = false;
        self.last_paint = Instant::now();
    }

    #[cfg(debug_assertions)]
    fn debug_setup(&mut self) {
        if let Some(path) = std::env::var_os("SYLLABLE_DEBUG_COVER") {
            match std::fs::read(&path) {
                Ok(bytes) => {
                    log!("visualizer: debug cover {}", std::path::Path::new(&path).display());
                    self.set_cover(Some(Arc::new(bytes)));
                }
                Err(e) => log!("visualizer: debug cover unreadable: {e}"),
            }
        }
        if let Some(dir) = std::env::var_os("SYLLABLE_DEBUG_VIZ_DUMP") {
            let dir = std::path::PathBuf::from(dir);
            let _ = std::fs::create_dir_all(&dir);
            self.debug_dump = Some((dir, Instant::now(), 0));
        }
        if let Some(secs) = std::env::var("SYLLABLE_DEBUG_VIZ_CYCLE").ok().and_then(|s| s.parse::<f64>().ok()) {
            if secs > 0.0 {
                self.debug_cycle = Some((Duration::from_secs_f64(secs), Instant::now()));
            }
        }
    }

    #[cfg(debug_assertions)]
    fn write_dump(&mut self) {
        let Some(r) = self.renderer.as_mut() else { return };
        let Some(bgra) = r.captured.take() else { return };
        let (w, h) = r.size();
        let Some((dir, _, n)) = &mut self.debug_dump else { return };
        *n += 1;
        let path = dir.join(format!("{:03}-{}.png", n, self.preset.id()));
        // Composite over a mid-grey "desktop" so transparent parts read as background.
        let png = png::encode_rgb(w, h, &png::bgra_over(&bgra, (40, 44, 52)));
        if let Err(e) = std::fs::write(&path, png) {
            log!("visualizer: dump failed: {e}");
        }
    }

    #[cfg(debug_assertions)]
    fn debug_tick(&mut self) {
        let Some((every, last)) = self.debug_cycle else { return };
        if last.elapsed() < every {
            return;
        }
        self.debug_cycle = Some((every, Instant::now()));
        // Walk every live setting once through `ConfigStore::update`, then put them back.
        let step = self.debug_step;
        self.debug_step += 1;
        if step == 0 {
            self.debug_original = Some(self.ctx.config.snapshot());
        }
        let all = presets::ALL;
        let n = all.len();
        let i = all.iter().position(|p| p.id() == self.preset.id()).unwrap_or(0);
        let next = all[(i + 1) % n].id().to_string();
        let (what, change): (String, Box<dyn FnOnce(&mut Config)>) = match step {
            s if s < n => (format!("preset {next}"), Box::new(move |c| c.viz_preset = next)),
            s if s == n => ("opacity 1.0".into(), Box::new(|c| c.viz_opacity = 1.0)),
            s if s == n + 1 => ("height 0.5".into(), Box::new(|c| c.viz_height_fraction = 0.5)),
            s if s == n + 2 => ("edge top".into(), Box::new(|c| c.viz_edge = "top".into())),
            s if s == n + 3 => ("edge bottom".into(), Box::new(|c| c.viz_edge = "bottom".into())),
            s if s == n + 4 => ("enabled false".into(), Box::new(|c| c.viz_enabled = false)),
            s if s == n + 5 => ("(still off)".into(), Box::new(|_| {})),
            s if s == n + 6 => ("enabled true".into(), Box::new(|c| c.viz_enabled = true)),
            _ => {
                let o = self.debug_original.take().unwrap_or_default();
                self.debug_cycle = None;
                (
                    "restore and stop".into(),
                    Box::new(move |c| {
                        c.viz_enabled = o.viz_enabled;
                        c.viz_preset = o.viz_preset;
                        c.viz_edge = o.viz_edge;
                        c.viz_height_fraction = o.viz_height_fraction;
                        c.viz_opacity = o.viz_opacity;
                    }),
                )
            }
        };
        log!("visualizer: debug cycle -> {what}");
        self.ctx.config.update(change);
    }
}

/// Whether a new audio frame needs painting: a different frame, unless both are silence.
fn audio_changed(latest: &Option<Arc<AudioFrame>>, drawn: &Option<Arc<AudioFrame>>) -> bool {
    match (latest, drawn) {
        (Some(a), Some(b)) => !Arc::ptr_eq(a, b) && !(is_silent(a) && is_silent(b)),
        (None, None) => false,
        _ => true,
    }
}

fn is_silent(f: &AudioFrame) -> bool {
    f.level < 0.004 && f.bass < 0.004 && f.bands.iter().all(|&b| b < 0.004)
}

fn compute_geometry(top: bool, fraction: f64) -> Geometry {
    unsafe {
        let monitor = MonitorFromPoint(POINT { x: 0, y: 0 }, MONITOR_DEFAULTTOPRIMARY);
        let mut info = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
        let (screen, work) = if GetMonitorInfoW(monitor, &mut info).as_bool() {
            (info.rcMonitor, info.rcWork)
        } else {
            let r = RECT { left: 0, top: 0, right: 1920, bottom: 1080 };
            (r, r)
        };
        let (mut dx, mut dy) = (96u32, 96u32);
        let _ = GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut dx, &mut dy);
        let scale = dx as f32 / 96.0;
        let screen_h = screen.bottom - screen.top;
        let h = ((screen_h as f64 * fraction.clamp(0.15, 0.6)) as i32).max((80.0 * scale) as i32).min(screen_h);
        let taskbar = if top { work.top - screen.top } else { screen.bottom - work.bottom };
        // Auto-hidden taskbar, or the taskbar sits on another edge: still paint a strip.
        let taskbar = if taskbar < 6 { (48.0 * scale).min(h as f32) } else { taskbar as f32 };
        Geometry {
            x: screen.left,
            y: if top { screen.top } else { screen.bottom - h },
            w: screen.right - screen.left,
            h,
            taskbar,
            scale,
        }
    }
}

const CLASS_NAME: PCWSTR = w!("SyllableVisualizer");

fn register_class() -> Result<()> {
    unsafe {
        let instance = GetModuleHandleW(None)?;
        let class = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(wndproc),
            hInstance: instance.into(),
            lpszClassName: CLASS_NAME,
            ..Default::default()
        };
        if RegisterClassExW(&class) == 0 {
            anyhow::bail!("RegisterClassExW failed: {}", windows::core::Error::from_thread());
        }
    }
    Ok(())
}

fn create_window(g: Geometry) -> Result<HWND> {
    unsafe {
        let instance = GetModuleHandleW(None)?;
        // Layered + transparent: clicks fall through to whatever is underneath.
        // No redirection bitmap: the content is a DirectComposition swap chain.
        let ex = WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_NOREDIRECTIONBITMAP;
        let hwnd = CreateWindowExW(
            ex,
            CLASS_NAME,
            w!("Syllable visualizer"),
            WS_POPUP,
            g.x,
            g.y,
            g.w,
            g.h,
            None,
            None,
            Some(instance.into()),
            None,
        )?;
        SetLayeredWindowAttributes(hwnd, windows::Win32::Foundation::COLORREF(0), 255, LWA_ALPHA)?;
        Ok(hwnd)
    }
}

unsafe fn send_to_bottom(hwnd: HWND) {
    let _ = SetWindowPos(hwnd, Some(HWND_BOTTOM), 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_NCHITTEST => LRESULT(HTTRANSPARENT as isize),
        WM_MOUSEACTIVATE => LRESULT(MA_NOACTIVATE as isize),
        WM_WINDOWPOSCHANGING => {
            // Whoever moves us in the z-order, we go back to the bottom.
            let pos = lparam.0 as *mut WINDOWPOS;
            if let Some(pos) = pos.as_mut() {
                if pos.flags & SWP_NOZORDER == SET_WINDOW_POS_FLAGS(0) {
                    pos.hwndInsertAfter = HWND_BOTTOM;
                }
            }
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        WM_DISPLAYCHANGE | WM_DPICHANGED | WM_SETTINGCHANGE => {
            GEOMETRY_DIRTY.set(true);
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

