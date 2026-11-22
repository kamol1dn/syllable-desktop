//! Runs the taskbar module alone with the demo lyrics source, without the rest of the
//! app (and without its single-instance lock), on a private config file so the shared
//! one is left alone. The strips go on whichever edge the taskbar is on.
//! `cargo run -p syllable-taskbar --example demo -- [seconds] [pause|line] [Key=json ...]`
//! e.g. `Align="left" AppNameEnabled=false`. The real exe does the same with `SYLLABLE_DEMO=1`.

use syllable_core::{Bus, ConfigStore, Context, Event};

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let secs: u64 = args.first().and_then(|a| a.parse().ok()).unwrap_or(60);
    for mode in ["pause", "line"] {
        if args.iter().any(|a| a == mode) {
            std::env::set_var("SYLLABLE_DEMO", mode);
        }
    }

    // Private config: defaults, the taskbar's edge, then overrides from the command line.
    let path = std::env::temp_dir().join(format!("syllable-taskbar-demo-{}.json", std::process::id()));
    let edge = taskbar_edge();
    let mut json = serde_json::json!({ "VPos": edge, "AppNameEnabled": true, "AppNameVPos": edge });
    for a in &args {
        if let Some((k, v)) = a.split_once('=') {
            json[k] = serde_json::from_str(v).unwrap_or(serde_json::Value::String(v.into()));
        }
    }
    std::fs::write(&path, json.to_string())?;
    let bus = Bus::new();
    let ctx = Context { config: ConfigStore::load_from(path.clone(), bus.clone()), bus };
    println!("demo config {}: {json}", path.display());

    syllable_taskbar::start(ctx.clone())?;
    if std::env::var("SYLLABLE_DEMO").is_err() {
        syllable_taskbar::demo::spawn(ctx.clone());
    }
    // Live config changes: a line on stdin like `Align="right"` is applied.
    let live = ctx.clone();
    std::thread::spawn(move || {
        for line in std::io::stdin().lines().map_while(Result::ok) {
            if let Some((k, v)) = line.trim().split_once('=') {
                let mut value = serde_json::to_value(live.config.snapshot()).unwrap();
                value[k] = serde_json::from_str(v).unwrap_or(serde_json::Value::String(v.into()));
                if let Ok(c) = serde_json::from_value(value) {
                    live.config.update(|cfg| *cfg = c);
                    println!("applied {k}={v}");
                }
            }
        }
    });
    std::thread::sleep(std::time::Duration::from_secs(secs));
    ctx.bus.publish(Event::Quit);
    std::thread::sleep(std::time::Duration::from_millis(300));
    let _ = std::fs::remove_file(path);
    Ok(())
}

fn taskbar_edge() -> &'static str {
    use windows::core::w;
    use windows::Win32::Foundation::RECT;
    use windows::Win32::UI::WindowsAndMessaging::{FindWindowW, GetWindowRect};
    unsafe {
        let mut r = RECT::default();
        match FindWindowW(w!("Shell_TrayWnd"), None) {
            Ok(h) if GetWindowRect(h, &mut r).is_ok() && r.top <= 0 && r.bottom < 400 => "top",
            _ => "bottom",
        }
    }
}
