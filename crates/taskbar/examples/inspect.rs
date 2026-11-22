//! Diagnostic: find this app's taskbar strip windows and report how they are hosted
//! (parent, bounds, visibility, z-order among the taskbar's children, the taskbar's
//! band, and what `WindowFromPoint` says at the strip's centre: click-through means
//! it is not our window). With `capture <file.bmp>`, saves the screen pixels under
//! the strip (only that rectangle). Sends no input.

use windows::core::*;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::*;
use windows::Win32::UI::HiDpi::*;
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

fn text(h: HWND) -> String {
    let mut buf = [0u16; 128];
    let n = unsafe { GetWindowTextW(h, &mut buf) };
    String::from_utf16_lossy(&buf[..n as usize])
}

unsafe extern "system" fn collect(h: HWND, l: LPARAM) -> BOOL {
    let v = &mut *(l.0 as *mut Vec<HWND>);
    if class(h) == "Syllable.TaskbarStrip" {
        v.push(h);
    }
    TRUE
}

fn save_bmp(path: &str, r: RECT) {
    unsafe {
        let (w, h) = (r.right - r.left, r.bottom - r.top);
        let screen = GetDC(None);
        let mem = CreateCompatibleDC(Some(screen));
        let bmp = CreateCompatibleBitmap(screen, w, h);
        let old = SelectObject(mem, bmp.into());
        let _ = BitBlt(mem, 0, 0, w, h, Some(screen), r.left, r.top, SRCCOPY);
        let mut info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w,
                biHeight: h,
                biPlanes: 1,
                biBitCount: 24,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let stride = ((w * 3 + 3) & !3) as usize;
        let mut px = vec![0u8; stride * h as usize];
        GetDIBits(mem, bmp, 0, h as u32, Some(px.as_mut_ptr().cast()), &mut info, DIB_RGB_COLORS);
        SelectObject(mem, old);
        let _ = DeleteObject(bmp.into());
        let _ = DeleteDC(mem);
        ReleaseDC(None, screen);
        let mut file = Vec::new();
        let size = 54 + px.len() as u32;
        file.extend_from_slice(b"BM");
        file.extend_from_slice(&size.to_le_bytes());
        file.extend_from_slice(&[0; 4]);
        file.extend_from_slice(&54u32.to_le_bytes());
        file.extend_from_slice(&40u32.to_le_bytes());
        file.extend_from_slice(&w.to_le_bytes());
        file.extend_from_slice(&h.to_le_bytes());
        file.extend_from_slice(&1u16.to_le_bytes());
        file.extend_from_slice(&24u16.to_le_bytes());
        file.extend_from_slice(&[0; 24]);
        file.extend_from_slice(&px);
        std::fs::write(path, file).unwrap();
    }
}

fn main() {
    unsafe {
        let _ = SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        let tray = FindWindowW(w!("Shell_TrayWnd"), None).unwrap_or_default();
        let fg = GetForegroundWindow();
        println!("taskbar {:?} band {:?}; foreground {}", tray.0, band(tray), class(fg));
        let mut found: Vec<HWND> = Vec::new();
        // Docked children of the taskbar, and floating top-level strips.
        let _ = EnumChildWindows(Some(tray), Some(collect), LPARAM(&mut found as *mut _ as isize));
        let _ = EnumWindows(Some(collect), LPARAM(&mut found as *mut _ as isize));
        found.dedup();
        let args: Vec<String> = std::env::args().collect();
        for h in &found {
            let mut r = RECT::default();
            let _ = GetWindowRect(*h, &mut r);
            let parent = GetAncestor(*h, GA_PARENT);
            let docked = parent == tray;
            // Position among the taskbar's children (0 = topmost).
            let mut z = 0;
            let mut p = GetWindow(*h, GW_HWNDPREV).unwrap_or_default();
            while !p.is_invalid() {
                z += 1;
                p = GetWindow(p, GW_HWNDPREV).unwrap_or_default();
            }
            let center = POINT { x: (r.left + r.right) / 2, y: (r.top + r.bottom) / 2 };
            let hit = WindowFromPoint(center);
            let mut pid = 0;
            GetWindowThreadProcessId(*h, Some(&mut pid));
            println!(
                "pid {pid} '{}' {:?}: {} visible={} rect=({},{})-({},{}) z={} hit-at-centre={} ({})",
                text(*h),
                h.0,
                if docked { "DOCKED child of the taskbar" } else { "floating top-level" },
                IsWindowVisible(*h).as_bool(),
                r.left, r.top, r.right, r.bottom,
                z,
                class(hit),
                if hit == *h { "NOT click-through" } else { "click-through" }
            );
            if let Some(i) = args.iter().position(|a| a == "capture") {
                let base = args.get(i + 1).cloned().unwrap_or("strip.bmp".into());
                let path = base.replace(".bmp", &format!("-{}.bmp", format!("{}-{pid}", text(*h).replace(" ", "-"))));
                save_bmp(&path, r);
                println!("  saved {path}");
            }
        }
        if found.is_empty() {
            println!("no strip windows found");
        }
    }
}
