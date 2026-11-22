//! The user's wallpaper image folder: the listing sent to the page, and the
//! collections ("categories") a settings menu can offer.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

const EXTENSIONS: [&str; 7] = ["jpg", "jpeg", "png", "webp", "gif", "bmp", "avif"];
const MAX_FILES: usize = 5000;
const MAX_DEPTH: usize = 3;

/// Expand `%VAR%` references the way the prototype did (`Environment.ExpandEnvironmentVariables`).
fn expand(dir: &str) -> String {
    let mut out = String::new();
    let mut rest = dir.trim();
    while let Some(start) = rest.find('%') {
        let Some(len) = rest[start + 1..].find('%') else { break };
        let name = &rest[start + 1..start + 1 + len];
        out.push_str(&rest[..start]);
        match std::env::var(name) {
            Ok(v) if !name.is_empty() => out.push_str(&v),
            _ => out.push_str(&rest[start..start + len + 2]),
        }
        rest = &rest[start + len + 2..];
    }
    out.push_str(rest);
    out
}

fn is_image(p: &Path) -> bool {
    p.extension().and_then(|e| e.to_str()).is_some_and(|e| EXTENSIONS.iter().any(|x| x.eq_ignore_ascii_case(e)))
}

/// Image files in `dir` and its subfolders (3 levels), at most 5000. Empty when the
/// folder is blank, missing or unreadable.
pub fn list(dir: &str) -> Vec<String> {
    let root = PathBuf::from(expand(dir));
    let mut files = Vec::new();
    if dir.trim().is_empty() || !root.is_dir() {
        return files;
    }
    walk(&root, 0, &mut files);
    files
}

fn walk(dir: &Path, depth: usize, files: &mut Vec<String>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    let mut subdirs = Vec::new();
    for entry in entries.flatten() {
        if files.len() >= MAX_FILES {
            return;
        }
        let path = entry.path();
        match entry.file_type() {
            Ok(t) if t.is_dir() => subdirs.push(path),
            Ok(_) if is_image(&path) => files.push(path.to_string_lossy().into_owned()),
            _ => {}
        }
    }
    if depth < MAX_DEPTH {
        for sub in subdirs {
            walk(&sub, depth + 1, files);
        }
    }
}

/// Collections for the configured folder: subfolder names and filename prefixes
/// (`nord_forest.jpg` -> `nord`) holding at least 3 images, largest first, at most 12,
/// lower-case. The page matches them case-insensitively (`collection` setting).
pub fn categories(dir: &str) -> Vec<String> {
    let root = PathBuf::from(expand(dir));
    let mut counts: HashMap<String, usize> = HashMap::new();
    for f in list(dir) {
        let p = Path::new(&f);
        if let Some(stem) = p.file_stem().and_then(|s| s.to_str()) {
            if let Some(cut) = stem.find('_').filter(|&c| c > 0) {
                *counts.entry(stem[..cut].to_lowercase()).or_default() += 1;
            }
        }
        if let Some(parent) = p.parent() {
            if parent != root {
                if let Some(name) = parent.file_name().and_then(|s| s.to_str()) {
                    *counts.entry(name.to_lowercase()).or_default() += 1;
                }
            }
        }
    }
    let mut names: Vec<(String, usize)> = counts.into_iter().filter(|(_, n)| *n >= 3).collect();
    names.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    names.into_iter().take(12).map(|(k, _)| k).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn lists_images_recursively_and_finds_categories() {
        let root = std::env::temp_dir().join(format!("syllable-wp-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("Calm/deep")).unwrap();
        for f in ["nord_a.jpg", "nord_b.PNG", "nord_c.webp", "x.txt", "solo.gif", "Calm/1.jpg", "Calm/2.jpg", "Calm/deep/3.jpg"] {
            fs::write(root.join(f), b"x").unwrap();
        }
        let dir = root.to_string_lossy().into_owned();
        let files = list(&dir);
        assert_eq!(files.len(), 7, "{files:?}");
        assert!(files.iter().all(|f| !f.ends_with(".txt")));
        // "calm" has 2 direct images + "deep" counts its own folder; nord has 3 prefixed files.
        assert_eq!(categories(&dir), vec!["nord".to_string()]);
        fs::write(root.join("Calm/4.jpg"), b"x").unwrap();
        assert_eq!(categories(&dir), vec!["calm".to_string(), "nord".to_string()]);
        assert!(list("").is_empty());
        assert!(list(&root.join("missing").to_string_lossy()).is_empty());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn expands_environment_variables() {
        std::env::set_var("SYLLABLE_TEST_DIR", r"C:\Pics");
        assert_eq!(expand(r"%SYLLABLE_TEST_DIR%\Walls"), r"C:\Pics\Walls");
        assert_eq!(expand("%NOT_SET_ANYWHERE_X%"), "%NOT_SET_ANYWHERE_X%");
        assert_eq!(expand("100%"), "100%");
    }
}
