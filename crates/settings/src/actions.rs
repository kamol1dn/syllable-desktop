//! One-shot actions behind the page's buttons, and the extras shown beside settings
//! (wallpaper collections, cache size).

use std::fs;
use std::path::{Path, PathBuf};

use syllable_core::config::data_dir;
use windows::core::{HSTRING, PCWSTR};
use windows::Win32::Foundation::HWND;
use windows::Win32::System::Com::{CoCreateInstance, CoTaskMemFree, CLSCTX_INPROC_SERVER};
use windows::Win32::UI::Shell::{
    FileOpenDialog, IFileOpenDialog, IShellItem, SHCreateItemFromParsingName, ShellExecuteW,
    FOS_FORCEFILESYSTEM, FOS_PICKFOLDERS, SIGDN_FILESYSPATH,
};
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

const IMAGE_EXT: [&str; 7] = ["jpg", "jpeg", "png", "webp", "gif", "bmp", "avif"];

/// Where the lyrics module keeps its cache (one file per song), as in the prototype.
pub fn cache_dir() -> PathBuf {
    data_dir().join("cache")
}

pub fn log_path() -> PathBuf {
    data_dir().join("log.txt")
}

/// "42 songs · 1.3 MB", or "Empty".
pub fn cache_summary() -> String {
    let mut count = 0u64;
    let mut bytes = 0u64;
    walk(&cache_dir(), 4, &mut |p| {
        if let Ok(m) = fs::metadata(p) {
            count += 1;
            bytes += m.len();
        }
    });
    if count == 0 {
        return "Empty".into();
    }
    let mb = bytes as f64 / 1024.0 / 1024.0;
    format!("{count} {} · {mb:.1} MB", if count == 1 { "song" } else { "songs" })
}

pub fn clear_cache() -> std::io::Result<()> {
    let dir = cache_dir();
    if dir.exists() {
        fs::remove_dir_all(&dir)?;
    }
    fs::create_dir_all(&dir)
}

fn walk(dir: &Path, depth: u32, f: &mut dyn FnMut(&Path)) {
    let Ok(entries) = fs::read_dir(dir) else { return };
    for e in entries.flatten() {
        let p = e.path();
        match e.file_type() {
            Ok(t) if t.is_dir() && depth > 0 => walk(&p, depth - 1, f),
            Ok(t) if t.is_file() => f(&p),
            _ => {}
        }
    }
}

/// Collections in the wallpaper folder: subfolders and filename prefixes
/// ("nord_forest.jpg" → "nord") that hold at least 3 images, largest first, at most 12.
pub fn collections(folder: &str) -> Vec<String> {
    if folder.is_empty() {
        return vec![];
    }
    let root = Path::new(folder);
    let mut counts: Vec<(String, u32)> = vec![];
    let mut bump = |k: &str| {
        let k = k.to_lowercase();
        match counts.iter_mut().find(|(n, _)| *n == k) {
            Some((_, c)) => *c += 1,
            None => counts.push((k, 1)),
        }
    };
    walk(root, 3, &mut |p| {
        let is_image = p
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| IMAGE_EXT.contains(&e.to_ascii_lowercase().as_str()));
        if !is_image {
            return;
        }
        if let Some(stem) = p.file_stem().and_then(|s| s.to_str()) {
            if let Some(cut) = stem.find('_').filter(|&i| i > 0) {
                bump(&stem[..cut]);
            }
        }
        if let Some(parent) = p.parent().filter(|d| *d != root) {
            if let Some(name) = parent.file_name().and_then(|n| n.to_str()) {
                bump(name);
            }
        }
    });
    counts.retain(|(_, c)| *c >= 3);
    counts.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    counts.into_iter().take(12).map(|(n, _)| n).collect()
}

fn shell_open(target: &Path) -> bool {
    let target = HSTRING::from(target.as_os_str());
    let r = unsafe { ShellExecuteW(None, &HSTRING::from("open"), &target, PCWSTR::null(), PCWSTR::null(), SW_SHOWNORMAL) };
    r.0 as isize > 32
}

pub fn open_log() -> bool {
    let path = log_path();
    if !path.exists() {
        let _ = fs::create_dir_all(data_dir());
        let _ = fs::write(&path, "");
    }
    shell_open(&path)
}

pub fn open_data_folder() -> bool {
    let _ = fs::create_dir_all(data_dir());
    shell_open(&data_dir())
}

/// The system folder picker, owned by `owner`. `None` when cancelled.
pub fn choose_folder(owner: HWND, start: &str) -> Option<String> {
    unsafe {
        let dialog: IFileOpenDialog = CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER).ok()?;
        let options = dialog.GetOptions().ok()?;
        dialog.SetOptions(options | FOS_PICKFOLDERS | FOS_FORCEFILESYSTEM).ok()?;
        let _ = dialog.SetTitle(&HSTRING::from("Folder of wallpapers (a random one on every song)"));
        if !start.is_empty() && Path::new(start).is_dir() {
            if let Ok(item) = SHCreateItemFromParsingName::<_, _, IShellItem>(&HSTRING::from(start), None) {
                let _ = dialog.SetFolder(&item);
            }
        }
        dialog.Show(Some(owner)).ok()?;
        let item = dialog.GetResult().ok()?;
        let name = item.GetDisplayName(SIGDN_FILESYSPATH).ok()?;
        let path = name.to_string().ok();
        CoTaskMemFree(Some(name.0 as *const _));
        path
    }
}

/// Put `text` on the clipboard.
pub fn copy_text(owner: HWND, text: &str) -> bool {
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::System::DataExchange::{CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData};
    use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
    use windows::Win32::System::Ole::CF_UNICODETEXT;
    let wide: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        if OpenClipboard(Some(owner)).is_err() {
            return false;
        }
        let ok = (|| -> windows::core::Result<()> {
            EmptyClipboard()?;
            let mem = GlobalAlloc(GMEM_MOVEABLE, wide.len() * 2)?;
            let dst = GlobalLock(mem) as *mut u16;
            if dst.is_null() {
                return Err(windows::core::Error::from_thread());
            }
            std::ptr::copy_nonoverlapping(wide.as_ptr(), dst, wide.len());
            let _ = GlobalUnlock(mem);
            // The clipboard owns the memory from here.
            SetClipboardData(CF_UNICODETEXT.0 as u32, Some(HANDLE(mem.0)))?;
            Ok(())
        })()
        .is_ok();
        let _ = CloseClipboard();
        ok
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collections_count_subfolders_and_prefixes() {
        let dir = std::env::temp_dir().join(format!("syllable-walls-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("City")).unwrap();
        for n in ["nord_a.jpg", "nord_b.png", "Nord_c.webp", "solo_a.jpg", "x.txt", "x_1.txt", "x_2.txt", "x_3.txt"] {
            fs::write(dir.join(n), b"").unwrap();
        }
        for n in ["1.jpg", "2.jpg", "3.jpg", "4.jpg"] {
            fs::write(dir.join("City").join(n), b"").unwrap();
        }
        assert_eq!(collections(dir.to_str().unwrap()), vec!["city", "nord"]);
        assert!(collections("").is_empty());
        let _ = fs::remove_dir_all(&dir);
    }
}
