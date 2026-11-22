//! The notification-area icon and the main thread's message loop.
//!
//! Left click asks for the settings window (`Event::OpenSettings`); right click
//! shows a small menu. The icon is re-added when Explorer restarts.

use std::sync::OnceLock;

use anyhow::{anyhow, Result};
use syllable_core::{Bus, Context, Event};
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{
    CloseHandle, GetLastError, ERROR_ALREADY_EXISTS, HANDLE, HWND, LPARAM, LRESULT, POINT, WPARAM,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::UI::Shell::{
    Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NOTIFYICONDATAW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu, DestroyWindow,
    DispatchMessageW, GetCursorPos, GetMessageW, LoadIconW, PostQuitMessage, RegisterClassW,
    RegisterWindowMessageW, SetForegroundWindow, TrackPopupMenu, TranslateMessage, IDI_APPLICATION,
    MF_SEPARATOR, MF_STRING, MSG, TPM_RIGHTBUTTON, WINDOW_EX_STYLE, WINDOW_STYLE, WM_APP,
    WM_COMMAND, WM_DESTROY, WM_LBUTTONUP, WM_RBUTTONUP, WNDCLASSW,
};

const WM_TRAY: u32 = WM_APP + 1;
const TRAY_ID: u32 = 1;
const CMD_SETTINGS: usize = 1;
const CMD_EXIT: usize = 2;

static BUS: OnceLock<Bus> = OnceLock::new();
static TASKBAR_CREATED: OnceLock<u32> = OnceLock::new();

/// Keeps the single-instance mutex alive for the life of the process.
pub struct Instance(HANDLE);

impl Drop for Instance {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

/// `None` when another copy of the app is already running.
pub fn single_instance() -> Option<Instance> {
    unsafe {
        let handle = CreateMutexW(None, true, w!("Local\\Syllable.SingleInstance")).ok()?;
        if GetLastError() == ERROR_ALREADY_EXISTS {
            let _ = CloseHandle(handle);
            return None;
        }
        Some(Instance(handle))
    }
}

/// Create the tray icon and pump messages until the user exits.
pub fn run(ctx: &Context) -> Result<()> {
    let _ = BUS.set(ctx.bus.clone());
    unsafe {
        let module = GetModuleHandleW(None)?;
        let class = w!("SyllableTray");
        let wc = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: module.into(),
            lpszClassName: class,
            ..Default::default()
        };
        if RegisterClassW(&wc) == 0 {
            return Err(anyhow!("RegisterClassW failed"));
        }
        // A hidden top-level window (not message-only): it must receive the
        // broadcast "TaskbarCreated" message to restore the icon after Explorer restarts.
        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            class,
            w!("Syllable"),
            WINDOW_STYLE(0),
            0,
            0,
            0,
            0,
            None,
            None,
            Some(module.into()),
            None,
        )?;
        let _ = TASKBAR_CREATED.set(RegisterWindowMessageW(w!("TaskbarCreated")));
        add_icon(hwnd);

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
    Ok(())
}

fn icon_data(hwnd: HWND) -> NOTIFYICONDATAW {
    let mut data = NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: TRAY_ID,
        uFlags: NIF_ICON | NIF_MESSAGE | NIF_TIP,
        uCallbackMessage: WM_TRAY,
        ..Default::default()
    };
    // Placeholder icon until the product has its own.
    data.hIcon = unsafe { LoadIconW(None, IDI_APPLICATION).unwrap_or_default() };
    for (dst, src) in data.szTip.iter_mut().zip("Syllable".encode_utf16()) {
        *dst = src;
    }
    data
}

fn add_icon(hwnd: HWND) {
    unsafe {
        let _ = Shell_NotifyIconW(NIM_ADD, &icon_data(hwnd));
    }
}

fn remove_icon(hwnd: HWND) {
    unsafe {
        let _ = Shell_NotifyIconW(NIM_DELETE, &icon_data(hwnd));
    }
}

fn show_menu(hwnd: HWND) {
    unsafe {
        let Ok(menu) = CreatePopupMenu() else { return };
        let _ = AppendMenuW(menu, MF_STRING, CMD_SETTINGS, w!("Settings…"));
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
        let _ = AppendMenuW(menu, MF_STRING, CMD_EXIT, w!("Exit"));
        let mut at = POINT::default();
        let _ = GetCursorPos(&mut at);
        // Without this the menu does not close when the user clicks elsewhere.
        let _ = SetForegroundWindow(hwnd);
        let _ = TrackPopupMenu(menu, TPM_RIGHTBUTTON, at.x, at.y, None, hwnd, None);
        let _ = DestroyMenu(menu);
    }
}

fn publish(event: Event) {
    if let Some(bus) = BUS.get() {
        bus.publish(event);
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_TRAY => {
            match lparam.0 as u32 {
                WM_LBUTTONUP => publish(Event::OpenSettings),
                WM_RBUTTONUP => show_menu(hwnd),
                _ => {}
            }
            LRESULT(0)
        }
        WM_COMMAND => {
            match wparam.0 & 0xFFFF {
                CMD_SETTINGS => publish(Event::OpenSettings),
                CMD_EXIT => {
                    let _ = DestroyWindow(hwnd);
                }
                _ => {}
            }
            LRESULT(0)
        }
        WM_DESTROY => {
            remove_icon(hwnd);
            PostQuitMessage(0);
            LRESULT(0)
        }
        _ => {
            if Some(&msg) == TASKBAR_CREATED.get() {
                add_icon(hwnd);
                return LRESULT(0);
            }
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
    }
}
