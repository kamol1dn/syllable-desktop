//! The page protocol: what the settings page sends, and the state it is sent.
//!
//! Page → Rust (each a JSON string posted with `chrome.webview.postMessage`):
//!   {"type":"ready"}                                   page loaded; reply with init + state
//!   {"type":"set","path":"WallpaperLock.Dim","value":40}  change one config value
//!   {"type":"applyStyle","surface":"desktop","id":"glow"}  one style, one update
//!   {"type":"action","name":"openLog"|"openDataFolder"|"clearCache"|"chooseFolder"|"clearFolder"|"nextWallpaper"|"copy" (+ "text")|"refresh"}
//!   {"type":"section","id":"lyrics"}                   remember the open section
//!   {"type":"account", ...}                            see `accounts.rs`
//!
//! Rust → page:
//!   {"type":"init", styles, version, dataDir, section}   once per load
//!   {"type":"state", config, active, collections, cache, accounts}  on load and on every change
//!   {"type":"toast", text, kind}                         short feedback for an action
//!
//! `set` is generic on purpose: the path names a key of the config file, so a field
//! another module adds to `Config` can be edited without any change here.

use serde_json::{json, Value};
use syllable_core::{Config, ConfigStore};

use crate::presets::{self, Surface};

/// Set `path` (dot-separated PascalCase keys, e.g. `WallpaperDesktop.Dim`) to `value`.
/// Only existing keys can be set, and only with a value of the same JSON kind;
/// integers are rounded, numbers are clamped by `sanitize`.
pub fn set_path(c: &mut Config, path: &str, value: &Value) -> Result<(), String> {
    let mut root = serde_json::to_value(&*c).map_err(|e| e.to_string())?;
    let mut slot = &mut root;
    for key in path.split('.') {
        slot = slot
            .as_object_mut()
            .and_then(|o| o.get_mut(key))
            .ok_or_else(|| format!("unknown setting {path}"))?;
    }
    let new = match (&*slot, value) {
        (Value::Bool(_), Value::Bool(_)) | (Value::String(_), Value::String(_)) => value.clone(),
        (Value::Number(old), Value::Number(n)) => {
            let f = n.as_f64().ok_or("bad number")?;
            if !f.is_finite() {
                return Err("bad number".into());
            }
            if old.is_f64() {
                json!(f)
            } else {
                json!(f.round() as i64)
            }
        }
        _ => return Err(format!("{path}: wrong kind of value {value}")),
    };
    *slot = new;
    let mut next: Config = serde_json::from_value(root).map_err(|e| format!("{path}: {e}"))?;
    if path == "WallpaperFolder" && next.wallpaper_folder != c.wallpaper_folder {
        // Collections belong to the old folder.
        next.wallpaper_desktop.collection = "all".into();
        next.wallpaper_lock.collection = "all".into();
    }
    // Clamp only the value being set: other keys belong to whoever wrote them.
    let mut clean = next.clone();
    sanitize(&mut clean);
    let clean = serde_json::to_value(&clean).map_err(|e| e.to_string())?;
    let mut merged = serde_json::to_value(&next).map_err(|e| e.to_string())?;
    let mut from = &clean;
    let mut to = &mut merged;
    for key in path.split('.') {
        from = &from[key];
        to = &mut to[key];
    }
    *to = from.clone();
    if path == "WallpaperFolder" {
        merged["WallpaperDesktop"]["Collection"] = json!(next.wallpaper_desktop.collection);
        merged["WallpaperLock"]["Collection"] = json!(next.wallpaper_lock.collection);
    }
    *c = serde_json::from_value(merged).map_err(|e| format!("{path}: {e}"))?;
    Ok(())
}

/// Keep every value inside the range the app can use (the page uses the same ranges).
pub fn sanitize(c: &mut Config) {
    fn pick(v: &mut String, allowed: &[&str], fallback: &str) {
        if !allowed.contains(&v.as_str()) {
            *v = fallback.into();
        }
    }
    c.port = c.port.max(1024);
    c.spotify_redirect_port = c.spotify_redirect_port.max(1024);
    pick(&mut c.placement, &["taskbar", "above"], "taskbar");
    pick(&mut c.v_pos, &["top", "bottom"], "bottom");
    pick(&mut c.align, &["left", "center", "right"], "center");
    c.x_offset = c.x_offset.clamp(0, 4000);
    c.width = c.width.clamp(200, 2000);
    c.main_font_px = c.main_font_px.clamp(8.0, 32.0);
    c.bg_font_px = c.bg_font_px.clamp(6.0, 24.0);
    if c.font_family.trim().is_empty() {
        c.font_family = Config::default().font_family;
    }
    c.interlude_gap_ms = c.interlude_gap_ms.clamp(500, 20000);
    c.global_offset_ms = c.global_offset_ms.clamp(-5000, 5000);

    pick(&mut c.app_name_placement, &["taskbar", "above"], "taskbar");
    pick(&mut c.app_name_v_pos, &["top", "bottom"], "top");
    pick(&mut c.app_name_align, &["left", "center", "right"], "left");
    c.app_name_x_offset = c.app_name_x_offset.clamp(0, 4000);
    c.app_name_width = c.app_name_width.clamp(80, 1200);
    c.app_name_font_px = c.app_name_font_px.clamp(8.0, 48.0);
    c.app_name_alpha = c.app_name_alpha.max(25);

    let viz: Vec<&str> = presets::VIZ_PRESETS.iter().map(|p| p.0).collect();
    pick(&mut c.viz_preset, &viz, "equalizer-bleed");
    pick(&mut c.viz_edge, &["top", "bottom"], "bottom");
    c.viz_height_fraction = c.viz_height_fraction.clamp(0.15, 0.6);
    c.viz_opacity = c.viz_opacity.clamp(0.05, 1.0);

    for s in [&mut c.wallpaper_desktop, &mut c.wallpaper_lock] {
        pick(&mut s.background, &["wallpapers", "dynamic"], "wallpapers");
        pick(&mut s.layout, &["split", "mirror", "lyrics", "karaoke", "corner", "lock"], "split");
        if s.collection.trim().is_empty() {
            s.collection = "all".into();
        }
        s.lyrics_size = s.lyrics_size.clamp(60, 200);
        s.dim = s.dim.clamp(0, 90);
        s.wallpaper_blur = s.wallpaper_blur.clamp(0, 40);
    }
}

/// What the page needs on load.
pub fn init_message(section: &str) -> Value {
    json!({
        "type": "init",
        "styles": presets::catalogue_json(),
        "version": env!("CARGO_PKG_VERSION"),
        "dataDir": syllable_core::config::data_dir().display().to_string(),
        "section": section,
    })
}

/// The live state: config, the active style per surface (`null` = Custom) and the extras.
pub fn state_message(c: &Config, accounts: Value) -> Value {
    let mut active = serde_json::Map::new();
    for s in Surface::ALL {
        active.insert(s.id().into(), json!(presets::active(c, s)));
    }
    json!({
        "type": "state",
        "config": c,
        "active": active,
        "collections": crate::actions::collections(&c.wallpaper_folder),
        "cache": crate::actions::cache_summary(),
        "accounts": accounts,
        "lock": lock_state(c),
    })
}

/// The lock screen is drawn by Aura Wallpaper loading our page from the local feed:
/// whether Aura is installed, the address to give it, and whether the page is showing.
fn lock_state(c: &Config) -> Value {
    let status = syllable_core::surfaces::lock_status();
    json!({
        "auraInstalled": crate::actions::aura_installed(),
        "url": format!("http://127.0.0.1:{}/lockscreen", c.port),
        "open": status.open,
        "lastSeen": status.last_seen,
    })
}

pub fn toast(text: &str, kind: &str) -> Value {
    json!({ "type": "toast", "text": text, "kind": kind })
}

/// Apply a `set` or `applyStyle` message to the store: one `ConfigStore::update` each.
pub fn apply_change(store: &ConfigStore, msg: &Value) -> Result<(), String> {
    match msg["type"].as_str() {
        Some("set") => {
            let path = msg["path"].as_str().ok_or("set: no path")?;
            let mut result = Ok(());
            store.update(|c| result = set_path(c, path, &msg["value"]));
            result
        }
        Some("applyStyle") => {
            let surface = msg["surface"].as_str().and_then(Surface::parse).ok_or("applyStyle: bad surface")?;
            let id = msg["id"].as_str().ok_or("applyStyle: no id")?;
            let mut found = false;
            store.update(|c| found = presets::apply(c, surface, id));
            if found {
                Ok(())
            } else {
                Err(format!("applyStyle: no style {id}"))
            }
        }
        other => Err(format!("not a change: {other:?}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use syllable_core::{Bus, Event};

    #[test]
    fn set_path_edits_top_level_and_nested_keys() {
        let mut c = Config::default();
        set_path(&mut c, "Width", &json!(812.6)).unwrap();
        assert_eq!(c.width, 813);
        set_path(&mut c, "MainFontPx", &json!(17.5)).unwrap();
        assert_eq!(c.main_font_px, 17.5);
        set_path(&mut c, "WallpaperLock.Layout", &json!("karaoke")).unwrap();
        assert_eq!(c.wallpaper_lock.layout, "karaoke");
        assert_eq!(c.wallpaper_desktop.layout, "split");
        set_path(&mut c, "VizEnabled", &json!(false)).unwrap();
        assert!(!c.viz_enabled);
    }

    #[test]
    fn set_path_refuses_unknown_keys_and_wrong_kinds() {
        let mut c = Config::default();
        assert!(set_path(&mut c, "Nope", &json!(1)).is_err());
        assert!(set_path(&mut c, "WallpaperLock.Nope", &json!(1)).is_err());
        assert!(set_path(&mut c, "Width", &json!("wide")).is_err());
        assert!(set_path(&mut c, "LyricsEnabled", &json!(1)).is_err());
        assert!(set_path(&mut c, "SungAlpha", &json!(999)).is_err(), "u8 overflow is refused");
        assert_eq!(c, Config::default());
    }

    #[test]
    fn set_path_clamps_and_resets_collections_with_a_new_folder() {
        let mut c = Config::default();
        set_path(&mut c, "GlobalOffsetMs", &json!(99999)).unwrap();
        assert_eq!(c.global_offset_ms, 5000);
        set_path(&mut c, "VPos", &json!("sideways")).unwrap();
        assert_eq!(c.v_pos, "bottom");
        c.wallpaper_desktop.collection = "nord".into();
        set_path(&mut c, "WallpaperFolder", &json!(r"D:\Walls")).unwrap();
        assert_eq!(c.wallpaper_desktop.collection, "all");
    }

    #[test]
    fn set_path_leaves_other_keys_as_their_owners_wrote_them() {
        let mut c = Config { viz_preset: "some-new-preset".into(), width: 5000, ..Config::default() };
        set_path(&mut c, "TextShadow", &json!(false)).unwrap();
        assert_eq!(c.viz_preset, "some-new-preset");
        assert_eq!(c.width, 5000);
        set_path(&mut c, "Width", &json!(5000)).unwrap();
        assert_eq!(c.width, 2000, "the key being set is clamped");
    }

    #[test]
    fn a_style_is_one_update_and_one_announcement() {
        let dir = std::env::temp_dir().join(format!("syllable-settings-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let bus = Bus::new();
        let store = ConfigStore::load_from(dir.join("config.json"), bus.clone());
        let events = bus.subscribe();
        apply_change(&store, &json!({"type":"applyStyle","surface":"desktop","id":"focus"})).unwrap();
        assert!(matches!(events.try_recv(), Ok(Event::ConfigChanged)));
        assert!(events.try_recv().is_err(), "exactly one ConfigChanged");
        let saved: Value = serde_json::from_str(&std::fs::read_to_string(dir.join("config.json")).unwrap()).unwrap();
        assert_eq!(saved["WallpaperDesktop"]["Layout"], "lyrics");
        assert_eq!(saved["WallpaperDesktop"]["Dim"], 65);
        assert_eq!(presets::active(&store.snapshot(), Surface::Desktop), Some("focus"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Every key of the config file has a control on the page. Catches fields that
    /// other modules add to `Config` without a control here.
    #[test]
    fn every_setting_has_a_control_on_the_page() {
        let page = include_str!("../../../assets/settings/app.js");
        let config = serde_json::to_value(Config::default()).unwrap();
        let mut missing = vec![];
        for (key, value) in config.as_object().unwrap() {
            if let Some(nested) = value.as_object() {
                // Wallpaper surfaces share one block of controls keyed by the field name.
                for sub in nested.keys() {
                    if !page.contains(&format!("'{sub}'")) {
                        missing.push(format!("{key}.{sub}"));
                    }
                }
            } else if !page.contains(&format!("'{key}'")) {
                missing.push(key.clone());
            }
        }
        assert!(missing.is_empty(), "no control on the settings page for: {missing:?}");
    }
}
