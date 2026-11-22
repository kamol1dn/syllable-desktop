//! Diagnostic: report the taskbar's z-band and whether a child window can be created
//! in it right now, then try the foreground nudge. `cargo run -p syllable-taskbar --example taskbar_probe [nudge]`

use windows::core::*;
use windows::Win32::Foundation::*;
use windows::Win32::System::LibraryLoader::*;
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

fn try_child(tray: HWND) -> bool {
    unsafe {
        let r = CreateWindowExW(
            WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_NOACTIVATE,
            w!("Static"),
            w!("probe"),
            WS_CHILD,
            0, 0, 10, 10,
            Some(tray), None, None, None,
        );
        match r {
            Ok(h) => {
                println!("  child created {:?}", h);
                let _ = DestroyWindow(h);
                true
            }
            Err(e) => {
                println!("  child refused: {e}");
                false
            }
        }
    }
}

fn main() {
    unsafe {
        let tray = FindWindowW(w!("Shell_TrayWnd"), None).unwrap();
        if std::env::args().any(|a| a == "shell") {
            let ok = SetForegroundWindow(tray);
            std::thread::sleep(std::time::Duration::from_millis(300));
            println!("focus taskbar: {}", ok.as_bool());
        }
        let fg = GetForegroundWindow();
        println!("fg={:?} class={} tray band={:?}", fg, class(fg), band(tray));
        try_child(tray);
        if std::env::args().any(|a| a == "nudge") {
            let me = CreateWindowExW(
                WS_EX_TOOLWINDOW | WS_EX_LAYERED,
                w!("Static"), w!("nudge"), WS_POPUP | WS_VISIBLE,
                -100, -100, 1, 1, None, None, None, None,
            ).unwrap();
            let ok = SetForegroundWindow(me);
            // let the shell react
            for _ in 0..20 {
                let mut msg = MSG::default();
                while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() { DispatchMessageW(&msg); }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            println!("nudge: SetForegroundWindow={} fg now={} band={:?}", ok.as_bool(), class(GetForegroundWindow()), band(tray));
            try_child(tray);
            let _ = SetForegroundWindow(fg);
            let _ = DestroyWindow(me);
            std::thread::sleep(std::time::Duration::from_millis(200));
            println!("restored fg={} band={:?}", class(GetForegroundWindow()), band(tray));
        }
    }
}
