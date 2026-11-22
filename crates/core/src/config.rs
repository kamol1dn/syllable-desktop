//! User settings: `%LOCALAPPDATA%\Syllable\config.json`.
//!
//! Field names serialise in PascalCase, matching the C# prototype's `config.json`,
//! so an old file can be imported as is. Unknown keys are ignored and missing keys
//! take their defaults, so adding a setting never breaks an existing file.

use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, RwLock, RwLockReadGuard};

use serde::{Deserialize, Serialize};

use crate::bus::{Bus, Event};

/// The app's per-user data folder (config, log, cache, secrets).
pub fn data_dir() -> PathBuf {
    let base = std::env::var_os("LOCALAPPDATA").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
    base.join("Syllable")
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "PascalCase")]
pub struct Config {
    /// Local WebSocket port for the wallpaper feed (and the optional spicetify bridge).
    pub port: u16,

    // ---- taskbar lyrics strip ----
    pub lyrics_enabled: bool,
    /// Hide the strip while the desktop itself has focus.
    pub lyrics_hide_on_desktop: bool,
    /// "taskbar" (on the bar) or "above" (floating strip beside it).
    pub placement: String,
    /// "top" | "bottom"
    pub v_pos: String,
    /// "left" | "center" | "right"
    pub align: String,
    pub x_offset: i32,
    pub width: i32,
    pub main_font_px: f64,
    pub bg_font_px: f64,
    pub font_family: String,
    pub text_shadow: bool,
    pub sung_alpha: u8,
    pub unsung_alpha: u8,
    pub bg_line_alpha: u8,
    /// Minimum instrumental gap before the interlude dots appear.
    pub interlude_gap_ms: i32,
    /// Sync nudge; positive = lyrics earlier.
    pub global_offset_ms: i32,

    // ---- active-app name strip ----
    pub app_name_enabled: bool,
    pub app_name_placement: String,
    pub app_name_v_pos: String,
    pub app_name_align: String,
    pub app_name_x_offset: i32,
    pub app_name_width: i32,
    pub app_name_font_px: f64,
    pub app_name_alpha: u8,

    // ---- audio visualizer ----
    pub viz_enabled: bool,
    pub viz_preset: String,
    pub viz_randomize_on_track: bool,
    /// "top" | "bottom"
    pub viz_edge: String,
    /// Share of the screen height the visualizer may paint into (0.15..0.6).
    pub viz_height_fraction: f64,
    pub viz_opacity: f64,

    // ---- wallpaper ----
    /// Show the lyrics wallpaper on the desktop ourselves.
    pub wallpaper_host_enabled: bool,
    /// Folder a random background image is taken from on every song change.
    pub wallpaper_folder: String,
    pub wallpaper_desktop: WallpaperSettings,
    pub wallpaper_lock: WallpaperSettings,

    // ---- lyrics sources ----
    /// Loopback port for the Spotify app (PKCE) redirect: `http://127.0.0.1:PORT/callback`.
    pub spotify_redirect_port: u16,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            port: 9012,
            lyrics_enabled: true,
            lyrics_hide_on_desktop: false,
            placement: "taskbar".into(),
            v_pos: "bottom".into(),
            align: "center".into(),
            x_offset: 16,
            width: 620,
            main_font_px: 15.0,
            bg_font_px: 10.5,
            font_family: "Segoe UI Variable Display".into(),
            text_shadow: true,
            sung_alpha: 255,
            unsung_alpha: 100,
            bg_line_alpha: 150,
            interlude_gap_ms: 2500,
            global_offset_ms: 0,
            app_name_enabled: false,
            app_name_placement: "taskbar".into(),
            app_name_v_pos: "top".into(),
            app_name_align: "left".into(),
            app_name_x_offset: 16,
            app_name_width: 260,
            app_name_font_px: 13.0,
            app_name_alpha: 230,
            viz_enabled: true,
            viz_preset: "equalizer-bleed".into(),
            viz_randomize_on_track: false,
            viz_edge: "bottom".into(),
            viz_height_fraction: 0.33,
            viz_opacity: 0.55,
            wallpaper_host_enabled: true,
            wallpaper_folder: String::new(),
            wallpaper_desktop: WallpaperSettings::default(),
            wallpaper_lock: WallpaperSettings::lock_defaults(),
            spotify_redirect_port: 9013,
        }
    }
}

/// One wallpaper surface's look. Sent to the page as the lower-case keys its
/// `settings` object uses (see `to_message`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "PascalCase")]
pub struct WallpaperSettings {
    /// "wallpapers" (random image per song) or "dynamic" (album-art backdrop).
    pub background: String,
    /// "all", or a subfolder / filename-prefix category of the folder.
    pub collection: String,
    /// "split" | "mirror" | "lyrics" | "karaoke" | "corner" | "lock"
    pub layout: String,
    /// Percent.
    pub lyrics_size: i32,
    /// Percent darkening over the background.
    pub dim: i32,
    /// Pixels.
    pub wallpaper_blur: i32,
    pub ken_burns: bool,
    pub audio_reactive: bool,
    pub line_blur: bool,
    pub clock: bool,
}

impl Default for WallpaperSettings {
    fn default() -> Self {
        Self {
            background: "wallpapers".into(),
            collection: "all".into(),
            layout: "split".into(),
            lyrics_size: 100,
            dim: 35,
            wallpaper_blur: 0,
            ken_burns: true,
            audio_reactive: true,
            line_blur: true,
            clock: true,
        }
    }
}

impl WallpaperSettings {
    pub fn lock_defaults() -> Self {
        Self { layout: "lock".into(), clock: false, dim: 45, ..Self::default() }
    }

    /// The settings as the wallpaper page expects them.
    pub fn to_message(&self) -> serde_json::Value {
        serde_json::json!({
            "background": self.background,
            "collection": self.collection,
            "layout": self.layout,
            "lyricssize": self.lyrics_size,
            "dim": self.dim,
            "wallpaperblur": self.wallpaper_blur,
            "kenburns": self.ken_burns,
            "audioreactive": self.audio_reactive,
            "lineblur": self.line_blur,
            "clock": self.clock,
        })
    }
}

/// The shared, live config. Clone freely; all clones see the same values.
#[derive(Clone)]
pub struct ConfigStore {
    inner: Arc<RwLock<Config>>,
    bus: Bus,
    path: PathBuf,
}

impl ConfigStore {
    /// Load `config.json`, or start from defaults (and write them) when it is missing or unreadable.
    pub fn load(bus: Bus) -> Self {
        Self::load_from(data_dir().join("config.json"), bus)
    }

    pub fn load_from(path: PathBuf, bus: Bus) -> Self {
        let config = fs::read_to_string(&path)
            .ok()
            .and_then(|text| serde_json::from_str::<Config>(text.trim_start_matches('\u{feff}')).ok());
        let store = Self { inner: Arc::new(RwLock::new(config.clone().unwrap_or_default())), bus, path };
        if config.is_none() {
            store.save();
        }
        store
    }

    /// Read access. Do not hold the guard across slow work.
    pub fn read(&self) -> RwLockReadGuard<'_, Config> {
        self.inner.read().unwrap()
    }

    /// A copy of the whole config, for code that wants a stable snapshot.
    pub fn snapshot(&self) -> Config {
        self.read().clone()
    }

    /// Change settings, save them, and tell everyone (`Event::ConfigChanged`).
    /// Nothing is saved or announced when the closure leaves the config as it was.
    pub fn update(&self, change: impl FnOnce(&mut Config)) {
        let changed = {
            let mut guard = self.inner.write().unwrap();
            let before = guard.clone();
            change(&mut guard);
            *guard != before
        };
        if changed {
            self.save();
            self.bus.publish(Event::ConfigChanged);
        }
    }

    fn save(&self) {
        let text = match serde_json::to_string_pretty(&*self.read()) {
            Ok(text) => text,
            Err(e) => return crate::log::write(&format!("config: serialise failed: {e}")),
        };
        if let Some(dir) = self.path.parent() {
            let _ = fs::create_dir_all(dir);
        }
        // Write beside, then rename, so a crash mid-write never leaves half a file.
        let tmp = self.path.with_extension("json.tmp");
        if let Err(e) = fs::write(&tmp, text).and_then(|_| fs::rename(&tmp, &self.path)) {
            crate::log::write(&format!("config: save failed: {e}"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("syllable-test-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir.join("config.json")
    }

    #[test]
    fn reads_the_prototype_config_file_format() {
        let path = temp_path("import");
        fs::write(
            &path,
            r#"{ "Port": 9100, "VPos": "top", "MainFontPx": 18, "LegacyKey": 1,
                 "WallpaperDesktop": { "Layout": "karaoke", "SpicyFont": true, "LyricsSize": 120 } }"#,
        )
        .unwrap();
        let store = ConfigStore::load_from(path, Bus::new());
        let c = store.snapshot();
        assert_eq!(c.port, 9100);
        assert_eq!(c.v_pos, "top");
        assert_eq!(c.main_font_px, 18.0);
        assert_eq!(c.wallpaper_desktop.layout, "karaoke");
        assert_eq!(c.wallpaper_desktop.lyrics_size, 120);
        // Missing keys keep their defaults.
        assert_eq!(c.width, 620);
        assert_eq!(c.wallpaper_lock.layout, "lock");
    }

    #[test]
    fn update_saves_and_announces_only_real_changes() {
        let path = temp_path("update");
        let bus = Bus::new();
        let events = bus.subscribe();
        let store = ConfigStore::load_from(path.clone(), bus.clone());
        assert!(path.exists(), "defaults are written on first run");

        store.update(|c| c.width = 620); // unchanged
        assert!(events.try_recv().is_err());

        store.update(|c| c.width = 700);
        assert!(matches!(events.try_recv(), Ok(Event::ConfigChanged)));
        assert_eq!(ConfigStore::load_from(path, bus).read().width, 700);
    }
}
