//! Where the settings window was, and which section was open:
//! `%LOCALAPPDATA%\Syllable\settings-window.json`. Kept out of `Config` so moving the
//! window does not announce `ConfigChanged` to every module.

use std::fs;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use syllable_core::config::data_dir;

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "PascalCase")]
pub struct WindowState {
    /// The restored (not maximized) rectangle, in workspace pixels. `None` before the first close.
    pub rect: Option<[i32; 4]>,
    pub maximized: bool,
    pub section: String,
}

fn path() -> PathBuf {
    data_dir().join("settings-window.json")
}

pub fn load() -> WindowState {
    fs::read_to_string(path()).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()
}

pub fn save(state: &WindowState) {
    let Ok(text) = serde_json::to_string_pretty(state) else { return };
    let _ = fs::create_dir_all(data_dir());
    let tmp = path().with_extension("json.tmp");
    if fs::write(&tmp, text).and_then(|_| fs::rename(&tmp, path())).is_err() {
        syllable_core::log!("settings: could not save the window position");
    }
}
