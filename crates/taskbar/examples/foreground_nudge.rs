//! Diagnostic: can a process without foreground rights take the foreground with
//! the zero-distance input nudge used by the dock unblock? Creates a transparent
//! 1x1 window, tries plain SetForegroundWindow, then the nudge, then hands the
//! foreground straight back. `cargo run -p syllable-taskbar --example foreground_nudge`

use windows::core::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::Win32::Foundation::COLORREF;

fn main() {
    std::thread::sleep(std::time::Duration::from_secs(3));
    unsafe {
        let inst = GetModuleHandleW(None).unwrap();
        let h = CreateWindowExW(WS_EX_TOOLWINDOW | WS_EX_LAYERED | WS_EX_TRANSPARENT, w!("Static"), w!("nudge"), WS_POPUP, 0, 0, 1, 1, None, None, Some(inst.into()), None).unwrap();
        let _ = SetLayeredWindowAttributes(h, COLORREF(0), 0, LWA_ALPHA);
        let _ = ShowWindow(h, SW_SHOWNA);
        let prev = GetForegroundWindow();
        let plain = SetForegroundWindow(h).as_bool() && GetForegroundWindow() == h;
        println!("plain SetForegroundWindow: {plain}");
        if !plain {
            let nudge = INPUT { r#type: INPUT_MOUSE, Anonymous: INPUT_0 { mi: MOUSEINPUT { dwFlags: MOUSEEVENTF_MOVE, ..Default::default() } } };
            SendInput(&[nudge], std::mem::size_of::<INPUT>() as i32);
            let after = SetForegroundWindow(h).as_bool() && GetForegroundWindow() == h;
            println!("after input nudge: {after}");
        }
        let _ = SetForegroundWindow(prev);
        println!("handed back: {}", GetForegroundWindow() == prev);
        let _ = DestroyWindow(h);
    }
}
