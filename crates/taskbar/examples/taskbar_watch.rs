//! Diagnostic: passively watch the taskbar's z-band. Logs every foreground change
//! (class and band only), and on every band change also whether a layered child can be
//! created in the taskbar right now, whether SetParent of an existing popup into it
//! works, and whether a child docked at start-up is still a visible child. Sends no input.
//! (Creating windows on every foreground event fed the events back; it only probes on
//! band changes now.)
//! `cargo run -p syllable-taskbar --example taskbar_watch -- <seconds>`

use std::time::{Duration, Instant};
use windows::core::*;
use windows::Win32::Foundation::*;
use windows::Win32::System::LibraryLoader::*;
use windows::Win32::UI::Accessibility::*;
use windows::Win32::UI::WindowsAndMessaging::*;

fn band(h: HWND) -> Option<u32> {
    unsafe {
        let m = GetModuleHandleW(w!("user32.dll")).ok()?;
        let p = GetProcAddress(m, s!("GetWindowBand"))?;
        let f: extern "system" fn(HWND, *mut u32) -> BOOL = std::mem::transmute(p);
        let mut b = 0;
        f(h, &mut b).as_bool().then_some(b)
    }
}

fn class(h: HWND) -> String {
    let mut buf = [0u16; 128];
    let n = unsafe { GetClassNameW(h, &mut buf) };
    String::from_utf16_lossy(&buf[..n as usize])
}

fn try_child(tray: HWND) -> String {
    unsafe {
        match CreateWindowExW(
            WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_NOACTIVATE,
            w!("Static"), w!("probe"), WS_CHILD, 0, 0, 10, 10, Some(tray), None, None, None,
        ) {
            Ok(h) => {
                let _ = DestroyWindow(h);
                "ok".into()
            }
            Err(e) => format!("refused {:?}", e.code()),
        }
    }
}

fn try_setparent(tray: HWND) -> String {
    unsafe {
        let Ok(h) = CreateWindowExW(
            WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW,
            w!("Static"), w!("probe2"), WS_POPUP, 0, 0, 10, 10, None, None, None, None,
        ) else {
            return "popup failed".into();
        };
        SetLastError(WIN32_ERROR(0));
        let r = SetParent(h, Some(tray));
        let s = match r {
            Ok(_) => "ok".into(),
            Err(e) => format!("refused {:?}", e.code()),
        };
        let _ = DestroyWindow(h);
        s
    }
}

static mut TRAY: HWND = HWND(std::ptr::null_mut());
static mut DOCKED: HWND = HWND(std::ptr::null_mut());
static mut START: Option<Instant> = None;

fn report(why: &str) {
    unsafe {
        let tray = TRAY;
        let fg = GetForegroundWindow();
        let docked = DOCKED;
        let dstate = if docked.0.is_null() {
            "none".to_string()
        } else {
            format!(
                "alive={} visible={} parent_is_tray={}",
                IsWindow(Some(docked)).as_bool(),
                IsWindowVisible(docked).as_bool(),
                GetAncestor(docked, GA_PARENT) == tray
            )
        };
        let t = START.map(|s| s.elapsed().as_secs_f32()).unwrap_or(0.0);
        println!(
            "{t:7.2}s {why:10} fg={} band={:?} create={} setparent={} docked[{dstate}]",
            class(fg), band(tray), try_child(tray), try_setparent(tray)
        );
    }
}

unsafe extern "system" fn on_fg(_: HWINEVENTHOOK, _: u32, _: HWND, _: i32, _: i32, _: u32, _: u32) {
    let t = START.map(|s| s.elapsed().as_secs_f32()).unwrap_or(0.0);
    println!("{t:7.2}s foreground fg={} band={:?}", class(GetForegroundWindow()), band(TRAY));
}

fn main() {
    let secs: u64 = std::env::args().nth(1).and_then(|a| a.parse().ok()).unwrap_or(120);
    unsafe {
        START = Some(Instant::now());
        TRAY = FindWindowW(w!("Shell_TrayWnd"), None).unwrap();
        DOCKED = CreateWindowExW(
            WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_NOACTIVATE,
            w!("Static"), w!("docked"), WS_CHILD | WS_VISIBLE, 0, 0, 2, 2, Some(TRAY), None, None, None,
        )
        .unwrap_or_default();
        report("start");
        let _hook = SetWinEventHook(EVENT_SYSTEM_FOREGROUND, EVENT_SYSTEM_FOREGROUND, None, Some(on_fg), 0, 0, WINEVENT_OUTOFCONTEXT);
        let end = Instant::now() + Duration::from_secs(secs);
        let mut last_band = band(TRAY);
        while Instant::now() < end {
            let mut msg = MSG::default();
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                DispatchMessageW(&msg);
            }
            let b = band(TRAY);
            if b != last_band {
                last_band = b;
                report("band");
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        if !DOCKED.0.is_null() {
            let _ = DestroyWindow(DOCKED);
        }
    }
}
