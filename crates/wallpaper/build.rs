//! Copies the wallpaper page (`assets/wallpaper/` in the workspace) to
//! `assets/wallpaper/` beside the built exe, where the desktop host loads it from.

use std::fs;
use std::path::{Path, PathBuf};

fn copy_dir(from: &Path, to: &Path) -> std::io::Result<()> {
    fs::create_dir_all(to)?;
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        let target = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir(&entry.path(), &target)?;
        } else {
            fs::copy(entry.path(), target)?;
        }
    }
    Ok(())
}

fn main() {
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let source = manifest.join("..").join("..").join("assets").join("wallpaper");
    println!("cargo:rerun-if-changed={}", source.display());
    // OUT_DIR is <target>/<profile>/build/<crate>-<hash>/out (or .../build/<crate>/<hash>/out);
    // the exe sits in <target>/<profile>, the parent of the nearest "build" folder.
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let Some(profile_dir) = out.ancestors().find(|p| p.file_name().is_some_and(|n| n == "build")).and_then(Path::parent) else {
        return;
    };
    let dest = profile_dir.join("assets").join("wallpaper");
    let _ = fs::remove_dir_all(&dest);
    if let Err(e) = copy_dir(&source, &dest) {
        println!("cargo:warning=copying the wallpaper page to {} failed: {e}", dest.display());
    }
}
