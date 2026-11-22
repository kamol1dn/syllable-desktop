//! Audio: WASAPI loopback capture of the default output device and spectrum
//! analysis, written to `syllable_core::audio` many times a second.
//!
//! Capture runs only while some consumer asks for it (`audio::demand`): the
//! visualizer while it is on, the wallpaper while it reacts to audio. With no
//! demand the device is released, `audio::latest()` returns `None`, and the thread
//! sleeps on a condition variable, so the app idles at zero CPU.
//!
//! The capture follows default-device changes (checked once a second) and reopens
//! after a device disappears.

mod analysis;
mod capture;

use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

use syllable_core::{audio, log, Context, Event};
use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};

pub use analysis::{band_map, smooth, Analyzer, BANDS, FFT_SIZE};

/// Analysis rate while audio is live.
const FRAME_INTERVAL: Duration = Duration::from_millis(16);
/// How long without audible samples before the output counts as silent.
const SILENT_AFTER: Duration = Duration::from_millis(1500);
/// Poll interval while capturing but silent (nothing is playing).
const QUIET_POLL: Duration = Duration::from_millis(60);
const LIVE_POLL: Duration = Duration::from_millis(8);
const REOPEN_DELAY: Duration = Duration::from_millis(1200);
const DEVICE_CHECK: Duration = Duration::from_secs(1);
/// Peak sample level below which a packet counts as silence (about -80 dB).
const AUDIBLE: f32 = 1e-4;

pub fn start(ctx: Context) -> anyhow::Result<()> {
    let events = ctx.bus.subscribe();
    std::thread::Builder::new().name("audio".into()).spawn(move || run(events))?;
    Ok(())
}

fn quit_seen(events: &Receiver<Event>) -> bool {
    loop {
        match events.try_recv() {
            Ok(Event::Quit) => return true,
            Ok(_) => continue,
            Err(std::sync::mpsc::TryRecvError::Empty) => return false,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => return true,
        }
    }
}

fn run(events: Receiver<Event>) {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }
    let mut analyzer = Analyzer::new(48_000);
    loop {
        if quit_seen(&events) {
            break;
        }
        if !audio::demanded() {
            audio::clear();
            // Wake now and then only to notice Quit; demand wakes us at once.
            audio::wait_demand(false, Duration::from_secs(1));
            continue;
        }
        match capture::Capture::open() {
            Ok(cap) => {
                log!("audio: capturing {} Hz, {} ch from {}", cap.sample_rate, cap.channels, cap.device_id());
                analyzer.set_sample_rate(cap.sample_rate);
                match run_capture(cap, &mut analyzer, &events) {
                    Ended::Quit => break,
                    Ended::NoDemand => log!("audio: stopped (nothing needs audio)"),
                    Ended::DeviceChanged => log!("audio: default output changed, reopening"),
                    Ended::Error(e) => {
                        log!("audio: capture lost: {e:#}");
                        sleep_unless_quit(&events, REOPEN_DELAY);
                    }
                }
            }
            Err(e) => {
                log!("audio: open failed: {e:#}");
                // No output device (or it is busy): retry, but slowly.
                sleep_unless_quit(&events, REOPEN_DELAY * 2);
            }
        }
        // Start fresh next time so stale bars do not flash.
        analyzer = Analyzer::new(analyzer.sample_rate());
    }
    audio::clear();
}

fn sleep_unless_quit(events: &Receiver<Event>, total: Duration) {
    let until = Instant::now() + total;
    while Instant::now() < until && audio::demanded() {
        if quit_seen(events) {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

enum Ended {
    Quit,
    NoDemand,
    DeviceChanged,
    Error(anyhow::Error),
}

fn run_capture(mut cap: capture::Capture, analyzer: &mut Analyzer, events: &Receiver<Event>) -> Ended {
    let started = Instant::now();
    let mut last_data = started;
    let mut last_frame = started;
    let mut last_device_check = started;
    let mut published_quiet = false;
    let rate = cap.sample_rate as f64;

    loop {
        if quit_seen(events) {
            return Ended::Quit;
        }
        if !audio::demanded() {
            return Ended::NoDemand;
        }
        let now = Instant::now();
        if now - last_device_check >= DEVICE_CHECK {
            last_device_check = now;
            if cap.default_changed() {
                return Ended::DeviceChanged;
            }
        }

        let mut peak = 0.0f32;
        let read = match cap.read(|s| {
            peak = peak.max(s.abs());
            analyzer.push(s)
        }) {
            Ok(n) => n,
            Err(e) => return Ended::Error(e),
        };
        let now = Instant::now();
        // Players often keep their stream open while paused and send digital
        // silence: only audible samples count as "live".
        if peak > AUDIBLE {
            last_data = now;
        }

        let dt = now - last_frame;
        let silent = now - last_data > SILENT_AFTER;
        if dt >= FRAME_INTERVAL {
            if read == 0 {
                // Nothing is rendering: the device sends no packets at all. Treat
                // the gap as silence so the bars fall instead of freezing.
                analyzer.push_silence((dt.as_secs_f64() * rate) as usize);
            }
            if !(silent && analyzer.is_quiet()) {
                audio::set(analyzer.update(dt.as_secs_f64() * 1000.0));
                published_quiet = false;
            } else if !published_quiet {
                // One last all-zero frame; readers then see an unchanging frame and idle.
                audio::set(analyzer.update(dt.as_secs_f64() * 1000.0));
                published_quiet = true;
            }
            last_frame = now;
        }
        std::thread::sleep(if silent { QUIET_POLL } else { LIVE_POLL });
    }
}
