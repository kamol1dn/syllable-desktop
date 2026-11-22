//! The settings window on its own, for working on the page without the whole app:
//!
//!     cargo run -p syllable-settings --example settings_window [section]
//!
//! Uses the real config file (`%LOCALAPPDATA%\Syllable\config.json`; point
//! LOCALAPPDATA elsewhere to keep it apart). Logs every bus event; exits when the
//! window has been closed for two seconds.

use std::time::Duration;

use syllable_core::{Context, Event};

fn main() -> anyhow::Result<()> {
    let ctx = Context::new();
    let events = ctx.bus.subscribe();
    std::env::set_var("SYLLABLE_OPEN_SETTINGS", std::env::args().nth(1).unwrap_or_else(|| "1".into()));
    syllable_settings::start(ctx.clone())?;
    let mut idle = 0;
    loop {
        match events.recv_timeout(Duration::from_millis(500)) {
            Ok(event) => println!("bus: {event:?}"),
            Err(_) => {
                idle += 1;
                if idle % 4 == 0 && !window_open() {
                    break;
                }
            }
        }
    }
    ctx.bus.publish(Event::Quit);
    std::thread::sleep(Duration::from_millis(300));
    Ok(())
}

fn window_open() -> bool {
    use windows::core::w;
    use windows::Win32::UI::WindowsAndMessaging::FindWindowW;
    unsafe { FindWindowW(w!("SyllableSettings"), None).is_ok() }
}
