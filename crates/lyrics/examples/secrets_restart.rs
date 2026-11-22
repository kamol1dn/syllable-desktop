//! Restart check for `secrets.dat` in the app's real data folder, through the same
//! `start` the app calls. Run each step as its own process (that is the restart):
//!
//!     cargo run -p syllable-lyrics --example secrets_restart -- set     # saves a dummy key
//!     cargo run -p syllable-lyrics --example secrets_restart -- check   # new process: key still there?
//!     cargo run -p syllable-lyrics --example secrets_restart -- clear   # removes it again
//!
//! Quit Syllable first: the app holds the same secrets in memory. The key is a
//! made-up value, never a real one.

use std::time::{Duration, Instant};

use syllable_core::accounts::{self, AccountsCommand};
use syllable_core::{config::data_dir, Context, Event};

const DUMMY: &str = "dummy-not-a-real-key-0000";

fn main() -> anyhow::Result<()> {
    let step = std::env::args().nth(1).unwrap_or_default();
    let ctx = Context::new();
    let events = ctx.bus.subscribe();
    syllable_lyrics::start(ctx.clone())?;
    let file = data_dir().join("secrets.dat");
    let wait_for = |want: bool| {
        let end = Instant::now() + Duration::from_secs(10);
        while Instant::now() < end {
            if accounts::current().has_spicy_key == want {
                return true;
            }
            let _ = events.recv_timeout(Duration::from_millis(100));
        }
        false
    };
    let ok = match step.as_str() {
        "set" => {
            ctx.bus.publish(Event::AccountsCommand(AccountsCommand::SetSpicyKey(Some(DUMMY.into()))));
            wait_for(true) && file.is_file()
        }
        "check" => {
            let loaded = wait_for(true);
            let raw = std::fs::read(&file).unwrap_or_default();
            let plain = raw.windows(DUMMY.len()).any(|w| w == DUMMY.as_bytes());
            println!("secrets.dat {} bytes, key loaded after restart: {loaded}, stored in plain text: {plain}", raw.len());
            loaded && !plain
        }
        "clear" => {
            ctx.bus.publish(Event::AccountsCommand(AccountsCommand::SetSpicyKey(None)));
            wait_for(false)
        }
        _ => anyhow::bail!("usage: secrets_restart set|check|clear"),
    };
    println!("{step}: {}", if ok { "ok" } else { "FAILED" });
    ctx.bus.publish(Event::Quit);
    std::thread::sleep(Duration::from_millis(200));
    std::process::exit(if ok { 0 } else { 1 });
}
