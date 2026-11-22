//! "Sign in to Spotify": the user signs in to the Spotify web player in a WebView2
//! window and the web player's own bearer token is picked up from its requests. The
//! sign-in cookie (sp_dc) stays in a WebView2 profile of our own
//! (`<data_dir>/spotify-webview`), so when the token ages out a fresh one is fetched
//! by loading the web player in an off-screen window.
//!
//! Every WebView2 run gets its own STA thread with a message pump; callers block on
//! it. Runs are serialised by `Accounts::web_lock`.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context as _};
use syllable_core::log;
use webview2_com::Microsoft::Web::WebView2::Win32::*;
use webview2_com::{
    take_pwstr, CoreWebView2EnvironmentOptions, CreateCoreWebView2ControllerCompletedHandler,
    CreateCoreWebView2EnvironmentCompletedHandler, GetCookiesCompletedHandler, NavigationCompletedEventHandler,
    WebMessageReceivedEventHandler, WebResourceRequestedEventHandler,
};
use windows::core::{w, Interface, HSTRING, PWSTR};
use windows::Win32::Foundation::{E_POINTER, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_APARTMENTTHREADED};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::accounts::Accounts;
use crate::cache::now_secs;

const LOGIN_URL: &str = "https://accounts.spotify.com/login?continue=https%3A%2F%2Fopen.spotify.com%2F";
const PLAYER_URL: &str = "https://open.spotify.com/";
const TOKEN_TTL_SECS: f64 = 50.0 * 60.0;

/// Hooks fetch / XHR so the bearer token reaches us even when request headers aren't exposed.
const HOOK_JS: &str = r#"(function(){
  if (window.__syHooked) return; window.__syHooked = true;
  function report(v){ try { if (v && /^Bearer /i.test(v)) window.chrome.webview.postMessage(String(v)); } catch(e){} }
  var of = window.fetch;
  if (of) window.fetch = function(input, init){
    try {
      var h = init && init.headers, v = null;
      if (h) { v = (typeof h.get === 'function') ? h.get('authorization') : (h['authorization'] || h['Authorization']); }
      if (!v && input && input.headers && typeof input.headers.get === 'function') v = input.headers.get('authorization');
      report(v);
    } catch(e){}
    return of.apply(this, arguments);
  };
  var sh = XMLHttpRequest.prototype.setRequestHeader;
  XMLHttpRequest.prototype.setRequestHeader = function(k, v){
    try { if (String(k).toLowerCase() === 'authorization') report(v); } catch(e){}
    return sh.apply(this, arguments);
  };
})();"#;

fn fresh_token(acc: &Accounts) -> Option<String> {
    let s = acc.secrets();
    s.spotify_web_token.clone().filter(|_| now_secs() - s.spotify_web_token_time < TOKEN_TTL_SECS)
}

/// A usable web player token, refreshed headlessly when it has aged out. `None` when signed out.
pub fn token(acc: &Accounts) -> Option<String> {
    if !acc.spotify_signed_in() {
        return None;
    }
    if let Some(t) = fresh_token(acc) {
        return Some(t);
    }
    let _guard = acc.web_lock.lock().unwrap();
    if let Some(t) = fresh_token(acc) {
        return Some(t);
    }
    if !acc.spotify_signed_in() {
        return None;
    }
    log!("spotify-web: token aged out, refreshing headlessly");
    let ok = run(acc, false).unwrap_or_else(|e| {
        log!("spotify-web: WebView2 unavailable: {e:#}");
        false
    });
    log!("spotify-web: headless refresh {}", if ok { "ok" } else { "failed" });
    if ok { fresh_token(acc) } else { None }
}

/// The token was rejected (401): the next `token()` fetches a new one.
pub fn invalidate(acc: &Accounts) {
    acc.secrets().spotify_web_token_time = 0.0;
}

/// Shows the sign-in window and waits. True once signed in and a token was captured.
pub fn sign_in(acc: &Accounts) -> anyhow::Result<bool> {
    let _guard = acc.web_lock.lock().unwrap();
    run(acc, true)
}

/// Clears the profile's cookies, so the next sign-in starts from scratch.
pub fn clear_cookies(acc: &Accounts) -> anyhow::Result<()> {
    let _guard = acc.web_lock.lock().unwrap();
    let dir = profile_dir(acc);
    on_sta(move || {
        let host = Host::create(&dir, false)?;
        let cookies = unsafe { host.webview.cast::<ICoreWebView2_2>()?.CookieManager()? };
        unsafe { cookies.DeleteAllCookies()? };
        // Read back before closing: the delete is processed asynchronously by the
        // browser process, and closing straight after could leave the cookie behind.
        let mut left = usize::MAX;
        for i in 0..20 {
            left = cookie_names(&cookies, "https://open.spotify.com")?.len();
            if left == 0 {
                break;
            }
            if i == 0 {
                unsafe { cookies.DeleteAllCookies()? };
            }
            pump(Duration::from_millis(100), || false);
        }
        log!("spotify-web: cookies cleared ({left} left for open.spotify.com)");
        host.close();
        Ok(())
    })
}

fn profile_dir(acc: &Accounts) -> String {
    acc.data_dir.join("spotify-webview").display().to_string()
}

/// Runs `f` on a new STA thread and waits for it.
fn on_sta<T: Send + 'static>(f: impl FnOnce() -> anyhow::Result<T> + Send + 'static) -> anyhow::Result<T> {
    std::thread::Builder::new()
        .name("spotify-web".into())
        .spawn(move || {
            // SAFETY: plain COM initialisation for this thread.
            unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok()? };
            let r = f();
            unsafe { CoUninitialize() };
            r
        })?
        .join()
        .map_err(|_| anyhow!("WebView2 thread panicked"))?
}

/// Pumps this thread's messages for up to `d`, or until `stop()`.
fn pump(d: Duration, stop: impl Fn() -> bool) {
    let end = Instant::now() + d;
    let mut msg = MSG::default();
    while Instant::now() < end && !stop() {
        // SAFETY: standard message pump on the owning thread.
        unsafe {
            if PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            } else {
                MsgWaitForMultipleObjects(None, false, 20, QS_ALLINPUT);
            }
        }
    }
}

thread_local! {
    static CONTROLLER: RefCell<Option<ICoreWebView2Controller>> = const { RefCell::new(None) };
    static CLOSED: Cell<bool> = const { Cell::new(false) };
}

extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_SIZE => {
            CONTROLLER.with(|c| {
                if let Some(c) = &*c.borrow() {
                    let mut r = RECT::default();
                    // SAFETY: our own window and controller, on their thread.
                    unsafe {
                        let _ = GetClientRect(hwnd, &mut r);
                        let _ = c.SetBounds(r);
                    }
                }
            });
            LRESULT(0)
        }
        WM_CLOSE => {
            CLOSED.with(|c| c.set(true));
            LRESULT(0)
        }
        // SAFETY: default handling.
        _ => unsafe { DefWindowProcW(hwnd, msg, wp, lp) },
    }
}

/// A window with a WebView2 in it, on the current (STA) thread.
struct Host {
    hwnd: HWND,
    controller: ICoreWebView2Controller,
    webview: ICoreWebView2,
}

impl Host {
    fn create(profile: &str, interactive: bool) -> anyhow::Result<Host> {
        CLOSED.with(|c| c.set(false));
        // SAFETY: Win32 window creation on this thread; strings outlive the calls.
        let hwnd = unsafe {
            let instance = GetModuleHandleW(None)?;
            let class = w!("SyllableSpotifySignIn");
            let wc = WNDCLASSW {
                lpfnWndProc: Some(wndproc),
                hInstance: instance.into(),
                lpszClassName: class,
                hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
                ..Default::default()
            };
            RegisterClassW(&wc); // fails harmlessly when already registered
            let (style, ex, x, y, wdt, hgt) = if interactive {
                let (w, h) = (480, 720);
                let x = (GetSystemMetrics(SM_CXSCREEN) - w) / 2;
                let y = (GetSystemMetrics(SM_CYSCREEN) - h) / 2;
                (WS_OVERLAPPEDWINDOW, WINDOW_EX_STYLE(0), x, y, w, h)
            } else {
                // Off-screen rather than hidden: the web player only runs (and asks for a token) in a live page.
                (WS_POPUP, WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE, -32000, -32000, 1000, 700)
            };
            let hwnd = CreateWindowExW(
                ex,
                class,
                w!("Sign in to Spotify - Syllable"),
                style,
                x,
                y,
                wdt,
                hgt,
                None,
                None,
                Some(instance.into()),
                None,
            )?;
            let _ = ShowWindow(hwnd, if interactive { SW_SHOWNORMAL } else { SW_SHOWNOACTIVATE });
            hwnd
        };

        let env = {
            let (tx, rx) = std::sync::mpsc::channel();
            let options: ICoreWebView2EnvironmentOptions = CoreWebView2EnvironmentOptions::default().into();
            let folder = HSTRING::from(profile);
            CreateCoreWebView2EnvironmentCompletedHandler::wait_for_async_operation(
                Box::new(move |handler| unsafe {
                    CreateCoreWebView2EnvironmentWithOptions(None, &folder, &options, &handler)
                        .map_err(webview2_com::Error::WindowsError)
                }),
                Box::new(move |hr, env| {
                    hr?;
                    let _ = tx.send(env.ok_or_else(|| windows::core::Error::from(E_POINTER)));
                    Ok(())
                }),
            )
            .map_err(|e| anyhow!("{e:?}"))?;
            rx.recv()??
        };
        let controller = {
            let (tx, rx) = std::sync::mpsc::channel();
            CreateCoreWebView2ControllerCompletedHandler::wait_for_async_operation(
                Box::new(move |handler| unsafe {
                    env.CreateCoreWebView2Controller(hwnd, &handler).map_err(webview2_com::Error::WindowsError)
                }),
                Box::new(move |hr, c| {
                    hr?;
                    let _ = tx.send(c.ok_or_else(|| windows::core::Error::from(E_POINTER)));
                    Ok(())
                }),
            )
            .map_err(|e| anyhow!("{e:?}"))?;
            rx.recv()??
        };
        // SAFETY: COM calls on the owning thread.
        let webview = unsafe {
            let mut r = RECT::default();
            let _ = GetClientRect(hwnd, &mut r);
            controller.SetBounds(r)?;
            controller.SetIsVisible(true)?;
            let webview = controller.CoreWebView2()?;
            let settings = webview.Settings()?;
            settings.SetAreDevToolsEnabled(false)?;
            settings.SetIsStatusBarEnabled(false)?;
            if let Ok(w8) = webview.cast::<ICoreWebView2_8>() {
                let _ = w8.SetIsMuted(true);
            }
            webview
        };
        CONTROLLER.with(|c| *c.borrow_mut() = Some(controller.clone()));
        Ok(Host { hwnd, controller, webview })
    }

    fn source(&self) -> String {
        let mut p = PWSTR::null();
        // SAFETY: Source allocates with CoTaskMemAlloc; take_pwstr frees it.
        unsafe { self.webview.Source(&mut p).map(|_| take_pwstr(p)).unwrap_or_default() }
    }

    fn close(self) {
        CONTROLLER.with(|c| *c.borrow_mut() = None);
        // SAFETY: our own controller and window, on their thread.
        unsafe {
            let _ = self.controller.Close();
            let _ = DestroyWindow(self.hwnd);
        }
    }
}

/// Synchronous cookie read (pumps messages while waiting).
fn cookie_names(cookies: &ICoreWebView2CookieManager, uri: &str) -> anyhow::Result<Vec<String>> {
    let (tx, rx) = std::sync::mpsc::channel();
    let cookies = cookies.clone();
    let uri = HSTRING::from(uri);
    GetCookiesCompletedHandler::wait_for_async_operation(
        Box::new(move |handler| unsafe { cookies.GetCookies(&uri, &handler).map_err(webview2_com::Error::WindowsError) }),
        Box::new(move |hr, list| {
            hr?;
            let _ = tx.send(list.map(|l| list_names(&l)).unwrap_or_default());
            Ok(())
        }),
    )
    .map_err(|e| anyhow!("{e:?}"))?;
    Ok(rx.recv()?)
}

fn list_names(list: &ICoreWebView2CookieList) -> Vec<String> {
    let mut names = Vec::new();
    let mut n = 0u32;
    // SAFETY: COM reads on the owning thread; Name allocates, take_pwstr frees.
    unsafe {
        if list.Count(&mut n).is_err() {
            return names;
        }
        for i in 0..n {
            if let Ok(c) = list.GetValueAtIndex(i) {
                let mut p = PWSTR::null();
                if c.Name(&mut p).is_ok() {
                    names.push(take_pwstr(p));
                }
            }
        }
    }
    names
}

fn host_of(url: &str) -> &str {
    url.split("://").nth(1).and_then(|r| r.split(['/', '?', '#']).next()).unwrap_or("")
}

#[derive(Default)]
struct RunState {
    signed_in: bool,
    cookie_gone: bool,
    done: Option<bool>,
    token: Option<String>,
}

/// Loads the login page (interactive) or the web player (headless) and waits for a
/// bearer token seen after the sp_dc sign-in cookie exists: the web player hands out
/// anonymous tokens too.
fn run(acc: &Accounts, interactive: bool) -> anyhow::Result<bool> {
    let dir = profile_dir(acc);
    let (token, cookie_gone) = on_sta(move || run_on_this_thread(&dir, interactive))?;
    let ok = token.is_some();
    if let Some(t) = token {
        let mut s = acc.secrets();
        s.spotify_web_token = Some(t);
        s.spotify_web_token_time = now_secs();
        s.spotify_web_signed_in = true;
    }
    if !ok && cookie_gone {
        // The cookie is gone (not merely offline): drop the sign-in so lyrics stop trying.
        {
            let mut s = acc.secrets();
            s.spotify_web_signed_in = false;
            s.spotify_web_token = None;
        }
        acc.save_and_notify();
    } else {
        acc.save();
    }
    Ok(ok)
}

fn run_on_this_thread(profile: &str, interactive: bool) -> anyhow::Result<(Option<String>, bool)> {
    let host = Rc::new(Host::create(profile, interactive).context("WebView2 unavailable")?);
    let state = Rc::new(RefCell::new(RunState::default()));
    let wv = host.webview.clone();

    let on_token = {
        let state = state.clone();
        Rc::new(move |raw: String| {
            if !state.borrow().signed_in {
                return;
            }
            let t = raw.trim();
            let t = if t.len() > 7 && t[..7].eq_ignore_ascii_case("bearer ") { t[7..].trim() } else { t };
            if t.len() < 40 {
                return; // not a real token
            }
            let mut s = state.borrow_mut();
            if s.done.is_none() {
                s.token = Some(t.to_string());
                s.done = Some(true);
            }
        })
    };

    // SAFETY: all COM calls below run on this STA thread, which owns the WebView.
    unsafe {
        let script = HSTRING::from(HOOK_JS);
        let wv2 = wv.clone();
        webview2_com::AddScriptToExecuteOnDocumentCreatedCompletedHandler::wait_for_async_operation(
            Box::new(move |h| wv2.AddScriptToExecuteOnDocumentCreated(&script, &h).map_err(webview2_com::Error::WindowsError)),
            Box::new(|hr, _id| hr),
        )
        .map_err(|e| anyhow!("{e:?}"))?;

        let mut reg = 0i64;
        let t = on_token.clone();
        wv.add_WebMessageReceived(
            &WebMessageReceivedEventHandler::create(Box::new(move |_, args| {
                if let Some(args) = args {
                    let mut p = PWSTR::null();
                    if args.TryGetWebMessageAsString(&mut p).is_ok() {
                        t(take_pwstr(p));
                    }
                }
                Ok(())
            })),
            &mut reg,
        )?;

        wv.AddWebResourceRequestedFilter(w!("*spotify.com/*"), COREWEBVIEW2_WEB_RESOURCE_CONTEXT_ALL)?;
        let t = on_token.clone();
        wv.add_WebResourceRequested(
            &WebResourceRequestedEventHandler::create(Box::new(move |_, args| {
                if let Some(args) = args {
                    if let Ok(headers) = args.Request().and_then(|r| r.Headers()) {
                        let mut has = windows::core::BOOL::default();
                        if headers.Contains(w!("Authorization"), &mut has).is_ok() && has.as_bool() {
                            let mut p = PWSTR::null();
                            if headers.GetHeader(w!("Authorization"), &mut p).is_ok() {
                                t(take_pwstr(p));
                            }
                        }
                    }
                }
                Ok(())
            })),
            &mut reg,
        )?;

        let cookies = wv.cast::<ICoreWebView2_2>()?.CookieManager()?;
        let (h, st) = (host.clone(), state.clone());
        wv.add_NavigationCompleted(
            &NavigationCompletedEventHandler::create(Box::new(move |_, _| {
                let source = h.source();
                let host_name = host_of(&source).to_ascii_lowercase();
                log!("spotify-web: loaded {host_name}{}", if interactive { "" } else { " (headless)" });
                if host_name != "open.spotify.com" {
                    return Ok(());
                }
                let (h2, st2) = (h.clone(), st.clone());
                let handler = GetCookiesCompletedHandler::create(Box::new(move |hr, list| {
                    if hr.is_err() {
                        return Ok(());
                    }
                    let has = list.map(|l| list_names(&l).iter().any(|n| n == "sp_dc")).unwrap_or(false);
                    let signed_in = st2.borrow().signed_in;
                    if has && !signed_in {
                        // Tokens seen from here on belong to the signed-in user; a reload
                        // makes the player ask for one straight away.
                        st2.borrow_mut().signed_in = true;
                        let _ = h2.webview.Reload();
                    } else if !has && !interactive {
                        log!("spotify-web: sign-in cookie is gone (signed out elsewhere or expired)");
                        let mut s = st2.borrow_mut();
                        s.cookie_gone = true;
                        s.done.get_or_insert(false);
                    } else if !has && interactive && !h2.source().contains("accounts.") {
                        // Landed on the player without signing in (e.g. pressed back): show the login page again.
                        let _ = h2.webview.Navigate(&HSTRING::from(LOGIN_URL));
                    }
                    Ok(())
                }));
                let _ = cookies.GetCookies(w!("https://open.spotify.com"), &handler);
                Ok(())
            })),
            &mut reg,
        )?;

        wv.Navigate(&HSTRING::from(if interactive { LOGIN_URL } else { PLAYER_URL }))?;
    }

    let timeout = if interactive { Duration::from_secs(600) } else { Duration::from_secs(25) };
    let st = state.clone();
    pump(timeout, move || st.borrow().done.is_some() || CLOSED.with(|c| c.get()));
    let (token, gone) = {
        let s = state.borrow();
        (if s.done == Some(true) { s.token.clone() } else { None }, s.cookie_gone)
    };
    let ok = token.is_some();
    if CLOSED.with(|c| c.get()) && !ok {
        log!("spotify-web: window closed before signing in");
    }
    drop(on_token);
    // Handlers hold clones of the host; closing releases the WebView regardless.
    match Rc::try_unwrap(host) {
        Ok(h) => h.close(),
        Err(rc) => {
            CONTROLLER.with(|c| *c.borrow_mut() = None);
            // SAFETY: our own controller and window.
            unsafe {
                let _ = rc.controller.Close();
                let _ = DestroyWindow(rc.hwnd);
            }
        }
    }
    Ok((token, gone))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_accounts(name: &str) -> (std::sync::Arc<Accounts>, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("syllable-lyrics-web-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        (Accounts::new(dir.clone(), 0, crate::http::agent(), None), dir)
    }

    /// Needs the WebView2 runtime and network: `cargo test -p syllable-lyrics -- --ignored webview`.
    /// With no sign-in cookie, a "signed in" state must be dropped by the headless refresh.
    #[test]
    #[ignore]
    fn webview_headless_refresh_without_cookie_signs_out() {
        let (acc, dir) = temp_accounts("headless");
        acc.secrets().spotify_web_signed_in = true;
        let started = Instant::now();
        assert_eq!(token(&acc), None);
        assert!(!acc.spotify_signed_in(), "cookie gone -> signed out");
        assert!(started.elapsed() < Duration::from_secs(30));
        clear_cookies(&acc).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Opens the real sign-in window, waits for it to load, then closes it like a user would.
    #[test]
    #[ignore]
    fn webview_sign_in_window_opens_and_closing_it_cancels() {
        let (acc, dir) = temp_accounts("interactive");
        let closer = std::thread::spawn(|| {
            std::thread::sleep(Duration::from_secs(10));
            // SAFETY: finding and closing our own test window.
            unsafe {
                let hwnd = FindWindowW(w!("SyllableSpotifySignIn"), None).expect("sign-in window is open");
                PostMessageW(Some(hwnd), WM_CLOSE, WPARAM(0), LPARAM(0)).unwrap();
            }
        });
        assert!(!sign_in(&acc).unwrap());
        closer.join().unwrap();
        assert!(!acc.spotify_signed_in());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn hosts() {
        assert_eq!(host_of("https://open.spotify.com/"), "open.spotify.com");
        assert_eq!(host_of("https://accounts.spotify.com/login?continue=x"), "accounts.spotify.com");
        assert_eq!(host_of("about:blank"), "");
    }
}
