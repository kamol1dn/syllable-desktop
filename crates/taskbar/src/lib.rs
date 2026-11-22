//! Taskbar overlays: the lyrics strip and the active-app name, docked into the taskbar.
//!
//! Everything visual lives on one dedicated thread ("taskbar-ui") with its own
//! message loop. A child window of another process's window shares an input queue
//! with it, so while this thread is not pumping, the taskbar itself stops reacting
//! to clicks: nothing here may block. Bus events reach the thread through a bridge
//! thread that posts a message; app names are resolved on a worker.
//!
//! - `dock`: hosting a strip as a layered child of `Shell_TrayWnd` (or floating).
//! - `shell`: the taskbar's handle, bounds, z-band, full-screen detection.
//! - `layout`, `timeline`: pure placement and lyric-timing logic (unit-tested).
//! - `gfx`: Direct2D/DirectWrite into a DIB, presented with `UpdateLayeredWindow`.
//! - `lyric_strip`, `app_name`: the two strips.
//! - `demo` (debug builds): `SYLLABLE_DEMO=1` publishes a fake track and lyrics.
//! - `diag`: `SYLLABLE_TASKBAR_STATS` / `SYLLABLE_TASKBAR_DUMP` diagnostics.

mod app_name;
mod diag;
mod dock;
mod gfx;
pub mod layout;
mod lyric_strip;
mod shell;
pub mod timeline;

/// Test source. Always compiled (the `demo` example uses it in release builds to
/// measure cost), but the app only switches it on in debug builds.
#[doc(hidden)]
pub mod demo;

use std::cell::RefCell;
use std::collections::VecDeque;
use std::sync::mpsc::{channel, Receiver};
use std::time::{Duration, Instant};

use syllable_core::{log, position, Config, Context, Event};
use windows::core::*;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Dwm::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Performance::*;
use windows::Win32::System::Threading::*;
use windows::Win32::UI::Accessibility::*;
use windows::Win32::UI::HiDpi::*;
use windows::Win32::UI::WindowsAndMessaging::*;

use app_name::{AppNameStrip, Resolver};
use lyric_strip::LyricStrip;

const WM_BUS: u32 = WM_APP + 1;
const WM_RESOLVED: u32 = WM_APP + 2;

/// Start the module. Returns once it is running: long-lived work goes on the
/// module's own threads, which stop when they see `Event::Quit` on the bus.
pub fn start(ctx: Context) -> anyhow::Result<()> {
    let events = ctx.bus.subscribe();
    let (ready_tx, ready_rx) = channel::<isize>();
    let (fwd_tx, fwd_rx) = channel::<Event>();
    let ui_ctx = ctx.clone();
    std::thread::Builder::new().name("taskbar-ui".into()).spawn(move || {
        if let Err(e) = run(ui_ctx, fwd_rx, ready_tx) {
            log!("taskbar: strip thread stopped: {e:#}");
        }
    })?;
    // The bridge: bus events -> the strip thread's queue, plus a wake-up message.
    std::thread::Builder::new().name("taskbar-bus".into()).spawn(move || {
        let Ok(hwnd) = ready_rx.recv() else { return };
        while let Ok(ev) = events.recv() {
            let quit = matches!(ev, Event::Quit);
            if fwd_tx.send(ev).is_err() {
                break;
            }
            unsafe {
                let _ = PostMessageW(Some(HWND(hwnd as *mut _)), WM_BUS, WPARAM(0), LPARAM(0));
            }
            if quit {
                break;
            }
        }
    })?;

    #[cfg(debug_assertions)]
    if std::env::var("SYLLABLE_DEMO").is_ok_and(|v| !v.is_empty() && v != "0") {
        demo::spawn(ctx.clone());
    }
    Ok(())
}

/// Things the window procedures and WinEvent hooks noticed, handled by the loop.
/// (They may run re-entrantly inside `SetWindowPos`/`DestroyWindow`, so they never
/// touch the strips directly.)
#[derive(Debug)]
enum Signal {
    TaskbarCreated,
    TaskbarMoved,
    Foreground(isize),
    Display,
    Wake,
}

thread_local! {
    static SIGNALS: RefCell<VecDeque<Signal>> = const { RefCell::new(VecDeque::new()) };
    static TASKBAR_CREATED: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
    static TRAY: std::cell::Cell<isize> = const { std::cell::Cell::new(0) };
}

fn signal(s: Signal) {
    diag::signal(match s {
        Signal::TaskbarCreated => 0,
        Signal::TaskbarMoved => 1,
        Signal::Foreground(_) => 2,
        Signal::Display => 3,
        Signal::Wake => 4,
    });
    SIGNALS.with(|q| q.borrow_mut().push_back(s));
}

unsafe extern "system" fn listener_proc(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    let created = TASKBAR_CREATED.with(|c| c.get());
    match msg {
        _ if created != 0 && msg == created => signal(Signal::TaskbarCreated),
        WM_DISPLAYCHANGE | WM_SETTINGCHANGE | WM_DPICHANGED => signal(Signal::Display),
        WM_BUS | WM_RESOLVED => signal(Signal::Wake),
        _ => return DefWindowProcW(hwnd, msg, w, l),
    }
    LRESULT(0)
}

unsafe extern "system" fn on_foreground(_: HWINEVENTHOOK, _: u32, hwnd: HWND, id_object: i32, _: i32, _: u32, _: u32) {
    if id_object == OBJID_WINDOW.0 && !hwnd.is_invalid() {
        signal(Signal::Foreground(hwnd.0 as isize));
    }
}

unsafe extern "system" fn on_location(_: HWINEVENTHOOK, _: u32, hwnd: HWND, id_object: i32, _: i32, _: u32, _: u32) {
    if id_object == OBJID_WINDOW.0 && hwnd.0 as isize == TRAY.with(|t| t.get()) {
        signal(Signal::TaskbarMoved);
    }
}

struct State {
    ctx: Context,
    cfg: Config,
    f: gfx::Factories,
    lyrics: LyricStrip,
    app: AppNameStrip,
    resolver: Resolver,
    tray: Option<HWND>,
    move_hook: Option<HWINEVENTHOOK>,
    /// The lyrics wallpaper is in view on the main screen (`syllable_core::wallpaper_view`).
    wallpaper_in_view: bool,
    track_key: Option<String>,
    /// Layout pass wanted now (an event moved something).
    relayout: bool,
    last_layout: Instant,
    /// A focus change may have put a full-screen app over the taskbar (or removed it).
    cover_check: bool,
    /// When the next animation frame is due (paced to the display, see `until_next_vblank`).
    next_frame: Instant,
    unblock: Unblock,
}

fn run(ctx: Context, events: Receiver<Event>, ready: std::sync::mpsc::Sender<isize>) -> anyhow::Result<()> {
    unsafe {
        // The taskbar is per-monitor-v2 aware; a child must match it, and all our
        // geometry is in physical pixels. Thread-scoped, so other modules' windows
        // keep whatever the process declares.
        let _ = SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
    dock::register_class()?;
    let listener = create_listener()?;
    let _ = ready.send(listener.0 as isize);

    let fg_hook = unsafe {
        SetWinEventHook(
            EVENT_SYSTEM_FOREGROUND,
            EVENT_SYSTEM_FOREGROUND,
            None,
            Some(on_foreground),
            0,
            0,
            WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS,
        )
    };

    let post_hwnd = listener.0 as isize;
    let mut st = State {
        cfg: ctx.config.snapshot(),
        ctx,
        f: gfx::Factories::new()?,
        lyrics: LyricStrip::new(),
        app: AppNameStrip::new(),
        resolver: Resolver::new(move || unsafe {
            let _ = PostMessageW(Some(HWND(post_hwnd as *mut _)), WM_RESOLVED, WPARAM(0), LPARAM(0));
        }),
        tray: None,
        move_hook: None,
        wallpaper_in_view: syllable_core::wallpaper_view::in_view(),
        track_key: None,
        relayout: true,
        last_layout: Instant::now(),
        next_frame: Instant::now(),
        cover_check: true,
        unblock: Unblock::new(),
    };
    st.find_taskbar();
    st.resolver.resolve(unsafe { GetForegroundWindow() });
    st.apply_wanted();
    log!(
        "taskbar: started (taskbar {:?}, band {:?}, foreground {})",
        st.tray.map(|t| t.0),
        st.tray.and_then(shell::window_band),
        shell::class_name(unsafe { GetForegroundWindow() })
    );

    let timer = unsafe {
        CreateWaitableTimerExW(None, None, CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, TIMER_ALL_ACCESS.0)
            .or_else(|_| CreateWaitableTimerExW(None, None, 0, TIMER_ALL_ACCESS.0))?
    };

    let mut stats = diag::Stats::new();
    'outer: loop {
        // Pump everything queued.
        let mut msg = MSG::default();
        while unsafe { PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE) }.as_bool() {
            if msg.message == WM_QUIT {
                break 'outer;
            }
            unsafe {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
        while let Ok(ev) = events.try_recv() {
            if matches!(ev, Event::Quit) {
                break 'outer;
            }
            st.on_event(ev);
        }
        let signals: Vec<Signal> = SIGNALS.with(|q| q.borrow_mut().drain(..).collect());
        for s in signals {
            st.on_signal(s);
        }
        if let Some(fg) = st.resolver.take() {
            st.on_foreground_resolved(fg);
        }

        st.tick();
        stats.looped();

        // Sleep until the next frame (display refresh) while animating, until the
        // next maintenance pass otherwise, or until a message arrives.
        let wait = st.next_wake();
        unsafe {
            let handles = [timer];
            match wait {
                Some(d) => {
                    let due = -((d.as_nanos() / 100).max(1) as i64);
                    let _ = SetWaitableTimer(timer, &due, 0, None, None, false);
                    MsgWaitForMultipleObjectsEx(Some(&handles), INFINITE, QS_ALLINPUT, MWMO_INPUTAVAILABLE);
                }
                None => {
                    MsgWaitForMultipleObjectsEx(None, INFINITE, QS_ALLINPUT, MWMO_INPUTAVAILABLE);
                }
            }
        }
    }

    unsafe {
        if !fg_hook.is_invalid() {
            let _ = UnhookWinEvent(fg_hook);
        }
        if let Some(h) = st.move_hook.take() {
            let _ = UnhookWinEvent(h);
        }
        let _ = CloseHandle(timer);
    }
    st.unblock.finish();
    drop(st); // destroys the strip windows
    unsafe {
        let _ = DestroyWindow(listener);
    }
    log!("taskbar: stopped");
    Ok(())
}

fn create_listener() -> Result<HWND> {
    unsafe {
        let inst = GetModuleHandleW(None)?;
        let class = w!("Syllable.TaskbarListener");
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(listener_proc),
            hInstance: inst.into(),
            lpszClassName: class,
            ..Default::default()
        };
        RegisterClassExW(&wc);
        TASKBAR_CREATED.with(|c| c.set(RegisterWindowMessageW(w!("TaskbarCreated"))));
        // "TaskbarCreated" is broadcast to top-level windows only (not message-only
        // ones), and the strips die with the taskbar: so a hidden top-level window of
        // our own that outlives Explorer listens for it.
        CreateWindowExW(
            WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
            class,
            w!("Syllable taskbar listener"),
            WS_POPUP,
            0,
            0,
            0,
            0,
            None,
            None,
            Some(inst.into()),
            None,
        )
    }
}

impl State {
    fn find_taskbar(&mut self) {
        let tray = shell::find_taskbar();
        if tray == self.tray && self.move_hook.is_some() {
            return;
        }
        if let Some(h) = self.move_hook.take() {
            unsafe {
                let _ = UnhookWinEvent(h);
            }
        }
        self.tray = tray;
        TRAY.with(|t| t.set(tray.map(|h| h.0 as isize).unwrap_or(0)));
        if let Some(t) = tray {
            let mut pid = 0;
            let tid = unsafe { GetWindowThreadProcessId(t, Some(&mut pid)) };
            if tid != 0 {
                // Scoped to the taskbar's thread: system-wide LOCATIONCHANGE fires on
                // every cursor move.
                let h = unsafe {
                    SetWinEventHook(
                        EVENT_OBJECT_LOCATIONCHANGE,
                        EVENT_OBJECT_LOCATIONCHANGE,
                        None,
                        Some(on_location),
                        pid,
                        tid,
                        WINEVENT_OUTOFCONTEXT,
                    )
                };
                self.move_hook = (!h.is_invalid()).then_some(h);
            }
        }
        self.relayout = true;
    }

    fn apply_wanted(&mut self) {
        let c = &self.cfg;
        // "Hide while the desktop is showing" only when the lyrics wallpaper is really
        // there to show them instead: with the wallpaper off (or covered) the strip stays.
        let a = self.lyrics.set_wanted(c.lyrics_enabled && !(c.lyrics_hide_on_desktop && self.wallpaper_in_view));
        let b = self.app.set_wanted(c.app_name_enabled);
        if a || b {
            self.relayout = true; // a strip coming back needs its window placed again
        }
    }

    fn on_event(&mut self, ev: Event) {
        match ev {
            Event::Lyrics(l) => self.lyrics.set_lyrics(l),
            Event::TrackChanged(t) => {
                let key = t.as_ref().map(|t| t.key());
                if key != self.track_key {
                    // The lyrics belong to the old song; new ones follow on the bus.
                    self.track_key = key;
                    self.lyrics.set_lyrics(None);
                }
            }
            Event::WallpaperInView(v) => {
                if v != self.wallpaper_in_view {
                    self.wallpaper_in_view = v;
                    if self.cfg.lyrics_hide_on_desktop && self.cfg.lyrics_enabled {
                        log!("taskbar: lyrics wallpaper {}", if v { "in view, fading the lyrics out" } else { "out of view, showing the lyrics" });
                    }
                    self.apply_wanted();
                }
            }
            Event::ConfigChanged => {
                let cfg = self.ctx.config.snapshot();
                if cfg != self.cfg {
                    self.cfg = cfg;
                    self.lyrics.config_changed();
                    self.app.config_changed();
                    self.apply_wanted();
                    self.relayout = true;
                }
            }
            _ => {}
        }
    }

    fn on_signal(&mut self, s: Signal) {
        match s {
            Signal::TaskbarCreated => {
                log!("taskbar: TaskbarCreated (Explorer started or restarted)");
                self.find_taskbar();
                self.unblock.rearm("taskbar re-created");
            }
            Signal::TaskbarMoved | Signal::Display => self.relayout = true,
            Signal::Foreground(h) => {
                self.resolver.resolve(HWND(h as *mut _));
                // The band may have changed (a waiting strip may dock now), or a window
                // went full screen over a floating strip. Docked strips need nothing.
                if !self.all_docked() {
                    self.relayout = true;
                }
                self.cover_check = true;
            }
            Signal::Wake => {}
        }
    }

    fn on_foreground_resolved(&mut self, fg: app_name::Foreground) {
        self.app.set_name(Some(fg.name));
    }

    fn tick(&mut self) {
        // Everything (layout passes included) happens on the frame clock, paced to the
        // display: other wake-ups (hooks, bus events) only queue work for it.
        let now = Instant::now();
        if now < self.next_frame {
            return;
        }
        if self.tray.is_none_or(|t| !shell::is_window(t)) {
            self.find_taskbar();
        }
        let screen = shell::screen(self.tray);
        let poll = self.poll_interval(&screen);
        let relayout = self.relayout || poll.is_some_and(|p| now - self.last_layout >= p);
        if relayout {
            self.relayout = false;
            self.last_layout = now;
        }

        if relayout || std::mem::take(&mut self.cover_check) {
            self.lyrics.check_covered();
        }
        let t = position::now_ms().map(|p| p + self.cfg.global_offset_ms as f64);
        self.lyrics.frame(&self.f, &self.cfg, self.tray, &screen, t, relayout);
        self.app.frame(&self.f, &self.cfg, self.tray, &screen, relayout);
        self.next_frame = Instant::now() + until_next_vblank();
        diag::tick(relayout);

        // Launch defect fix: a strip that should be on the taskbar but cannot dock
        // because the shell holds focus.
        // Only after Windows actually refused a dock, never on the band alone.
        let waiting = self.lyrics.dock_refused() || self.app.dock_refused();
        if self.unblock.step(self.tray, waiting) {
            self.relayout = true;
        }
    }

    /// Both strips that should show are docked children (which need no upkeep on
    /// focus changes).
    fn all_docked(&self) -> bool {
        let lyr = !self.cfg.lyrics_enabled || self.lyrics.is_docked();
        let app = !self.cfg.app_name_enabled || self.app.is_docked();
        lyr && app
    }

    fn poll_interval(&self, screen: &layout::Screen) -> Option<Duration> {
        let a = self.lyrics.poll_interval(&self.cfg, screen);
        let b = self.app.poll_interval(&self.cfg, screen);
        match (a, b) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }

    fn next_wake(&self) -> Option<Duration> {
        let screen = shell::screen(self.tray);
        let playing = position::playing();
        if self.relayout
            || self.lyrics.animating(playing)
            || self.lyrics.redraw_pending()
            || self.app.animating()
            || self.app.redraw_pending()
            || self.unblock.busy()
        {
            return Some(self.next_frame.saturating_duration_since(Instant::now()).max(Duration::from_micros(500)));
        }
        let mut wake = self.poll_interval(&screen).map(|p| p.saturating_sub(self.last_layout.elapsed()));
        if let Some(idle) = self.lyrics.idle_poll() {
            wake = Some(wake.map_or(idle, |w| w.min(idle)));
        }
        wake.map(|w| w.max(Duration::from_millis(1)))
    }
}

/// Time until just after the next display refresh, from DWM's composition clock.
fn until_next_vblank() -> Duration {
    unsafe {
        let mut freq = 0i64;
        let mut now = 0i64;
        let _ = QueryPerformanceFrequency(&mut freq);
        let _ = QueryPerformanceCounter(&mut now);
        let mut info = DWM_TIMING_INFO { cbSize: std::mem::size_of::<DWM_TIMING_INFO>() as u32, ..Default::default() };
        let (vblank, period) = if DwmGetCompositionTimingInfo(HWND::default(), &mut info).is_ok() && info.qpcRefreshPeriod > 0 {
            (info.qpcVBlank as i64, info.qpcRefreshPeriod as i64)
        } else {
            (now, freq / 60)
        };
        // Every vblank up to ~60-75 Hz; every 2nd (or 3rd) on faster displays. The
        // sweep gains nothing visible above that, and each frame costs a layered-window
        // update in DWM. `qpcVBlank` is the latest vblank, so step from the next one.
        let period = period.max(1);
        let every = ((freq as f64 / period as f64) / 60.0).round().max(1.0) as i64;
        let since = (now - vblank).rem_euclid(period);
        let until = (period - since) + (every - 1) * period + freq / 1000; // 1 ms after it
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| log!("taskbar: display {:.1} Hz, drawing every {every} vblank(s)", freq as f64 / period as f64));
        Duration::from_nanos((until.max(freq / 1000) as f64 / freq.max(1) as f64 * 1e9) as u64)
    }
}

/// The fix for "the strip sits behind the taskbar from launch until an app is
/// focused". While the taskbar, Start or the desktop has focus, Windows 11 lifts the
/// taskbar into a higher z-band and refuses new children in it; only a docked child
/// is visible there. A process the user just launched may take the foreground, and
/// any app window in the foreground drops the taskbar back to the normal band. So
/// at start-up (and after Explorer restarts) we briefly activate an invisible window
/// of our own, dock the moment the band allows it, then hand the foreground back.
/// Make `hwnd` the foreground window. Windows only lets a process do that while it
/// "received the last input event" (or was just launched by the shell); a program
/// started minutes ago, with the user's last click on the taskbar, has neither. A
/// zero-distance relative mouse move from this process counts as that last input
/// without moving the pointer or pressing anything, after which the request is
/// honoured. Only called while the desktop or taskbar itself holds focus, so no
/// app or open menu loses anything.
unsafe fn take_foreground(hwnd: HWND) -> (bool, &'static str) {
    use windows::Win32::UI::Input::KeyboardAndMouse::*;
    if SetForegroundWindow(hwnd).as_bool() {
        return (true, "ok");
    }
    let nudge = INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 { mi: MOUSEINPUT { dx: 0, dy: 0, dwFlags: MOUSEEVENTF_MOVE, ..Default::default() } },
    };
    SendInput(&[nudge], std::mem::size_of::<INPUT>() as i32);
    if SetForegroundWindow(hwnd).as_bool() && GetForegroundWindow() == hwnd {
        return (true, "ok after input nudge");
    }
    (false, "")
}

struct Unblock {
    /// Attempts left for the current trigger.
    budget: u32,
    helper: Option<HWND>,
    /// Foreground to give back once docked, and when we took it.
    taken: Option<(HWND, Instant)>,
    next_try: Instant,
}

impl Unblock {
    fn new() -> Self {
        Self { budget: 3, helper: None, taken: None, next_try: Instant::now() }
    }

    fn rearm(&mut self, why: &str) {
        log!("taskbar: dock unblock re-armed ({why})");
        self.budget = 3;
        self.next_try = Instant::now() + Duration::from_millis(300);
    }

    fn busy(&self) -> bool {
        self.taken.is_some()
    }

    /// Returns true when the layout should be re-run now.
    fn step(&mut self, tray: Option<HWND>, waiting: bool) -> bool {
        let now = Instant::now();
        if let Some((prev, at)) = self.taken {
            // Docked (or gave up): hand the foreground back.
            if !waiting || now - at > Duration::from_millis(1500) {
                self.give_back(prev, waiting);
            }
            return true;
        }
        let Some(tray) = tray else { return false };
        if !waiting || self.budget == 0 || now < self.next_try {
            return false;
        }
        // Only when the shell itself holds focus (desktop, taskbar, nothing). Never
        // with Start or search open: taking focus would close them under the user.
        if !shell::shell_has_focus() {
            return false;
        }
        self.budget -= 1;
        self.next_try = now + Duration::from_secs(2);
        let Some(helper) = self.helper() else { return false };
        let prev = unsafe { GetForegroundWindow() };
        let (ok, how) = unsafe {
            let _ = ShowWindow(helper, SW_SHOWNA);
            take_foreground(helper)
        };
        log!(
            "taskbar: shell has focus ({}), band {:?}: activating our own window to dock: {}",
            shell::class_name(prev),
            shell::window_band(tray),
            if ok { how } else { "refused (no foreground rights)" }
        );
        if ok {
            self.taken = Some((prev, now));
            true
        } else {
            unsafe {
                let _ = ShowWindow(helper, SW_HIDE);
            }
            false
        }
    }

    fn give_back(&mut self, prev: HWND, still_waiting: bool) {
        self.taken = None;
        let Some(helper) = self.helper else { return };
        unsafe {
            if GetForegroundWindow() == helper && shell::is_window(prev) {
                let _ = SetForegroundWindow(prev);
            }
            let _ = ShowWindow(helper, SW_HIDE);
        }
        log!(
            "taskbar: dock unblock {}; foreground handed back to {}",
            if still_waiting { "did not help" } else { "worked" },
            shell::class_name(prev)
        );
    }

    fn helper(&mut self) -> Option<HWND> {
        if let Some(h) = self.helper.filter(|h| shell::is_window(*h)) {
            return Some(h);
        }
        let h = unsafe {
            let inst = GetModuleHandleW(None).ok()?;
            let h = CreateWindowExW(
                WS_EX_TOOLWINDOW | WS_EX_LAYERED | WS_EX_TRANSPARENT,
                w!("Static"),
                w!("Syllable"),
                WS_POPUP,
                0,
                0,
                1,
                1,
                None,
                None,
                Some(inst.into()),
                None,
            )
            .ok()?;
            // Fully transparent: it is only there to hold the foreground for a moment.
            let _ = SetLayeredWindowAttributes(h, COLORREF(0), 0, LWA_ALPHA);
            h
        };
        self.helper = Some(h);
        Some(h)
    }

    fn finish(&mut self) {
        if let Some(h) = self.helper.take() {
            unsafe {
                let _ = DestroyWindow(h);
            }
        }
    }
}
