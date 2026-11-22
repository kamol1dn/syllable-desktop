//! The settings window: a Win32 window hosting the settings page in WebView2.
//!
//! Everything runs on one STA thread with its own message loop. A hidden
//! message-only window receives the bus events (forwarded by a small thread that
//! blocks on the bus), so the window can be opened, refreshed and closed from there.
//! The window itself is created on `OpenSettings` and destroyed when closed: WebView2
//! costs memory, and opening again takes well under a second.

use std::cell::RefCell;
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Receiver;
use std::sync::Arc;

use serde_json::Value;
use syllable_core::{config::data_dir, log, Context, Event};
use webview2_com::Microsoft::Web::WebView2::Win32::*;
use webview2_com::{
    take_pwstr, CreateCoreWebView2ControllerCompletedHandler, CreateCoreWebView2EnvironmentCompletedHandler,
    NewWindowRequestedEventHandler, WebMessageReceivedEventHandler,
};
use windows::core::{w, Interface, HSTRING, PCWSTR, PWSTR};
use windows::Win32::Foundation::{COLORREF, E_POINTER, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Dwm::{DwmSetWindowAttribute, DWMWA_CAPTION_COLOR, DWMWA_USE_IMMERSIVE_DARK_MODE};
use windows::Win32::Graphics::Gdi::{
    CreateSolidBrush, GetMonitorInfoW, MonitorFromRect, MonitorFromWindow, MONITORINFO, MONITOR_DEFAULTTONEAREST,
    MONITOR_DEFAULTTONULL,
};
use windows::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::{GetDpiForWindow, SetThreadDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2};
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::placement::{self, WindowState};
use crate::{accounts, actions, bridge};

const WM_OPEN: u32 = WM_APP + 1;
const WM_PUSH_STATE: u32 = WM_APP + 2;
const WM_SHUTDOWN: u32 = WM_APP + 3;

/// #111114, the page background, as COLORREF (0x00BBGGRR).
const BACKGROUND: u32 = 0x0014_1111;
const DEFAULT_SIZE: (i32, i32) = (1040, 740);
const MIN_SIZE: (i32, i32) = (820, 560);

struct Ui {
    ctx: Context,
    /// Set while a state push is queued, so a burst of changes sends one.
    push_queued: Arc<AtomicBool>,
    state: WindowState,
    win: Option<Win>,
}

struct Win {
    hwnd: HWND,
    controller: Option<ICoreWebView2Controller>,
    webview: Option<ICoreWebView2>,
}

thread_local! {
    static UI: RefCell<Option<Ui>> = const { RefCell::new(None) };
}

/// Short, non-reentrant access to the UI state. Never call into Win32 or WebView2
/// inside `f`: those can dispatch messages that come back here.
fn with_ui<R>(f: impl FnOnce(&mut Ui) -> R) -> Option<R> {
    UI.with(|u| u.try_borrow_mut().ok().and_then(|mut u| u.as_mut().map(f)))
}

fn hwnd_from(raw: isize) -> HWND {
    HWND(raw as *mut c_void)
}

/// The settings thread. Returns on `Event::Quit`.
pub fn run(ctx: Context, events: Receiver<Event>, open_now: Option<String>) {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        // The window and the page scale crisply on every monitor.
        let _ = SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
    let msg_hwnd = match register_and_create_message_window() {
        Ok(h) => h,
        Err(e) => return log!("settings: could not create its windows: {e}"),
    };

    let push_queued = Arc::new(AtomicBool::new(false));
    let mut state = placement::load();
    if let Some(section) = open_now.as_deref().filter(|s| !s.is_empty() && *s != "1") {
        state.section = section.to_string();
    }
    UI.with(|u| *u.borrow_mut() = Some(Ui { ctx, push_queued: push_queued.clone(), state, win: None }));

    // Bus → this thread's message loop.
    let target = msg_hwnd.0 as isize;
    let forwarder = std::thread::Builder::new().name("settings-bus".into()).spawn(move || {
        let post = |msg: u32| unsafe {
            let _ = PostMessageW(Some(hwnd_from(target)), msg, WPARAM(0), LPARAM(0));
        };
        for event in events {
            match event {
                Event::OpenSettings => post(WM_OPEN),
                Event::ConfigChanged | Event::AccountsChanged | Event::SurfacesChanged => {
                    if !push_queued.swap(true, Ordering::AcqRel) {
                        post(WM_PUSH_STATE);
                    }
                }
                Event::Quit => {
                    post(WM_SHUTDOWN);
                    break;
                }
                _ => {}
            }
        }
    });
    if let Err(e) = forwarder {
        return log!("settings: could not start the bus thread: {e}");
    }

    if open_now.is_some() {
        open();
    }

    unsafe {
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
    log!("settings: stopped");
}

fn register_and_create_message_window() -> windows::core::Result<HWND> {
    unsafe {
        let module = GetModuleHandleW(None)?;
        let msg_class = WNDCLASSW {
            lpfnWndProc: Some(msg_proc),
            hInstance: module.into(),
            lpszClassName: w!("SyllableSettingsBus"),
            ..Default::default()
        };
        RegisterClassW(&msg_class);
        let win_class = WNDCLASSW {
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(win_proc),
            hInstance: module.into(),
            hCursor: LoadCursorW(None, IDC_ARROW)?,
            hIcon: LoadIconW(Some(module.into()), PCWSTR(1 as _)).or_else(|_| LoadIconW(None, IDI_APPLICATION))?,
            hbrBackground: CreateSolidBrush(COLORREF(BACKGROUND)),
            lpszClassName: w!("SyllableSettings"),
            ..Default::default()
        };
        RegisterClassW(&win_class);
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("SyllableSettingsBus"),
            w!(""),
            WINDOW_STYLE(0),
            0,
            0,
            0,
            0,
            Some(HWND_MESSAGE),
            None,
            Some(module.into()),
            None,
        )
    }
}

unsafe extern "system" fn msg_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_OPEN => open(),
        WM_PUSH_STATE => {
            with_ui(|ui| ui.push_queued.store(false, Ordering::Release));
            push_state();
        }
        WM_SHUTDOWN => {
            if let Some(h) = with_ui(|ui| ui.win.as_ref().map(|w| w.hwnd)).flatten() {
                let _ = DestroyWindow(h);
            }
            PostQuitMessage(0);
        }
        _ => return DefWindowProcW(hwnd, msg, wparam, lparam),
    }
    LRESULT(0)
}

/// Show the window, creating it if needed; bring it to the front if it is open.
fn open() {
    if let Some(h) = with_ui(|ui| ui.win.as_ref().map(|w| w.hwnd)).flatten() {
        unsafe {
            let _ = ShowWindow(h, if IsIconic(h).as_bool() { SW_RESTORE } else { SW_SHOW });
            let _ = SetForegroundWindow(h);
        }
        return;
    }
    match create_window() {
        Ok(h) => {
            with_ui(|ui| ui.win = Some(Win { hwnd: h, controller: None, webview: None }));
            if let Err(e) = start_webview(h) {
                log!("settings: WebView2 failed to start: {e:?}");
                show_webview_missing(h);
            }
        }
        Err(e) => log!("settings: could not create the window: {e}"),
    }
}

fn create_window() -> windows::core::Result<HWND> {
    let saved = with_ui(|ui| ui.state.clone()).unwrap_or_default();
    unsafe {
        let module = GetModuleHandleW(None)?;
        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("SyllableSettings"),
            w!("Syllable"),
            WS_OVERLAPPEDWINDOW,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            None,
            None,
            Some(module.into()),
            None,
        )?;
        let on: i32 = 1;
        let _ = DwmSetWindowAttribute(hwnd, DWMWA_USE_IMMERSIVE_DARK_MODE, &on as *const _ as _, 4);
        let caption = COLORREF(BACKGROUND);
        let _ = DwmSetWindowAttribute(hwnd, DWMWA_CAPTION_COLOR, &caption as *const _ as _, 4);

        // Where it was last time, if that is still on a screen; else centred on the
        // screen Windows picked, at the default size for that screen's DPI.
        let restored = saved.rect.and_then(|[l, t, r, b]| {
            let rect = RECT { left: l, top: t, right: r, bottom: b };
            let on_screen = !MonitorFromRect(&rect, MONITOR_DEFAULTTONULL).is_invalid();
            (on_screen && r - l >= 200 && b - t >= 150).then_some(rect)
        });
        match restored {
            Some(rect) => {
                let wp = WINDOWPLACEMENT {
                    length: std::mem::size_of::<WINDOWPLACEMENT>() as u32,
                    showCmd: SW_HIDE.0 as u32,
                    rcNormalPosition: rect,
                    ..Default::default()
                };
                let _ = SetWindowPlacement(hwnd, &wp);
            }
            None => {
                let dpi = GetDpiForWindow(hwnd).max(96) as i32;
                let (w, h) = (DEFAULT_SIZE.0 * dpi / 96, DEFAULT_SIZE.1 * dpi / 96);
                let mut info = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
                let _ = GetMonitorInfoW(MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST), &mut info);
                let work = info.rcWork;
                let (w, h) = (w.min(work.right - work.left), h.min(work.bottom - work.top));
                let x = work.left + (work.right - work.left - w) / 2;
                let y = work.top + (work.bottom - work.top - h) / 2;
                let _ = SetWindowPos(hwnd, None, x, y, w, h, SWP_NOZORDER | SWP_NOACTIVATE);
            }
        }
        let _ = ShowWindow(hwnd, if saved.maximized && restored.is_some() { SW_SHOWMAXIMIZED } else { SW_SHOW });
        let _ = SetForegroundWindow(hwnd);
        Ok(hwnd)
    }
}

fn start_webview(hwnd: HWND) -> webview2_com::Result<()> {
    let folder = data_dir().join("WebView2").join("settings");
    let _ = std::fs::create_dir_all(&folder);
    let folder = HSTRING::from(folder.as_os_str());
    let raw = hwnd.0 as isize;

    let on_env = CreateCoreWebView2EnvironmentCompletedHandler::create(Box::new(move |result, env| {
        if let Err(e) = &result {
            log!("settings: WebView2 environment failed: {e}");
        }
        result?;
        let env = env.ok_or_else(|| windows::core::Error::from(E_POINTER))?;
        let on_controller = CreateCoreWebView2ControllerCompletedHandler::create(Box::new(move |result, controller| {
            if let Err(e) = &result {
                log!("settings: WebView2 controller failed: {e}");
            }
            result?;
            let controller = controller.ok_or_else(|| windows::core::Error::from(E_POINTER))?;
            if let Err(e) = attach(hwnd_from(raw), controller) {
                log!("settings: WebView2 setup failed: {e:?}");
            }
            Ok(())
        }));
        unsafe { env.CreateCoreWebView2Controller(hwnd_from(raw), &on_controller) }
    }));
    // Debug builds: SYLLABLE_SETTINGS_DEVTOOLS_PORT=9334 exposes this page (and only
    // this one) to the Chrome DevTools protocol, for automated checks.
    let options: Option<ICoreWebView2EnvironmentOptions> = std::env::var("SYLLABLE_SETTINGS_DEVTOOLS_PORT")
        .ok()
        .filter(|_| cfg!(debug_assertions))
        .and_then(|port| port.parse::<u16>().ok())
        .map(|port| {
            let o = webview2_com::CoreWebView2EnvironmentOptions::default();
            unsafe { o.set_additional_browser_arguments(format!("--remote-debugging-port={port}")) };
            o.into()
        });
    unsafe {
        CreateCoreWebView2EnvironmentWithOptions(PCWSTR::null(), &folder, options.as_ref(), &on_env)?;
    }
    Ok(())
}

/// The controller is ready: configure it, wire messages and load the page.
fn attach(hwnd: HWND, controller: ICoreWebView2Controller) -> windows::core::Result<()> {
    // Closed before WebView2 was ready.
    let current = with_ui(|ui| ui.win.as_ref().map(|w| w.hwnd)).flatten();
    if current != Some(hwnd) {
        unsafe {
            let _ = controller.Close();
        }
        return Ok(());
    }
    unsafe {
        if let Ok(c2) = controller.cast::<ICoreWebView2Controller2>() {
            let _ = c2.SetDefaultBackgroundColor(COREWEBVIEW2_COLOR { A: 255, R: 0x11, G: 0x11, B: 0x14 });
        }
        let webview = controller.CoreWebView2()?;
        let settings = webview.Settings()?;
        let debug = cfg!(debug_assertions);
        // The page suppresses the menu except in text fields (Paste into the key box).
        let _ = settings.SetAreDefaultContextMenusEnabled(true);
        let _ = settings.SetAreDevToolsEnabled(debug);
        let _ = settings.SetIsZoomControlEnabled(false);
        let _ = settings.SetIsStatusBarEnabled(false);
        if let Ok(s3) = settings.cast::<ICoreWebView2Settings3>() {
            let _ = s3.SetAreBrowserAcceleratorKeysEnabled(debug);
        }
        if let Ok(s5) = settings.cast::<ICoreWebView2Settings5>() {
            let _ = s5.SetIsPinchZoomEnabled(false);
        }
        if let Ok(s6) = settings.cast::<ICoreWebView2Settings6>() {
            let _ = s6.SetIsSwipeNavigationEnabled(false);
        }

        let mut token = Default::default();
        let raw = hwnd.0 as isize;
        webview.add_WebMessageReceived(
            &WebMessageReceivedEventHandler::create(Box::new(move |sender, args| {
                if let (Some(sender), Some(args)) = (sender, args) {
                    let mut text = PWSTR::null();
                    if args.TryGetWebMessageAsString(&mut text).is_ok() {
                        on_page_message(hwnd_from(raw), &sender, &take_pwstr(text));
                    }
                }
                Ok(())
            })),
            &mut token,
        )?;
        // Links with target=_blank open in the user's browser, never in a new WebView window.
        webview.add_NewWindowRequested(
            &NewWindowRequestedEventHandler::create(Box::new(|_, args| {
                if let Some(args) = args {
                    let mut uri = PWSTR::null();
                    args.Uri(&mut uri)?;
                    let uri = take_pwstr(uri);
                    if uri.starts_with("https://") {
                        ShellExecuteW(None, w!("open"), &HSTRING::from(uri), PCWSTR::null(), PCWSTR::null(), SW_SHOWNORMAL);
                    }
                    args.SetHandled(true)?;
                }
                Ok(())
            })),
            &mut token,
        )?;

        let mut rect = RECT::default();
        let _ = GetClientRect(hwnd, &mut rect);
        controller.SetBounds(rect)?;
        controller.SetIsVisible(true)?;
        webview.NavigateToString(&HSTRING::from(crate::page_html()))?;
        let _ = controller.MoveFocus(COREWEBVIEW2_MOVE_FOCUS_REASON_PROGRAMMATIC);

        with_ui(|ui| {
            if let Some(win) = ui.win.as_mut() {
                win.controller = Some(controller.clone());
                win.webview = Some(webview.clone());
            }
        });
    }
    Ok(())
}

fn send(webview: &ICoreWebView2, message: &Value) {
    unsafe {
        let _ = webview.PostWebMessageAsJson(&HSTRING::from(message.to_string()));
    }
}

fn webview() -> Option<ICoreWebView2> {
    with_ui(|ui| ui.win.as_ref().and_then(|w| w.webview.clone())).flatten()
}

fn ctx() -> Option<Context> {
    with_ui(|ui| ui.ctx.clone())
}

fn state_message() -> Option<Value> {
    let ctx = ctx()?;
    let config = ctx.config.snapshot();
    Some(bridge::state_message(&config, accounts::state(&ctx)))
}

fn push_state() {
    if let (Some(wv), Some(state)) = (webview(), state_message()) {
        send(&wv, &state);
    }
}

fn on_page_message(hwnd: HWND, webview: &ICoreWebView2, text: &str) {
    let Some(ctx) = ctx() else { return };
    let msg: Value = match serde_json::from_str(text) {
        Ok(v) => v,
        Err(e) => return log!("settings: bad message from the page: {e}"),
    };
    match msg["type"].as_str().unwrap_or("") {
        "ready" => {
            let section = with_ui(|ui| ui.state.section.clone()).unwrap_or_default();
            send(webview, &bridge::init_message(&section));
            if let Some(state) = state_message() {
                send(webview, &state);
            }
        }
        "set" | "applyStyle" => {
            if let Err(e) = bridge::apply_change(&ctx.config, &msg) {
                log!("settings: {e}");
                // Put the page back to what is really stored.
                push_state();
            }
        }
        "section" => {
            if let Some(id) = msg["id"].as_str() {
                with_ui(|ui| ui.state.section = id.to_string());
            }
        }
        "account" => {
            if let Some(toast) = accounts::handle(&ctx, &msg) {
                send(webview, &toast);
            }
            push_state();
        }
        "action" => {
            let name = msg["name"].as_str().unwrap_or("");
            match name {
                "openLog" => {
                    actions::open_log();
                }
                "openDataFolder" => {
                    actions::open_data_folder();
                }
                "clearCache" => {
                    let (text, kind) = match actions::clear_cache() {
                        Ok(()) => ("Lyrics cache cleared.", "ok"),
                        Err(e) => {
                            log!("settings: clear cache failed: {e}");
                            ("Could not clear the cache: a file is in use.", "error")
                        }
                    };
                    send(webview, &bridge::toast(text, kind));
                    push_state();
                }
                "chooseFolder" => {
                    let start = ctx.config.read().wallpaper_folder.clone();
                    if let Some(folder) = actions::choose_folder(hwnd, &start) {
                        let msg = serde_json::json!({"type": "set", "path": "WallpaperFolder", "value": folder});
                        if let Err(e) = bridge::apply_change(&ctx.config, &msg) {
                            log!("settings: {e}");
                        }
                    }
                }
                "clearFolder" => {
                    let msg = serde_json::json!({"type": "set", "path": "WallpaperFolder", "value": ""});
                    let _ = bridge::apply_change(&ctx.config, &msg);
                }
                "nextWallpaper" => {
                    // The new image on the wallpaper is the feedback.
                    ctx.bus.publish(Event::NextWallpaper);
                }
                "copy" => {
                    let ok = actions::copy_text(hwnd, msg["text"].as_str().unwrap_or(""));
                    send(webview, &bridge::toast(if ok { "Copied." } else { "Could not copy." }, if ok { "ok" } else { "error" }));
                }
                "refresh" => push_state(),
                "openAuraStore" => {
                    if !actions::open_aura_store() {
                        send(webview, &bridge::toast("Could not open the Steam store.", "error"));
                    }
                }
                other => log!("settings: unknown action {other}"),
            }
        }
        other => log!("settings: unknown message {other}"),
    }
}

/// Without the WebView2 runtime there is no page: say so in the window itself.
fn show_webview_missing(hwnd: HWND) {
    unsafe {
        let _ = MessageBoxW(
            Some(hwnd),
            w!("The settings need the Microsoft Edge WebView2 Runtime, which is missing or broken on this PC.\n\nInstall it from https://go.microsoft.com/fwlink/p/?LinkId=2124703 and open the settings again."),
            w!("Syllable"),
            MB_OK | MB_ICONWARNING,
        );
        let _ = DestroyWindow(hwnd);
    }
}

fn save_placement(hwnd: HWND) {
    let mut wp = WINDOWPLACEMENT { length: std::mem::size_of::<WINDOWPLACEMENT>() as u32, ..Default::default() };
    if unsafe { GetWindowPlacement(hwnd, &mut wp) }.is_err() {
        return;
    }
    let r = wp.rcNormalPosition;
    let state = with_ui(|ui| {
        ui.state.rect = Some([r.left, r.top, r.right, r.bottom]);
        ui.state.maximized = wp.showCmd == SW_SHOWMAXIMIZED.0 as u32 || unsafe { IsZoomed(hwnd).as_bool() };
        ui.state.clone()
    });
    if let Some(state) = state {
        placement::save(&state);
    }
}

fn controller() -> Option<ICoreWebView2Controller> {
    with_ui(|ui| ui.win.as_ref().and_then(|w| w.controller.clone())).flatten()
}

unsafe extern "system" fn win_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_SIZE => {
            if let Some(c) = controller() {
                let mut rect = RECT::default();
                let _ = GetClientRect(hwnd, &mut rect);
                let _ = c.SetBounds(rect);
            }
            LRESULT(0)
        }
        WM_MOVE => {
            if let Some(c) = controller() {
                let _ = c.NotifyParentWindowPositionChanged();
            }
            LRESULT(0)
        }
        WM_EXITSIZEMOVE => {
            save_placement(hwnd);
            LRESULT(0)
        }
        WM_ACTIVATE => {
            if wparam.0 & 0xFFFF != 0 {
                if let Some(c) = controller() {
                    let _ = c.MoveFocus(COREWEBVIEW2_MOVE_FOCUS_REASON_PROGRAMMATIC);
                }
            }
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        WM_GETMINMAXINFO => {
            let info = &mut *(lparam.0 as *mut MINMAXINFO);
            let dpi = GetDpiForWindow(hwnd).max(96) as i32;
            info.ptMinTrackSize = POINT { x: MIN_SIZE.0 * dpi / 96, y: MIN_SIZE.1 * dpi / 96 };
            LRESULT(0)
        }
        WM_DPICHANGED => {
            let r = &*(lparam.0 as *const RECT);
            let _ = SetWindowPos(hwnd, None, r.left, r.top, r.right - r.left, r.bottom - r.top, SWP_NOZORDER | SWP_NOACTIVATE);
            LRESULT(0)
        }
        WM_DESTROY => {
            save_placement(hwnd);
            let controller = with_ui(|ui| ui.win.take().and_then(|w| w.controller)).flatten();
            if let Some(c) = controller {
                let _ = c.Close();
            }
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}
