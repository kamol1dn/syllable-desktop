//! STYLES (presets): the whole catalogue lives in this file.
//!
//! A style is a named bundle of settings for one surface (desktop wallpaper, lock
//! screen, taskbar). To add one, add an entry to the list for its surface below; the
//! settings page draws its thumbnail from the values and shows "Custom" whenever the
//! live settings match no style. Nothing records which style is active: it is worked
//! out by comparing, so it survives restarts and follows changes made anywhere.
//!
//! Styles leave out what is personal to a machine: the wallpaper folder and
//! collection, and where on the taskbar a strip sits (edge, alignment, offset, sizes).
//!
//! The first entry of each list equals `Config::default()`, so a fresh install starts
//! on a named style instead of "Custom" (a test holds this).

use serde::Serialize;
use syllable_core::{Config, WallpaperSettings};

/// Which surface a style belongs to; also the `surface` the page sends.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Surface {
    Taskbar,
    Desktop,
    Lock,
}

impl Surface {
    pub const ALL: [Surface; 3] = [Surface::Taskbar, Surface::Desktop, Surface::Lock];

    pub fn id(self) -> &'static str {
        match self {
            Surface::Taskbar => "taskbar",
            Surface::Desktop => "desktop",
            Surface::Lock => "lock",
        }
    }

    pub fn parse(id: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|s| s.id() == id)
    }
}

/// A look for one wallpaper surface. `look.collection` is ignored: that belongs to
/// the user's folder.
#[derive(Clone, Debug, Serialize)]
pub struct WallpaperStyle {
    pub id: &'static str,
    pub name: &'static str,
    pub blurb: &'static str,
    pub look: WallpaperSettings,
}

impl WallpaperStyle {
    pub fn matches(&self, s: &WallpaperSettings) -> bool {
        let l = &self.look;
        s.background == l.background
            && s.layout == l.layout
            && s.lyrics_size == l.lyrics_size
            && s.dim == l.dim
            && s.wallpaper_blur == l.wallpaper_blur
            && s.ken_burns == l.ken_burns
            && s.audio_reactive == l.audio_reactive
            && s.line_blur == l.line_blur
            && s.clock == l.clock
    }

    pub fn apply_to(&self, s: &mut WallpaperSettings) {
        let l = &self.look;
        s.background = l.background.clone();
        s.layout = l.layout.clone();
        s.lyrics_size = l.lyrics_size;
        s.dim = l.dim;
        s.wallpaper_blur = l.wallpaper_blur;
        s.ken_burns = l.ken_burns;
        s.audio_reactive = l.audio_reactive;
        s.line_blur = l.line_blur;
        s.clock = l.clock;
    }
}

/// A taskbar setup: which modules are on and how the lyrics strip attaches.
/// `viz_preset` only counts while `visualizer` is on, and a style with the
/// visualizer on also turns "new look on every song" off.
#[derive(Clone, Debug, Serialize)]
pub struct TaskbarStyle {
    pub id: &'static str,
    pub name: &'static str,
    pub blurb: &'static str,
    pub lyrics: bool,
    /// "taskbar" (on the taskbar) or "above" (floating).
    pub placement: &'static str,
    pub app_name: bool,
    pub visualizer: bool,
    /// A visualizer preset id (see `VIZ_PRESETS`).
    pub viz_preset: &'static str,
}

impl TaskbarStyle {
    const fn new(id: &'static str, name: &'static str, blurb: &'static str) -> Self {
        Self {
            id,
            name,
            blurb,
            lyrics: true,
            placement: "taskbar",
            app_name: false,
            visualizer: false,
            viz_preset: "equalizer-bleed",
        }
    }

    pub fn matches(&self, c: &Config) -> bool {
        c.lyrics_enabled == self.lyrics
            && placement_of(&c.placement) == self.placement
            && c.app_name_enabled == self.app_name
            && c.viz_enabled == self.visualizer
            && (!self.visualizer || (!c.viz_randomize_on_track && c.viz_preset == self.viz_preset))
    }

    pub fn apply_to(&self, c: &mut Config) {
        c.lyrics_enabled = self.lyrics;
        c.placement = self.placement.into();
        c.app_name_enabled = self.app_name;
        c.viz_enabled = self.visualizer;
        if self.visualizer {
            c.viz_randomize_on_track = false;
            c.viz_preset = self.viz_preset.into();
        }
    }
}

/// Anything that is not "taskbar" floats (the prototype also wrote "above"/"floating").
fn placement_of(p: &str) -> &'static str {
    if p == "taskbar" {
        "taskbar"
    } else {
        "above"
    }
}

/// The visualizer looks, in the order the page lists them.
pub const VIZ_PRESETS: [(&str, &str); 5] = [
    ("glow-taskbar", "Glow, taskbar only"),
    ("equalizer-taskbar", "Equalizer, taskbar only"),
    ("equalizer-bleed", "Equalizer, bleed"),
    ("pulse-bleed", "Pulse, bleed"),
    ("wave-bleed", "Wave, bleed"),
];

fn wp(change: impl FnOnce(&mut WallpaperSettings)) -> WallpaperSettings {
    let mut s = WallpaperSettings::default();
    change(&mut s);
    s
}

/// Desktop wallpaper.
pub fn desktop() -> Vec<WallpaperStyle> {
    vec![
        WallpaperStyle {
            id: "classic",
            name: "Classic",
            blurb: "Cover beside the lyrics, over your wallpapers",
            look: WallpaperSettings::default(),
        },
        WallpaperStyle {
            id: "glow",
            name: "Album glow",
            blurb: "Cover and lyrics on a gradient made from the album art",
            look: wp(|s| {
                s.background = "dynamic".into();
                s.dim = 20;
            }),
        },
        WallpaperStyle {
            id: "mirror",
            name: "Mirror",
            blurb: "Lyrics on the left, cover on the right, soft-focus wallpaper",
            look: wp(|s| {
                s.layout = "mirror".into();
                s.dim = 50;
                s.wallpaper_blur = 6;
            }),
        },
        WallpaperStyle {
            id: "karaoke",
            name: "Karaoke",
            blurb: "One big line in the middle of the screen",
            look: wp(|s| {
                s.layout = "karaoke".into();
                s.background = "dynamic".into();
                s.dim = 20;
                s.lyrics_size = 115;
            }),
        },
        WallpaperStyle {
            id: "corner",
            name: "Corner",
            blurb: "A small panel bottom-right; the wallpaper stays the star",
            look: wp(|s| {
                s.layout = "corner".into();
                s.dim = 20;
            }),
        },
        WallpaperStyle {
            id: "focus",
            name: "Just the words",
            blurb: "Lyrics only, large, on a darkened and blurred wallpaper",
            look: wp(|s| {
                s.layout = "lyrics".into();
                s.dim = 65;
                s.wallpaper_blur = 12;
                s.lyrics_size = 115;
            }),
        },
    ]
}

/// Lock screen. Windows draws its own clock there.
pub fn lock() -> Vec<WallpaperStyle> {
    vec![
        WallpaperStyle {
            id: "centred",
            name: "Centred",
            blurb: "Lyrics under the Windows clock, over your wallpapers",
            look: WallpaperSettings::lock_defaults(),
        },
        WallpaperStyle {
            id: "glow",
            name: "Album glow",
            blurb: "Centred lyrics on the album-art gradient",
            look: wp(|s| {
                s.layout = "lock".into();
                s.background = "dynamic".into();
                s.dim = 20;
                s.clock = false;
            }),
        },
        WallpaperStyle {
            id: "corner",
            name: "Corner",
            blurb: "A small panel bottom-right, wallpaper left clear",
            look: wp(|s| {
                s.layout = "corner".into();
                s.dim = 20;
                s.clock = false;
            }),
        },
        WallpaperStyle {
            id: "karaoke",
            name: "Karaoke",
            blurb: "One big line on the album-art gradient",
            look: wp(|s| {
                s.layout = "karaoke".into();
                s.background = "dynamic".into();
                s.dim = 20;
                s.lyrics_size = 115;
                s.clock = false;
            }),
        },
    ]
}

/// Taskbar.
pub fn taskbar() -> Vec<TaskbarStyle> {
    vec![
        TaskbarStyle {
            visualizer: true,
            viz_preset: "equalizer-bleed",
            ..TaskbarStyle::new("visualizer", "Visualizer", "Lyrics with the equalizer rising behind the taskbar")
        },
        TaskbarStyle::new("minimal", "Minimal", "Just the lyrics on the taskbar"),
        TaskbarStyle {
            app_name: true,
            ..TaskbarStyle::new("appname", "App name", "Lyrics plus the focused app's name, menu-bar style")
        },
        TaskbarStyle {
            app_name: true,
            visualizer: true,
            viz_preset: "glow-taskbar",
            ..TaskbarStyle::new("full", "Everything", "Lyrics, app name and a soft glow on the taskbar")
        },
        TaskbarStyle { placement: "above", ..TaskbarStyle::new("floating", "Floating", "Lyrics float just above the taskbar") },
    ]
}

fn surface_of(c: &mut Config, surface: Surface) -> &mut WallpaperSettings {
    match surface {
        Surface::Lock => &mut c.wallpaper_lock,
        _ => &mut c.wallpaper_desktop,
    }
}

/// Apply style `id` of `surface` to `c`. `false` when there is no such style.
pub fn apply(c: &mut Config, surface: Surface, id: &str) -> bool {
    match surface {
        Surface::Taskbar => match taskbar().into_iter().find(|s| s.id == id) {
            Some(style) => {
                style.apply_to(c);
                true
            }
            None => false,
        },
        Surface::Desktop | Surface::Lock => {
            let list = if surface == Surface::Lock { lock() } else { desktop() };
            match list.into_iter().find(|s| s.id == id) {
                Some(style) => {
                    style.apply_to(surface_of(c, surface));
                    true
                }
                None => false,
            }
        }
    }
}

/// The style `c` currently matches on `surface`, or `None` for "Custom".
pub fn active(c: &Config, surface: Surface) -> Option<&'static str> {
    match surface {
        Surface::Taskbar => taskbar().into_iter().find(|s| s.matches(c)).map(|s| s.id),
        Surface::Desktop => desktop().into_iter().find(|s| s.matches(&c.wallpaper_desktop)).map(|s| s.id),
        Surface::Lock => lock().into_iter().find(|s| s.matches(&c.wallpaper_lock)).map(|s| s.id),
    }
}

/// The whole catalogue as the page wants it (looks in the config's PascalCase keys).
pub fn catalogue_json() -> serde_json::Value {
    serde_json::json!({
        "taskbar": taskbar(),
        "desktop": desktop(),
        "lock": lock(),
        "vizPresets": VIZ_PRESETS.iter().map(|(id, name)| serde_json::json!({"id": id, "name": name})).collect::<Vec<_>>(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(surface: Surface) -> Vec<&'static str> {
        match surface {
            Surface::Taskbar => taskbar().iter().map(|s| s.id).collect(),
            Surface::Desktop => desktop().iter().map(|s| s.id).collect(),
            Surface::Lock => lock().iter().map(|s| s.id).collect(),
        }
    }

    #[test]
    fn a_fresh_install_starts_on_the_first_style_of_each_surface() {
        let c = Config::default();
        for surface in Surface::ALL {
            assert_eq!(active(&c, surface), Some(ids(surface)[0]), "{surface:?}");
        }
    }

    #[test]
    fn every_style_once_applied_is_active_and_unique() {
        for surface in Surface::ALL {
            let all = ids(surface);
            let mut seen = std::collections::HashSet::new();
            for id in &all {
                assert!(seen.insert(*id), "duplicate id {id} on {surface:?}");
                // From defaults and from every other style: the result is the same.
                for from in &all {
                    let mut c = Config::default();
                    assert!(apply(&mut c, surface, from));
                    assert!(apply(&mut c, surface, id));
                    assert_eq!(active(&c, surface), Some(*id), "{from} -> {id} on {surface:?}");
                }
            }
        }
    }

    #[test]
    fn changing_one_setting_after_a_style_shows_custom() {
        let mut c = Config::default();
        apply(&mut c, Surface::Desktop, "mirror");
        c.wallpaper_desktop.dim += 5;
        assert_eq!(active(&c, Surface::Desktop), None);

        let mut c = Config::default();
        apply(&mut c, Surface::Lock, "karaoke");
        c.wallpaper_lock.clock = true;
        assert_eq!(active(&c, Surface::Lock), None);

        let mut c = Config::default();
        apply(&mut c, Surface::Taskbar, "visualizer");
        c.viz_preset = "wave-bleed".into();
        assert_eq!(active(&c, Surface::Taskbar), None);

        // Randomize on: no visualizer style matches, since the look keeps changing.
        let mut c = Config::default();
        c.viz_randomize_on_track = true;
        assert_eq!(active(&c, Surface::Taskbar), None);
        // A style without the visualizer ignores the preset and randomize.
        apply(&mut c, Surface::Taskbar, "minimal");
        c.viz_preset = "pulse-bleed".into();
        assert_eq!(active(&c, Surface::Taskbar), Some("minimal"));
    }

    #[test]
    fn styles_leave_personal_settings_alone() {
        let mut c = Config::default();
        c.wallpaper_folder = r"D:\Pictures".into();
        c.wallpaper_desktop.collection = "nord".into();
        c.wallpaper_lock.collection = "city".into();
        c.v_pos = "top".into();
        c.align = "right".into();
        c.x_offset = 120;
        c.width = 900;
        c.app_name_align = "center".into();
        c.viz_edge = "top".into();
        c.viz_height_fraction = 0.5;
        let before = c.clone();

        for surface in Surface::ALL {
            for id in ids(surface) {
                apply(&mut c, surface, id);
                assert_eq!(c.wallpaper_folder, before.wallpaper_folder);
                assert_eq!(c.wallpaper_desktop.collection, "nord");
                assert_eq!(c.wallpaper_lock.collection, "city");
                assert_eq!((c.v_pos.as_str(), c.align.as_str(), c.x_offset, c.width), ("top", "right", 120, 900));
                assert_eq!(c.app_name_align, "center");
                assert_eq!((c.viz_edge.as_str(), c.viz_height_fraction), ("top", 0.5));
            }
        }
    }

    #[test]
    fn desktop_and_lock_styles_are_independent() {
        let mut c = Config::default();
        apply(&mut c, Surface::Desktop, "karaoke");
        assert_eq!(active(&c, Surface::Lock), Some("centred"));
        apply(&mut c, Surface::Lock, "corner");
        assert_eq!(active(&c, Surface::Desktop), Some("karaoke"));
    }

    #[test]
    fn unknown_style_changes_nothing() {
        let mut c = Config::default();
        assert!(!apply(&mut c, Surface::Desktop, "nope"));
        assert_eq!(c, Config::default());
    }

    #[test]
    fn taskbar_styles_only_name_known_visualizer_presets() {
        for s in taskbar() {
            assert!(VIZ_PRESETS.iter().any(|(id, _)| *id == s.viz_preset), "{}", s.id);
        }
        assert_eq!(Config::default().viz_preset, "equalizer-bleed");
    }
}
