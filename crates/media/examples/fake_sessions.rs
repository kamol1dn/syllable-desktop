//! Stress aid (not part of the app): SMTC sessions of our own, created and destroyed
//! constantly (like Instagram reels in a browser), with titles, status, timeline and
//! thumbnails changing fast. Touches no real player.
//!
//!     cargo run -p syllable-media --example fake_sessions -- <seconds> [live] [swap_ms] [update_ms]
//!     cargo run -p syllable-media --example fake_sessions -- list
//!
//! `FAKE_INSTA=1` keeps the title "(1) Instagram" with no artist (lyrics then skip the
//! lookup). Used while hunting the SMTC double free (see the crash notes); to make the
//! app follow these sessions, start it with `SYLLABLE_MEDIA_FOLLOW=fake_sessions`.

use std::time::{Duration, Instant};

use windows::core::HSTRING;
use windows::Foundation::TimeSpan;
use windows::Media::Playback::MediaPlayer;
use windows::Media::{MediaPlaybackStatus, MediaPlaybackType, SystemMediaTransportControlsTimelineProperties};
use windows::Storage::Streams::{DataWriter, InMemoryRandomAccessStream, RandomAccessStreamReference};
use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};

fn bmp(w: u32, h: u32, seed: u32) -> Vec<u8> {
    let row = w * 4;
    let size = 54 + row * h;
    let mut f = Vec::with_capacity(size as usize);
    f.extend_from_slice(b"BM");
    f.extend_from_slice(&size.to_le_bytes());
    f.extend_from_slice(&[0; 4]);
    f.extend_from_slice(&54u32.to_le_bytes());
    f.extend_from_slice(&40u32.to_le_bytes());
    f.extend_from_slice(&w.to_le_bytes());
    f.extend_from_slice(&h.to_le_bytes());
    f.extend_from_slice(&1u16.to_le_bytes());
    f.extend_from_slice(&32u16.to_le_bytes());
    f.extend_from_slice(&[0; 24]);
    for y in 0..h {
        for x in 0..w {
            f.extend_from_slice(&[(x * 3 + seed) as u8, (y * 5 + seed * 7) as u8, (seed * 13) as u8, 255]);
        }
    }
    f
}

fn thumb(seed: u32) -> windows::core::Result<RandomAccessStreamReference> {
    let stream = InMemoryRandomAccessStream::new()?;
    let w = DataWriter::CreateDataWriter(&stream)?;
    w.WriteBytes(&bmp(96, 96, seed))?;
    w.StoreAsync()?.join()?;
    w.DetachStream()?;
    stream.Seek(0)?;
    RandomAccessStreamReference::CreateFromStream(&stream)
}

struct Fake {
    player: MediaPlayer,
    id: u32,
    born: Instant,
}

fn make(id: u32) -> windows::core::Result<Fake> {
    let player = MediaPlayer::new()?;
    player.CommandManager()?.SetIsEnabled(false)?;
    let smtc = player.SystemMediaTransportControls()?;
    smtc.SetIsEnabled(true)?;
    smtc.SetIsPlayEnabled(true)?;
    smtc.SetIsPauseEnabled(true)?;
    smtc.SetIsNextEnabled(true)?;
    smtc.SetPlaybackStatus(MediaPlaybackStatus::Playing)?;
    let f = Fake { player, id, born: Instant::now() };
    update(&f, 0)?;
    Ok(f)
}

fn update(f: &Fake, n: u32) -> windows::core::Result<()> {
    let smtc = f.player.SystemMediaTransportControls()?;
    let d = smtc.DisplayUpdater()?;
    d.SetType(MediaPlaybackType::Music)?;
    let m = d.MusicProperties()?;
    if std::env::var("FAKE_INSTA").is_ok() {
        m.SetTitle(&HSTRING::from("(1) Instagram"))?;
        m.SetArtist(&HSTRING::new())?;
    } else {
        m.SetTitle(&HSTRING::from(format!("Reel {} take {}", f.id, n / 3)))?;
        m.SetArtist(&HSTRING::from(if n % 5 == 0 { String::new() } else { format!("Fake Artist {}", f.id % 7) }))?;
    }
    if n % 4 == 0 {
        d.SetThumbnail(&thumb(f.id * 31 + n)?)?;
    }
    d.Update()?;
    smtc.SetPlaybackStatus(if n % 9 == 8 { MediaPlaybackStatus::Paused } else { MediaPlaybackStatus::Playing })?;
    let tl = SystemMediaTransportControlsTimelineProperties::new()?;
    let pos = f.born.elapsed().as_millis() as i64 * 10_000;
    tl.SetStartTime(TimeSpan { Duration: 0 })?;
    tl.SetEndTime(TimeSpan { Duration: 25 * 10_000_000 })?;
    tl.SetPosition(TimeSpan { Duration: pos % (25 * 10_000_000) })?;
    tl.SetMinSeekTime(TimeSpan { Duration: 0 })?;
    tl.SetMaxSeekTime(TimeSpan { Duration: 25 * 10_000_000 })?;
    smtc.UpdateTimelineProperties(&tl)?;
    Ok(())
}

fn main() -> windows::core::Result<()> {
    unsafe { CoInitializeEx(None, COINIT_MULTITHREADED).ok()? };
    if std::env::args().nth(1).as_deref() == Some("list") {
        use windows::Media::Control::GlobalSystemMediaTransportControlsSessionManager as M;
        let mgr = M::RequestAsync()?.join()?;
        for s in mgr.GetSessions()? {
            let p = s.TryGetMediaPropertiesAsync()?.join()?;
            println!("{} | {}", s.SourceAppUserModelId()?, p.Title()?);
        }
        return Ok(());
    }
    let a: Vec<u64> = std::env::args().skip(1).filter_map(|s| s.parse().ok()).collect();
    let secs = *a.first().unwrap_or(&120);
    let live = *a.get(1).unwrap_or(&3) as usize;
    let swap = Duration::from_millis(*a.get(2).unwrap_or(&1500));
    let step = Duration::from_millis(*a.get(3).unwrap_or(&150));
    let start = Instant::now();
    let mut next_id = 0;
    let mut fakes: Vec<Fake> = Vec::new();
    let mut last_swap = Instant::now();
    let (mut created, mut updates, mut errors) = (0u32, 0u32, 0u32);
    let mut n = 0u32;
    while start.elapsed() < Duration::from_secs(secs) {
        if fakes.len() < live || last_swap.elapsed() >= swap {
            if fakes.len() >= live {
                let old = fakes.remove(0);
                let _ = old.player.SystemMediaTransportControls().and_then(|s| s.SetPlaybackStatus(MediaPlaybackStatus::Closed));
                let _ = old.player.Close();
            }
            next_id += 1;
            match make(next_id) {
                Ok(f) => {
                    fakes.push(f);
                    created += 1;
                }
                Err(e) => {
                    errors += 1;
                    eprintln!("make failed: {e}");
                }
            }
            last_swap = Instant::now();
        }
        n += 1;
        for f in &fakes {
            match update(f, n) {
                Ok(()) => updates += 1,
                Err(_) => errors += 1,
            }
        }
        std::thread::sleep(step);
    }
    for f in fakes {
        let _ = f.player.Close();
    }
    println!("fakesmtc: {created} sessions created, {updates} updates, {errors} errors in {secs} s");
    Ok(())
}
