//! Drives the accounts contract over the bus, the way settings will, in a temp
//! data folder. Needs no credentials: uses a made-up Spicy key. Does not run the
//! Spotify app connect (it would open the real browser).
//!
//!     cargo run -p syllable-lyrics --example accounts

use std::sync::Arc;
use std::time::{Duration, Instant};

use syllable_core::accounts::{self, AccountsAction, AccountsCommand};
use syllable_core::{ConfigStore, Context, Event, TrackInfo};

fn main() -> anyhow::Result<()> {
    let dir = std::env::temp_dir().join(format!("syllable-lyrics-accounts-{}", std::process::id()));
    let bus = syllable_core::Bus::new();
    let ctx = Context { bus: bus.clone(), config: ConfigStore::load_from(dir.join("config.json"), bus.clone()) };
    let events = bus.subscribe();
    let config = ctx.config.clone();
    syllable_lyrics::start_in(ctx, dir.clone())?;

    let s = accounts::current();
    assert!(s.ready && !s.has_spicy_key && !s.has_spotify());
    println!("initial: redirect uri {}", s.spotify_redirect_uri);
    assert_eq!(s.spotify_redirect_uri, "http://127.0.0.1:9013/callback");

    // Waits for an event matching `f`, skipping others.
    let wait = |what: &str, f: &dyn Fn(&Event) -> bool| -> Event {
        let end = Instant::now() + Duration::from_secs(40);
        while Instant::now() < end {
            if let Ok(e) = events.recv_timeout(Duration::from_millis(200)) {
                if f(&e) {
                    return e;
                }
            }
        }
        panic!("timed out waiting for {what}");
    };
    let result_for = |action: AccountsAction| {
        wait(&format!("{action:?} result"), &|e| {
            matches!(e, Event::AccountsChanged)
                && accounts::current().last_result.as_ref().is_some_and(|r| r.action == action)
                && !accounts::current().busy.contains(&action)
        });
        accounts::current().last_result.clone().unwrap()
    };

    // The redirect URI follows SpotifyRedirectPort, back and forth.
    for port in [9014u16, 9013] {
        config.update(|c| c.spotify_redirect_port = port);
        let want = format!("http://127.0.0.1:{port}/callback");
        wait("redirect uri", &|e| matches!(e, Event::AccountsChanged) && accounts::current().spotify_redirect_uri == want);
        println!("redirect uri follows config: {want}");
    }

    bus.publish(Event::AccountsCommand(AccountsCommand::TestSpicyKey("not-a-real-key".into())));
    let r = result_for(AccountsAction::TestSpicyKey);
    println!("test key: ok={} message={:?}", r.ok, r.message);
    assert!(!r.ok);

    // A song, then a (bogus) key: the song is asked again; the key fails; LRCLIB lines stay.
    bus.publish(Event::TrackChanged(Some(Arc::new(TrackInfo {
        title: "Blinding Lights".into(),
        artist: "The Weeknd".into(),
        duration_ms: 200_040.0,
        ..Default::default()
    }))));
    let first = wait("lyrics", &|e| matches!(e, Event::Lyrics(_)));
    println!("lyrics before the key: {:?}", lines(&first));

    bus.publish(Event::AccountsCommand(AccountsCommand::SetSpicyKey(Some("  bogus-key  ".into()))));
    wait("key set", &|e| matches!(e, Event::AccountsChanged) && accounts::current().has_spicy_key);
    // First the cached answer as an interim, then the final one once spicy-api has failed.
    let interim = wait("interim lyrics", &|e| matches!(e, Event::Lyrics(_)));
    println!("interim after the key: {:?}", lines(&interim));
    let again = wait("re-resolved lyrics", &|e| matches!(e, Event::Lyrics(_)));
    println!("final after the key: {:?}", lines(&again));
    assert_eq!(lines(&first), lines(&again), "a failing key leaves the LRCLIB answer");

    bus.publish(Event::AccountsCommand(AccountsCommand::SetSpicyKey(None)));
    wait("key removed", &|e| matches!(e, Event::AccountsChanged) && !accounts::current().has_spicy_key);

    bus.publish(Event::AccountsCommand(AccountsCommand::SpotifySignOut));
    let r = result_for(AccountsAction::SpotifySignOut);
    println!("sign out: ok={} message={:?}", r.ok, r.message);
    assert!(r.ok);

    println!("final state: {:?}", accounts::current());
    bus.publish(Event::Quit);
    std::thread::sleep(Duration::from_millis(300));
    let _ = std::fs::remove_dir_all(&dir);
    Ok(())
}

fn lines(e: &Event) -> Option<(usize, String)> {
    match e {
        Event::Lyrics(Some(l)) => Some((l.lines.len(), l.source.clone())),
        _ => None,
    }
}
