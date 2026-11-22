//! Shows the wallpaper page on the desktop, behind the icons: one borderless window
//! with a WebView2 per monitor, parented into Explorer's desktop.
//!
//!   Windows 11 24H2+: Progman -> [SHELLDLL_DefView (icons), WorkerW (static picture)].
//!     Our window is a child of Progman, directly below SHELLDLL_DefView.
//!   Older builds: after message 0x052C the icons live in a top-level WorkerW and the
//!     WorkerW that follows it is the wallpaper layer; our window becomes its child.
//!
//! Everything is reconciled by `Host::sync`, which is idempotent: it runs once a second,
//! when Explorer touches the desktop stack (a WinEvent hook on Explorer's process), on a
//! display change and on `ConfigChanged`. So the windows come back after an Explorer
//! restart (which destroys them along with Progman), a display change or a
//! virtual-desktop switch (Explorer rebuilds its WorkerW on top of us).
//!
//! The windows sit under the icon layer and never receive mouse input, so the desktop
//! icons stay fully clickable. A monitor whose work area is completely covered by
//! another window gets its page paused (`hostSetPaused` in the page) until it shows.
//!
//! All of this runs on the host's own thread. WebView2 calls back on that thread through
//! its message loop; callbacks only queue a `Done` and post `WM_APP_SYNC`, so nothing
//! re-enters `Host` while it is borrowed.

use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::c_void;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
use std::sync::mpsc::Receiver;

use syllable_core::{config::data_dir, log, Context, Event};
use webview2_com::Microsoft::Web::WebView2::Win32::*;
use webview2_com::{
    CoTaskMemPWSTR, CoreWebView2EnvironmentOptions, CreateCoreWebView2ControllerCompletedHandler,
    CreateCoreWebView2EnvironmentCompletedHandler, ExecuteScriptCompletedHandler, NavigationCompletedEventHandler,
    NewWindowRequestedEventHandler, ProcessFailedEventHandler,
};
use windows::core::{w, Interface, BOOL, HRESULT, PCWSTR, PWSTR};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_CLOAKED, DWMWA_EXTENDED_FRAME_BOUNDS};
use windows::Win32::Graphics::Gdi::{
    EnumDisplayMonitors, GetMonitorInfoW, GetStockObject, ScreenToClient, BLACK_BRUSH, HBRUSH, HDC, HMONITOR,
    MONITORINFO, MONITORINFOEXW,
};
use windows::Win32::System::Com::{CoInitializeEx, CoTaskMemFree, CoUninitialize, COINIT_APARTMENTTHREADED};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Accessibility::{SetWinEventHook, UnhookWinEvent, HWINEVENTHOOK};
use windows::Win32::UI::HiDpi::{SetThreadDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2};
use windows::Win32::UI::WindowsAndMessaging::*;

const WM_APP_SYNC: u32 = WM_APP + 1;
const WM_APP_DESKTOP: u32 = WM_APP + 2;
const WM_APP_QUIT: u32 = WM_APP + 3;
const TIMER_TICK: usize = 1;
const TIMER_SETTLE: usize = 2;

const CLASS: PCWSTR = w!("SyllableWallpaperHost");

/// The hidden control window of the host thread (an `HWND` as isize, 0 = none).
static CONTROL: AtomicIsize = AtomicIsize::new(0);
/// Progman as last seen, for the WinEvent filter.
static PROGMAN: AtomicIsize = AtomicIsize::new(0);
static DESKTOP_PENDING: AtomicBool = AtomicBool::new(false);
/// A foreground or minimise/restore event queued a sync that has not run yet.
static SYNC_PENDING: AtomicBool = AtomicBool::new(false);
static QUIT: AtomicBool = AtomicBool::new(false);

// The static WebView2 loader (shipped inside webview2-com-sys, whose build script puts
// it on the link path): nothing extra has to sit beside the exe, and a machine without
// the runtime only fails `GetAvailableCoreWebView2BrowserVersionString`.
#[link(name = "WebView2LoaderStatic", kind = "static")]
extern "system" {
    fn GetAvailableCoreWebView2BrowserVersionString(folder: PCWSTR, version: *mut PWSTR) -> HRESULT;
    fn CreateCoreWebView2EnvironmentWithOptions(
        folder: PCWSTR,
        user_data: PCWSTR,
        options: *mut c_void,
        handler: *mut c_void,
    ) -> HRESULT;
}
#[link(name = "version")]
extern "system" {}
#[link(name = "advapi32")]
extern "system" {}
#[link(name = "shell32")]
extern "system" {}
#[link(name = "ole32")]
extern "system" {}

// The page tells itself apart from a covered monitor only through this hook (app.js).
const PAUSE: &str = "window.hostSetPaused && window.hostSetPaused(true)";
const RESUME: &str = "window.hostSetPaused && window.hostSetPaused(false)";

/// Results of WebView2's asynchronous calls, handed from the callbacks to `Host::sync`.
enum Done {
    Env(Result<ICoreWebView2Environment, String>),
    Controller(u64, Result<ICoreWebView2Controller, String>),
    Navigated(u64, bool, i32),
    ProcessFailed(u64, i32),
}

thread_local! {
    static HOST: RefCell<Option<Host>> = const { RefCell::new(None) };
    static DONE: RefCell<Vec<Done>> = const { RefCell::new(Vec::new()) };
}

fn post(msg: u32) {
    let h = CONTROL.load(Ordering::SeqCst);
    if h != 0 {
        unsafe {
            let _ = PostMessageW(Some(HWND(h as *mut c_void)), msg, WPARAM(0), LPARAM(0));
        }
    }
}

fn done(d: Done) {
    DONE.with(|q| q.borrow_mut().push(d));
    post(WM_APP_SYNC);
}

pub fn start(ctx: Context) {
    let events = ctx.bus.subscribe();
    std::thread::Builder::new().name("wallpaper-host-events".into()).spawn(move || forward(events)).ok();
    std::thread::Builder::new()
        .name("wallpaper-host".into())
        .spawn(move || {
            if let Err(e) = run(ctx) {
                log!("wallpaper host: stopped: {e:#}");
            }
        })
        .ok();
}

/// Bus -> host thread: wake it on config changes, stop it on quit.
fn forward(events: Receiver<Event>) {
    while let Ok(e) = events.recv() {
        match e {
            Event::ConfigChanged => post(WM_APP_SYNC),
            Event::Quit => {
                QUIT.store(true, Ordering::SeqCst);
                post(WM_APP_QUIT);
                break;
            }
            _ => {}
        }
    }
}

fn run(ctx: Context) -> anyhow::Result<()> {
    unsafe {
        // Explorer's windows are per-monitor aware; work in real pixels like they do.
        SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok()?;
        let instance = GetModuleHandleW(None)?.into();
        let class = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: instance,
            lpszClassName: CLASS,
            hbrBackground: HBRUSH(GetStockObject(BLACK_BRUSH).0),
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            ..Default::default()
        };
        RegisterClassW(&class);
        // A hidden top-level window (not message-only): it also gets WM_DISPLAYCHANGE.
        let control = CreateWindowExW(
            WS_EX_TOOLWINDOW,
            CLASS,
            w!("Syllable wallpaper host"),
            WS_POPUP,
            0,
            0,
            0,
            0,
            None,
            None,
            Some(instance),
            None,
        )?;
        CONTROL.store(control.0 as isize, Ordering::SeqCst);
        HOST.with(|h| *h.borrow_mut() = Some(Host::new(ctx)));
        SetTimer(Some(control), TIMER_TICK, 1000, None);
        // Show desktop, focus moves and minimise/restore change whether the wallpaper
        // is in view: look again at once instead of on the next tick.
        let view_hooks = [
            SetWinEventHook(EVENT_SYSTEM_FOREGROUND, EVENT_SYSTEM_FOREGROUND, None, Some(on_view_event), 0, 0, WINEVENT_OUTOFCONTEXT),
            SetWinEventHook(EVENT_SYSTEM_MINIMIZESTART, EVENT_SYSTEM_MINIMIZEEND, None, Some(on_view_event), 0, 0, WINEVENT_OUTOFCONTEXT),
        ];
        post(WM_APP_SYNC);
        if QUIT.load(Ordering::SeqCst) {
            post(WM_APP_QUIT);
        }

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).0 > 0 {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        for h in view_hooks {
            if !h.is_invalid() {
                let _ = UnhookWinEvent(h);
            }
        }
        HOST.with(|h| h.borrow_mut().take());
        CONTROL.store(0, Ordering::SeqCst);
        CoUninitialize();
    }
    Ok(())
}

fn with_host(f: impl FnOnce(&mut Host)) {
    HOST.with(|cell| match cell.try_borrow_mut() {
        Ok(mut guard) => {
            if let Some(h) = guard.as_mut() {
                f(h);
            }
        }
        // Re-entered from inside a sync (a sent message during a cross-thread call): retry later.
        Err(_) => post(WM_APP_SYNC),
    });
}

extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    let is_control = hwnd.0 as isize == CONTROL.load(Ordering::SeqCst);
    match msg {
        WM_MOUSEACTIVATE => return LRESULT(MA_NOACTIVATE as isize),
        WM_TIMER if is_control => {
            if wp.0 == TIMER_SETTLE {
                unsafe {
                    let _ = KillTimer(Some(hwnd), TIMER_SETTLE);
                }
            }
            with_host(Host::sync);
            return LRESULT(0);
        }
        WM_APP_SYNC | WM_DISPLAYCHANGE if is_control => {
            SYNC_PENDING.store(false, Ordering::SeqCst);
            with_host(Host::sync);
            return LRESULT(0);
        }
        WM_APP_DESKTOP if is_control => {
            DESKTOP_PENDING.store(false, Ordering::SeqCst);
            with_host(Host::sync);
            // Explorer builds a new layer over a burst of events: look again once it settles.
            unsafe { SetTimer(Some(hwnd), TIMER_SETTLE, 400, None) };
            return LRESULT(0);
        }
        WM_APP_QUIT if is_control => {
            with_host(|h| {
                h.close_all();
                h.unhook();
            });
            unsafe {
                let _ = DestroyWindow(hwnd);
                PostQuitMessage(0);
            }
            return LRESULT(0);
        }
        _ => {}
    }
    unsafe { DefWindowProcW(hwnd, msg, wp, lp) }
}

unsafe extern "system" fn on_win_event(
    _hook: HWINEVENTHOOK,
    _event: u32,
    hwnd: HWND,
    id_object: i32,
    _id_child: i32,
    _thread: u32,
    _time: u32,
) {
    if id_object != OBJID_WINDOW.0 || hwnd.0.is_null() {
        return;
    }
    let progman = PROGMAN.load(Ordering::SeqCst);
    if progman == 0 {
        return;
    }
    // Only the desktop stack matters: Progman itself (REORDER) or one of its children.
    let parent = unsafe { GetAncestor(hwnd, GA_PARENT) };
    if hwnd.0 as isize != progman && parent.0 as isize != progman {
        return;
    }
    if !DESKTOP_PENDING.swap(true, Ordering::SeqCst) {
        post(WM_APP_DESKTOP);
    }
}

/// Foreground changes and minimise/restore anywhere: one sync per burst.
unsafe extern "system" fn on_view_event(
    _hook: HWINEVENTHOOK,
    _event: u32,
    _hwnd: HWND,
    _id_object: i32,
    _id_child: i32,
    _thread: u32,
    _time: u32,
) {
    if !SYNC_PENDING.swap(true, Ordering::SeqCst) {
        post(WM_APP_SYNC);
    }
}

enum EnvState {
    NotStarted,
    Pending,
    Ready(ICoreWebView2Environment),
}

struct Screen {
    id: u64,
    name: String,
    hwnd: HWND,
    controller: Option<ICoreWebView2Controller>,
    webview: Option<ICoreWebView2>,
    bounds: RECT,
    ready: bool,
    covered: bool,
    paused: bool,
    covered_passes: u32,
}

struct Host {
    ctx: Context,
    env: EnvState,
    /// No runtime, no page, or the browser keeps dying: stay off for this run.
    unavailable: bool,
    browser_failures: u32,
    screens: HashMap<String, Screen>,
    next_id: u64,
    port: u16,
    page: Option<PathBuf>,
    nudged_progman: isize,
    last_target: &'static str,
    hook: HWINEVENTHOOK,
    explorer_pid: u32,
    seen_explorer: bool,
    syncing: bool,
    was_enabled: Option<bool>,
    /// What was last published as `Event::WallpaperInView`.
    in_view: Option<bool>,
}

impl Host {
    fn new(ctx: Context) -> Self {
        let port = ctx.config.read().port;
        Self {
            ctx,
            env: EnvState::NotStarted,
            unavailable: false,
            browser_failures: 0,
            screens: HashMap::new(),
            next_id: 1,
            port,
            page: crate::page_path(),
            nudged_progman: 0,
            last_target: "",
            hook: HWINEVENTHOOK::default(),
            explorer_pid: 0,
            seen_explorer: false,
            syncing: false,
            was_enabled: None,
            in_view: None,
        }
    }

    // ---------------- reconcile ----------------

    fn sync(&mut self) {
        if self.syncing || QUIT.load(Ordering::SeqCst) {
            return;
        }
        self.syncing = true;
        self.drain();
        self.rehook();
        let (enabled, port) = {
            let c = self.ctx.config.read();
            (c.wallpaper_host_enabled, c.port)
        };
        if self.was_enabled != Some(enabled) {
            if self.was_enabled.is_some() {
                log!("wallpaper host: {}", if enabled { "enabled" } else { "disabled" });
            }
            self.was_enabled = Some(enabled);
        }
        if port != self.port {
            log!("wallpaper host: port changed to {port}, reloading pages");
            self.close_all();
            self.port = port;
        }
        if !enabled || self.unavailable {
            self.close_all();
        } else {
            match &self.env {
                EnvState::NotStarted => self.start_env(),
                EnvState::Pending => {}
                EnvState::Ready(env) => {
                    let env = env.clone();
                    self.reconcile(&env);
                }
            }
        }
        self.publish_in_view(enabled && !self.unavailable);
        self.syncing = false;
    }

    /// Whether the lyrics wallpaper is in view on the main screen (where the taskbar
    /// lyrics are): its page is loaded there and the desktop shows. Published on change.
    fn publish_in_view(&mut self, running: bool) {
        let in_view = running
            && monitors().iter().find(|m| m.bounds.left == 0 && m.bounds.top == 0).is_some_and(|mon| {
                self.screens.get(&mon.name).is_some_and(|s| s.ready)
                    && (desktop_in_front() || desktop_clear(&covering_candidates(), &mon.work))
            });
        if self.in_view == Some(in_view) {
            return;
        }
        let first = self.in_view.is_none();
        self.in_view = Some(in_view);
        syllable_core::wallpaper_view::set(in_view);
        if !first || in_view {
            log!("wallpaper host: lyrics wallpaper {} on the main screen", if in_view { "in view" } else { "out of view" });
        }
        self.ctx.bus.publish(Event::WallpaperInView(in_view));
    }

    fn drain(&mut self) {
        let items: Vec<Done> = DONE.with(|q| std::mem::take(&mut *q.borrow_mut()));
        for d in items {
            match d {
                Done::Env(Ok(env)) => {
                    self.env = EnvState::Ready(env);
                }
                Done::Env(Err(e)) => {
                    log!("wallpaper host: WebView2 environment failed ({e}); the desktop wallpaper is off");
                    self.env = EnvState::NotStarted;
                    self.unavailable = true;
                }
                Done::Controller(id, result) => {
                    let Some(s) = self.screens.values_mut().find(|s| s.id == id) else {
                        if let Ok(c) = result {
                            unsafe {
                                let _ = c.Close();
                            }
                        }
                        continue;
                    };
                    if let Err(e) = result.and_then(|c| configure(s, c)) {
                        let why = format!("{} WebView2 failed ({e})", s.name);
                        self.restart(&why);
                    }
                }
                Done::Navigated(id, ok, status) => {
                    if let Some(s) = self.screens.values_mut().find(|s| s.id == id) {
                        if ok {
                            s.ready = true;
                            s.paused = false;
                            apply_pause(s);
                        } else {
                            log!("wallpaper host: {} page load failed (status {status})", s.name);
                        }
                    }
                }
                Done::ProcessFailed(id, kind) => {
                    let Some(s) = self.screens.values_mut().find(|s| s.id == id) else { continue };
                    log!("wallpaper host: {} WebView2 process failed (kind {kind})", s.name);
                    if kind == COREWEBVIEW2_PROCESS_FAILED_KIND_BROWSER_PROCESS_EXITED.0 {
                        self.restart("the browser process exited");
                    } else if kind == COREWEBVIEW2_PROCESS_FAILED_KIND_RENDER_PROCESS_EXITED.0
                        || kind == COREWEBVIEW2_PROCESS_FAILED_KIND_RENDER_PROCESS_UNRESPONSIVE.0
                    {
                        s.ready = false;
                        if let Some(w) = &s.webview {
                            unsafe {
                                let _ = w.Reload();
                            }
                        }
                    }
                }
            }
        }
    }

    /// Start over with a new environment (next sync), a few times per run at most.
    fn restart(&mut self, why: &str) {
        self.browser_failures += 1;
        self.close_all();
        self.env = EnvState::NotStarted;
        if self.browser_failures >= 3 {
            log!("wallpaper host: {why}; failed 3 times, the desktop wallpaper is off for this run");
            self.unavailable = true;
        } else {
            log!("wallpaper host: {why}; starting WebView2 again");
        }
    }

    fn reconcile(&mut self, env: &ICoreWebView2Environment) {
        // Windows that died with Explorer's Progman can't be re-parented; drop them.
        let dead: Vec<String> =
            self.screens.iter().filter(|(_, s)| !unsafe { IsWindow(Some(s.hwnd)) }.as_bool()).map(|(k, _)| k.clone()).collect();
        for key in dead {
            log!("wallpaper host: {key} window was destroyed (Explorer restart?), recreating");
            if let Some(s) = self.screens.remove(&key) {
                discard(s);
            }
        }

        let Some((parent, def_view)) = self.find_desktop() else { return }; // Explorer is (re)starting

        let monitors = monitors();
        let gone: Vec<String> = self.screens.keys().filter(|k| !monitors.iter().any(|m| &m.name == *k)).cloned().collect();
        for key in gone {
            log!("wallpaper host: {key} disconnected");
            if let Some(s) = self.screens.remove(&key) {
                discard(s);
            }
        }

        let covering = covering_candidates();
        // Show desktop leaves windows up (not minimised), only behind the desktop: the
        // wallpaper is in plain sight then, so nothing counts as covering it.
        let shown = desktop_in_front();
        for mon in &monitors {
            if !self.screens.contains_key(&mon.name) {
                match self.create_screen(env, mon, parent, def_view) {
                    Ok(s) => {
                        self.screens.insert(mon.name.clone(), s);
                    }
                    Err(e) => {
                        log!("wallpaper host: {} window failed: {e}", mon.name);
                        continue;
                    }
                }
            }
            let s = self.screens.get_mut(&mon.name).unwrap();
            attach(s, parent, def_view, mon.bounds);
            let by = if shown { None } else { covered_by(&covering, &mon.work) };
            set_covered(s, by);
        }
    }

    fn create_screen(&mut self, env: &ICoreWebView2Environment, mon: &Monitor, parent: HWND, def_view: Option<HWND>) -> anyhow::Result<Screen> {
        let id = self.next_id;
        self.next_id += 1;
        let b = mon.bounds;
        let title: Vec<u16> = format!("Syllable wallpaper {}\0", mon.name).encode_utf16().collect();
        let hwnd = unsafe {
            CreateWindowExW(
                WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
                CLASS,
                PCWSTR(title.as_ptr()),
                WS_POPUP | WS_CLIPCHILDREN,
                b.left,
                b.top,
                b.right - b.left,
                b.bottom - b.top,
                None,
                None,
                Some(GetModuleHandleW(None)?.into()),
                None,
            )?
        };
        let mut s = Screen {
            id,
            name: mon.name.clone(),
            hwnd,
            controller: None,
            webview: None,
            bounds: RECT::default(),
            ready: false,
            covered: false,
            paused: false,
            covered_passes: 0,
        };
        attach(&mut s, parent, def_view, b);
        unsafe {
            let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        }
        let url = page_url(self.page.as_ref().unwrap(), &mon.name, self.port);
        let handler = CreateCoreWebView2ControllerCompletedHandler::create(Box::new(move |hr, controller| {
            let r = hr.map_err(|e| e.to_string()).and_then(|_| controller.ok_or_else(|| "no controller".to_string()));
            let r = r.and_then(|c| {
                navigate(&c, id, &url).map_err(|e| e.to_string())?;
                Ok(c)
            });
            done(Done::Controller(id, r));
            Ok(())
        }));
        unsafe { env.CreateCoreWebView2Controller(hwnd, &handler)? };
        log!(
            "wallpaper host: {} attached, hwnd=0x{:X} {}x{} at {},{}",
            mon.name,
            hwnd.0 as usize,
            b.right - b.left,
            b.bottom - b.top,
            b.left,
            b.top
        );
        Ok(s)
    }

    fn close_all(&mut self) {
        if self.screens.is_empty() {
            return;
        }
        for (_, s) in self.screens.drain() {
            discard(s);
        }
        log!("wallpaper host: windows closed");
    }

    // ---------------- WebView2 environment ----------------

    fn start_env(&mut self) {
        let Some(page) = &self.page else {
            log!("wallpaper host: assets\\wallpaper\\index.html is missing beside the exe; the desktop wallpaper is off");
            self.unavailable = true;
            return;
        };
        if !page.is_file() {
            log!("wallpaper host: {} is missing; the desktop wallpaper is off", page.display());
            self.unavailable = true;
            return;
        }
        let mut version = PWSTR::null();
        let hr = unsafe { GetAvailableCoreWebView2BrowserVersionString(PCWSTR::null(), &mut version) };
        if hr.is_err() || version.is_null() {
            log!("wallpaper host: WebView2 runtime not found ({hr:?}); the desktop wallpaper is off");
            self.unavailable = true;
            return;
        }
        let version_text = unsafe { version.to_string().unwrap_or_default() };
        unsafe { CoTaskMemFree(Some(version.0 as *const c_void)) };

        let options = CoreWebView2EnvironmentOptions::default();
        // The window is a child of the desktop, which confuses Chromium's own "is this
        // window covered?" check; covered monitors are paused by `set_covered`.
        let mut args = String::from("--disable-features=CalculateNativeWinOcclusion --autoplay-policy=no-user-gesture-required");
        if let Some(port) = crate::debug_port() {
            args.push_str(&format!(" --remote-debugging-port={port}"));
        }
        unsafe { options.set_additional_browser_arguments(args) };
        let options: ICoreWebView2EnvironmentOptions = options.into();
        // The default user-data folder is beside the exe, which is read-only once installed.
        // Our own profile: environments sharing a folder must use identical options, and
        // the settings window and Spotify sign-in have theirs (WebView2\settings, ...).
        let profile = if crate::debug_port().is_some() { "wallpaper-debug" } else { "wallpaper" };
        let user_data = data_dir().join("WebView2").join(profile);
        let user_data = CoTaskMemPWSTR::from(user_data.to_string_lossy().as_ref());
        let handler = CreateCoreWebView2EnvironmentCompletedHandler::create(Box::new(move |hr, env| {
            let r = hr.map_err(|e| e.to_string()).and_then(|_| env.ok_or_else(|| "no environment".to_string()));
            done(Done::Env(r));
            Ok(())
        }));
        let hr = unsafe {
            CreateCoreWebView2EnvironmentWithOptions(PCWSTR::null(), *user_data.as_ref().as_pcwstr(), options.as_raw(), handler.as_raw())
        };
        if hr.is_err() {
            log!("wallpaper host: WebView2 environment could not start ({hr:?}); the desktop wallpaper is off");
            self.unavailable = true;
            return;
        }
        log!("wallpaper host: WebView2 runtime {version_text}");
        self.env = EnvState::Pending;
    }

    // ---------------- desktop ----------------

    /// Where the windows go: the parent, and on 24H2+ the icon view to sit under.
    fn find_desktop(&mut self) -> Option<(HWND, Option<HWND>)> {
        unsafe {
            let progman = FindWindowW(w!("Progman"), None).ok().filter(|h| !h.0.is_null())?;
            if progman.0 as isize != self.nudged_progman {
                // Undocumented: makes Progman build its wallpaper layer (the WorkerW).
                SendMessageTimeoutW(progman, 0x052C, WPARAM(0), LPARAM(0), SMTO_NORMAL, 1000, None);
                self.nudged_progman = progman.0 as isize;
            }
            let (parent, def_view, target) = match FindWindowExW(Some(progman), None, w!("SHELLDLL_DefView"), None) {
                Ok(view) if !view.0.is_null() => (progman, Some(view), "Progman, below SHELLDLL_DefView (24H2+ layout)"),
                _ => {
                    // Older: the WorkerW after the top-level window that hosts the icons.
                    let mut icon_host: Option<HWND> = None;
                    let mut worker = None;
                    while let Ok(h) = FindWindowExW(None, icon_host, w!("WorkerW"), None) {
                        if h.0.is_null() {
                            break;
                        }
                        icon_host = Some(h);
                        if FindWindowExW(Some(h), None, w!("SHELLDLL_DefView"), None).is_ok_and(|v| !v.0.is_null()) {
                            worker = FindWindowExW(None, Some(h), w!("WorkerW"), None).ok().filter(|w| !w.0.is_null());
                            break;
                        }
                    }
                    let Some(worker) = worker else {
                        self.nudged_progman = 0; // layout not built yet: ask again next pass
                        return None;
                    };
                    (worker, None, "WorkerW behind the icons (pre-24H2 layout)")
                }
            };
            if target != self.last_target {
                self.last_target = target;
                log!("wallpaper host: desktop target is {target}");
            }
            Some((parent, def_view))
        }
    }

    /// Watch Explorer's desktop windows; re-hook after Explorer restarts (new pid).
    fn rehook(&mut self) {
        unsafe {
            let progman = FindWindowW(w!("Progman"), None).unwrap_or_default();
            PROGMAN.store(progman.0 as isize, Ordering::SeqCst);
            let mut pid = 0u32;
            if !progman.0.is_null() {
                GetWindowThreadProcessId(progman, Some(&mut pid));
            }
            if pid == self.explorer_pid && !self.hook.is_invalid() {
                return;
            }
            self.unhook();
            self.explorer_pid = pid;
            if pid == 0 {
                return; // Explorer is restarting; the next tick tries again
            }
            if self.seen_explorer {
                log!("wallpaper host: Explorer restarted");
            }
            self.seen_explorer = true;
            self.hook = SetWinEventHook(
                EVENT_OBJECT_CREATE,
                EVENT_OBJECT_REORDER,
                None,
                Some(on_win_event),
                pid,
                0,
                WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS,
            );
            if self.hook.is_invalid() {
                log!("wallpaper host: SetWinEventHook failed");
            }
        }
    }

    fn unhook(&mut self) {
        if !self.hook.is_invalid() {
            unsafe {
                let _ = UnhookWinEvent(self.hook);
            }
            self.hook = HWINEVENTHOOK::default();
        }
        self.explorer_pid = 0;
    }
}

/// Settings, handlers and the size, once WebView2 handed us the controller.
fn configure(s: &mut Screen, c: ICoreWebView2Controller) -> Result<(), String> {
    unsafe {
        let webview = c.CoreWebView2().map_err(|e| e.to_string())?;
        if let Ok(c2) = c.cast::<ICoreWebView2Controller2>() {
            let _ = c2.SetDefaultBackgroundColor(COREWEBVIEW2_COLOR { A: 255, R: 0, G: 0, B: 0 });
        }
        let mut rc = RECT::default();
        let _ = GetClientRect(s.hwnd, &mut rc);
        let _ = c.SetBounds(rc);
        s.bounds = rc;
        let _ = c.SetIsVisible(true);
        s.controller = Some(c);
        s.webview = Some(webview);
    }
    Ok(())
}

/// Called inside the controller callback: settings, event handlers, then the page.
fn navigate(c: &ICoreWebView2Controller, id: u64, url: &str) -> windows::core::Result<()> {
    unsafe {
        let webview = c.CoreWebView2()?;
        let settings = webview.Settings()?;
        let debug = crate::debug_port().is_some();
        settings.SetAreDefaultContextMenusEnabled(false)?;
        settings.SetAreDevToolsEnabled(debug)?;
        settings.SetIsStatusBarEnabled(false)?;
        settings.SetIsZoomControlEnabled(false)?;
        if let Ok(s3) = settings.cast::<ICoreWebView2Settings3>() {
            let _ = s3.SetAreBrowserAcceleratorKeysEnabled(false);
        }
        if let Ok(s6) = settings.cast::<ICoreWebView2Settings6>() {
            let _ = s6.SetIsSwipeNavigationEnabled(false);
        }
        let mut token = 0i64;
        // The page is ours and local; nothing should open windows or leave it.
        webview.add_NewWindowRequested(
            &NewWindowRequestedEventHandler::create(Box::new(|_, args| {
                if let Some(a) = args {
                    let _ = a.SetHandled(true);
                }
                Ok(())
            })),
            &mut token,
        )?;
        webview.add_NavigationCompleted(
            &NavigationCompletedEventHandler::create(Box::new(move |_, args| {
                let (mut ok, mut status) = (BOOL(0), COREWEBVIEW2_WEB_ERROR_STATUS(0));
                if let Some(a) = args {
                    let _ = a.IsSuccess(&mut ok);
                    let _ = a.WebErrorStatus(&mut status);
                }
                done(Done::Navigated(id, ok.as_bool(), status.0));
                Ok(())
            })),
            &mut token,
        )?;
        webview.add_ProcessFailed(
            &ProcessFailedEventHandler::create(Box::new(move |_, args| {
                let mut kind = COREWEBVIEW2_PROCESS_FAILED_KIND(0);
                if let Some(a) = args {
                    let _ = a.ProcessFailedKind(&mut kind);
                }
                done(Done::ProcessFailed(id, kind.0));
                Ok(())
            })),
            &mut token,
        )?;
        let url = CoTaskMemPWSTR::from(url);
        webview.Navigate(*url.as_ref().as_pcwstr())?;
    }
    Ok(())
}

/// Close a screen's WebView2 and window. One whose window Explorer already destroyed
/// has a half-torn-down controller; errors from it are expected and ignored.
fn discard(s: Screen) {
    unsafe {
        if let Some(c) = s.controller {
            let _ = c.Close();
        }
        if IsWindow(Some(s.hwnd)).as_bool() {
            let _ = DestroyWindow(s.hwnd);
        }
    }
}

/// Parent, place and z-order one window. Cheap when nothing changed.
fn attach(s: &mut Screen, parent: HWND, def_view: Option<HWND>, bounds: RECT) {
    unsafe {
        let hwnd = s.hwnd;
        if GetAncestor(hwnd, GA_PARENT) != parent {
            let _ = SetParent(hwnd, Some(parent));
            if GetAncestor(hwnd, GA_PARENT) != parent {
                log!("wallpaper host: SetParent failed for {} ({:?})", s.name, windows::core::Error::from_thread());
                return;
            }
        }
        let mut cur = RECT::default();
        let _ = GetWindowRect(hwnd, &mut cur);
        if cur != bounds {
            // Child coordinates are relative to the parent, which starts at the virtual screen's corner.
            let mut pt = POINT { x: bounds.left, y: bounds.top };
            let _ = ScreenToClient(parent, &mut pt);
            let _ = SetWindowPos(
                hwnd,
                None,
                pt.x,
                pt.y,
                bounds.right - bounds.left,
                bounds.bottom - bounds.top,
                SWP_NOZORDER | SWP_NOACTIVATE,
            );
        }
        if let Some(c) = &s.controller {
            let mut rc = RECT::default();
            let _ = GetClientRect(hwnd, &mut rc);
            if rc != s.bounds {
                let _ = c.SetBounds(rc);
                s.bounds = rc;
            }
        }

        let Some(def_view) = def_view else { return }; // old layout: the WorkerW holds nothing else
        // Must be below the icons and above Explorer's static picture (a WorkerW sibling).
        let (mut saw_icons, mut saw_worker) = (false, false);
        let mut h = GetWindow(parent, GW_CHILD).unwrap_or_default();
        while !h.0.is_null() && h != hwnd {
            if h == def_view {
                saw_icons = true;
            } else if class_of(h) == "WorkerW" {
                saw_worker = true;
            }
            h = GetWindow(h, GW_HWNDNEXT).unwrap_or_default();
        }
        if !saw_icons || saw_worker {
            let _ = SetWindowPos(hwnd, Some(def_view), 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
        }
    }
}

/// Pause after two covered passes in a row (a window being dragged past), resume at once.
fn set_covered(s: &mut Screen, by: Option<HWND>) {
    s.covered_passes = if by.is_some() { s.covered_passes + 1 } else { 0 };
    let want = s.covered_passes >= 2;
    if want == s.covered {
        return;
    }
    s.covered = want;
    if want {
        log!("wallpaper host: {} paused (desktop covered by a {} window)", s.name, by.map(class_of).unwrap_or_default());
    } else {
        log!("wallpaper host: {} resumed", s.name);
    }
    apply_pause(s);
}

fn apply_pause(s: &mut Screen) {
    if !s.ready || s.paused == s.covered {
        return;
    }
    let Some(w) = &s.webview else { return };
    s.paused = s.covered;
    let js = CoTaskMemPWSTR::from(if s.paused { PAUSE } else { RESUME });
    let handler = ExecuteScriptCompletedHandler::create(Box::new(|_, _| Ok(())));
    if let Err(e) = unsafe { w.ExecuteScript(*js.as_ref().as_pcwstr(), &handler) } {
        log!("wallpaper host: {} pause script failed: {e}", s.name);
    }
}

// ---------------- monitors ----------------

struct Monitor {
    /// "DISPLAY1"
    name: String,
    bounds: RECT,
    work: RECT,
}

fn monitors() -> Vec<Monitor> {
    unsafe extern "system" fn each(mon: HMONITOR, _: HDC, _: *mut RECT, data: LPARAM) -> BOOL {
        let list = unsafe { &mut *(data.0 as *mut Vec<Monitor>) };
        let mut mi = MONITORINFOEXW::default();
        mi.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
        if unsafe { GetMonitorInfoW(mon, &mut mi as *mut MONITORINFOEXW as *mut MONITORINFO) }.as_bool() {
            let len = mi.szDevice.iter().position(|&c| c == 0).unwrap_or(mi.szDevice.len());
            let name = String::from_utf16_lossy(&mi.szDevice[..len]).trim_start_matches(['\\', '.']).to_string();
            list.push(Monitor { name, bounds: mi.monitorInfo.rcMonitor, work: mi.monitorInfo.rcWork });
        }
        BOOL(1)
    }
    let mut list: Vec<Monitor> = Vec::new();
    unsafe {
        let _ = EnumDisplayMonitors(None, None, Some(each), LPARAM(&mut list as *mut _ as isize));
    }
    list
}

// ---------------- "is the desktop covered?" ----------------

/// Top-level windows that could hide a desktop. Kept strict, since a false "covered"
/// freezes the wallpaper in plain sight: visible, not minimised, not cloaked (other
/// virtual desktop, suspended store app), not click-through, not ours, not the shell.
fn covering_candidates() -> Vec<(HWND, RECT)> {
    unsafe extern "system" fn each(h: HWND, data: LPARAM) -> BOOL {
        let list = unsafe { &mut *(data.0 as *mut Vec<(HWND, RECT)>) };
        unsafe {
            if !IsWindowVisible(h).as_bool() || IsIconic(h).as_bool() {
                return BOOL(1);
            }
            let ex = GetWindowLongPtrW(h, GWL_EXSTYLE) as u32;
            if ex & WS_EX_TRANSPARENT.0 != 0 {
                return BOOL(1);
            }
            let mut pid = 0u32;
            GetWindowThreadProcessId(h, Some(&mut pid));
            if pid == std::process::id() {
                return BOOL(1);
            }
            let mut cloaked = 0u32;
            if DwmGetWindowAttribute(h, DWMWA_CLOAKED, &mut cloaked as *mut u32 as *mut c_void, 4).is_ok() && cloaked != 0 {
                return BOOL(1);
            }
            let cls = class_of(h);
            // The shell itself, and its full-screen overlays that only show during a virtual-desktop
            // switch, Task View or Alt+Tab (they would pause the wallpaper for nothing).
            if matches!(
                cls.as_str(),
                "Progman"
                    | "WorkerW"
                    | "Shell_TrayWnd"
                    | "Shell_SecondaryTrayWnd"
                    | "VirtualDesktopHotkeySwitcher"
                    | "MultitaskingViewFrame"
                    | "XamlExplorerHostIslandWindow"
            ) {
                return BOOL(1);
            }
            let mut r = RECT::default();
            let got = DwmGetWindowAttribute(
                h,
                DWMWA_EXTENDED_FRAME_BOUNDS,
                &mut r as *mut RECT as *mut c_void,
                std::mem::size_of::<RECT>() as u32,
            )
            .is_ok()
                || GetWindowRect(h, &mut r).is_ok();
            if got {
                list.push((h, r));
            }
        }
        BOOL(1)
    }
    let mut list: Vec<(HWND, RECT)> = Vec::new();
    unsafe {
        let _ = EnumWindows(Some(each), LPARAM(&mut list as *mut _ as isize));
    }
    list
}

/// The window that hides this monitor's whole work area (maximised or full screen).
fn covered_by(windows: &[(HWND, RECT)], work: &RECT) -> Option<HWND> {
    windows
        .iter()
        .find(|(_, r)| r.left <= work.left && r.top <= work.top && r.right >= work.right && r.bottom >= work.bottom)
        .map(|(h, _)| *h)
}

/// No window covers a real part of this work area: everything is minimised or hidden.
/// Specks (a 2x2 helper window, a tray flyout's shadow) don't count.
fn desktop_clear(windows: &[(HWND, RECT)], work: &RECT) -> bool {
    let area = |r: &RECT| ((r.right - r.left).max(0) as i64) * ((r.bottom - r.top).max(0) as i64);
    let limit = area(work) / 50;
    !windows.iter().any(|(_, r)| {
        let x = RECT {
            left: r.left.max(work.left),
            top: r.top.max(work.top),
            right: r.right.min(work.right),
            bottom: r.bottom.min(work.bottom),
        };
        area(&x) > limit
    })
}

/// The desktop itself has the foreground: Show desktop (Win+D), or a click on it.
fn desktop_in_front() -> bool {
    let fg = unsafe { GetForegroundWindow() };
    !fg.0.is_null() && matches!(class_of(fg).as_str(), "Progman" | "WorkerW")
}

fn class_of(h: HWND) -> String {
    let mut buf = [0u16; 256];
    let n = unsafe { GetClassNameW(h, &mut buf) };
    String::from_utf16_lossy(&buf[..n.max(0) as usize])
}

/// `?screen=` names the monitor on the socket; `?port=` points the page at the feed.
fn page_url(page: &std::path::Path, screen: &str, port: u16) -> String {
    let mut url = String::from("file:///");
    for ch in page.to_string_lossy().chars() {
        match ch {
            '\\' => url.push('/'),
            ' ' => url.push_str("%20"),
            '#' => url.push_str("%23"),
            '%' => url.push_str("%25"),
            '?' => url.push_str("%3F"),
            c => url.push(c),
        }
    }
    let screen: String = screen.chars().filter(|c| c.is_ascii_alphanumeric()).collect();
    format!("{url}?screen={screen}&port={port}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn page_urls_are_escaped_file_urls() {
        let u = page_url(std::path::Path::new(r"D:\projects\lyrics spicy\a#b\index.html"), "DISPLAY1", 9012);
        assert_eq!(u, "file:///D:/projects/lyrics%20spicy/a%23b/index.html?screen=DISPLAY1&port=9012");
    }

    #[test]
    fn clear_only_when_nothing_real_overlaps() {
        let work = RECT { left: 0, top: 48, right: 1920, bottom: 1080 };
        let h = HWND(std::ptr::null_mut());
        let speck = (h, RECT { left: 78, top: 114, right: 80, bottom: 116 });
        let offscreen = (h, RECT { left: -32000, top: -32000, right: -31984, bottom: -31984 });
        let notepad = (h, RECT { left: 156, top: 228, right: 1596, bottom: 981 });
        let other_monitor = (h, RECT { left: 1920, top: 0, right: 3840, bottom: 1080 });
        assert!(desktop_clear(&[], &work));
        assert!(desktop_clear(&[speck, offscreen, other_monitor], &work));
        assert!(!desktop_clear(&[speck, notepad], &work));
    }

    #[test]
    fn covered_only_when_the_whole_work_area_is() {
        let work = RECT { left: 0, top: 0, right: 1920, bottom: 1040 };
        let full = (HWND(1 as *mut c_void), RECT { left: -8, top: -8, right: 1928, bottom: 1048 });
        let half = (HWND(2 as *mut c_void), RECT { left: 0, top: 0, right: 960, bottom: 1040 });
        assert!(covered_by(&[half], &work).is_none());
        assert_eq!(covered_by(&[half, full], &work), Some(full.0));
    }
}
