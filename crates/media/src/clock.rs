//! Turns media-session samples into writes to the shared playback clock
//! (`syllable_core::position`). Pure logic with injected time so it can be tested.
//!
//! Why this is not just "set the clock from the timeline every poll":
//! - Players publish the timeline rarely (Spotify on play/pause/seek, browsers every
//!   few seconds). A sample carries the position and when it was taken, so a stale
//!   sample is extrapolated forward by `now - last_updated`.
//! - Only a *new* sample (position or update time differs from the previous one) may
//!   correct the clock. Recomputing from wall time on every poll would let a wall
//!   clock step move the lyrics.
//! - Small backward corrections are absorbed by running the clock 10% slow for a
//!   moment instead of jumping back, so a sung word never "un-sings". Forward
//!   corrections and real seeks apply at once.
//! - On a track change the session can still hold the previous song's timeline.
//!   Until a fresh sample arrives the clock runs from 0.
//! - Players without a timeline (position 0, no end, no update time) get our own
//!   clock: 0 at track start, frozen on pause, resumed on play.

/// A timeline as the session reports it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Timeline {
    pub position_ms: f64,
    /// Unix ms of when `position_ms` was true; `None` when the player gives none.
    pub updated_unix_ms: Option<f64>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Sample {
    pub playing: bool,
    /// Nominal playback rate (1.0 normally).
    pub rate: f64,
    /// `None` for players that report no timeline at all.
    pub timeline: Option<Timeline>,
}

/// What to hand to `position::set`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Write {
    pub position_ms: f64,
    pub playing: bool,
    pub rate: f64,
}

/// Differences smaller than this are timeline jitter: ignored.
const DEADBAND_MS: f64 = 40.0;
/// Backward corrections up to this size are slewed; larger ones are a seek.
const MAX_SLEW_MS: f64 = 1000.0;
/// While slewing the clock runs at `rate * (1 - SLEW)`.
const SLEW: f64 = 0.1;
/// Extrapolation is only trusted for samples younger than this.
const MAX_STALE_MS: f64 = 30.0 * 60_000.0;
/// After a track change, an unchanged timeline this close to 0 is taken as the new song's.
const NEW_TRACK_START_MS: f64 = 5000.0;
/// For this long after a jump, backward corrections apply at once instead of slewing.
const SETTLE_MS: f64 = 2000.0;

#[derive(Clone, Copy, Debug)]
struct Model {
    base_ms: f64,
    at_mono: f64,
    playing: bool,
    /// Rate actually written (slowed while slewing).
    rate: f64,
}

impl Model {
    fn now(&self, mono: f64) -> f64 {
        if self.playing {
            self.base_ms + (mono - self.at_mono) * self.rate
        } else {
            self.base_ms
        }
    }
}

#[derive(Default)]
pub struct Clock {
    model: Option<Model>,
    nominal_rate: f64,
    slew_until: Option<f64>,
    /// The raw timeline from the previous update, to tell fresh samples from repeats.
    last_raw: Option<Timeline>,
    /// Set by `track_changed`: the raw timeline that belonged to the previous song.
    stale_raw: Option<Option<Timeline>>,
    /// The clock was restarted from 0 for the current track change.
    zeroed: bool,
    /// Mono time of the last write that moved the clock (seek, resume, track change).
    jumped_at: Option<f64>,
}

impl Clock {
    pub fn new() -> Self {
        Self::default()
    }

    /// Forget everything (no session). The caller clears `position`.
    pub fn clear(&mut self) {
        *self = Self::default();
    }

    /// The song changed: until the timeline moves, it is the previous song's.
    pub fn track_changed(&mut self) {
        self.stale_raw = Some(self.last_raw);
        self.zeroed = false;
    }

    /// Mono time at which `update` must run again (end of a slew), if any.
    pub fn deadline(&self) -> Option<f64> {
        self.slew_until
    }

    /// The position the clock shows at `mono` (tests).
    #[cfg(test)]
    pub fn now(&self, mono: f64) -> Option<f64> {
        self.model.map(|m| m.now(mono))
    }

    /// Feed one sample. `mono` is a monotonic ms clock, `wall_unix_ms` the wall clock
    /// the session's update times are measured in. Returns the write to apply, if any.
    pub fn update(&mut self, s: &Sample, mono: f64, wall_unix_ms: f64) -> Option<Write> {
        let rate = if s.rate > 0.0 { s.rate } else { 1.0 };
        let fresh = s.timeline != self.last_raw;
        self.last_raw = s.timeline;

        let estimate = s.timeline.map(|tl| {
            let mut pos = tl.position_ms;
            if s.playing {
                if let Some(at) = tl.updated_unix_ms {
                    let stale = wall_unix_ms - at;
                    if stale > 0.0 && stale < MAX_STALE_MS {
                        pos += stale * rate;
                    }
                }
            }
            pos
        });

        // A track change still waiting for the new song's own timeline.
        if let Some(stale) = self.stale_raw {
            // A sample that moved since the change is the new song's. So is an
            // unchanged one that already sits near the start (the player published
            // it just before the new title).
            let own = s.timeline.is_some()
                && (s.timeline != stale || estimate.is_some_and(|e| e < NEW_TRACK_START_MS));
            if own {
                self.stale_raw = None;
                return self.set(estimate.unwrap_or(0.0), s.playing, rate, mono);
            }
            if !self.zeroed {
                self.zeroed = true;
                return self.set(0.0, s.playing, rate, mono);
            }
            return self.own_clock(s.playing, rate, mono);
        }
        let Some(m) = self.model else {
            return self.set(estimate.unwrap_or(0.0), s.playing, rate, mono);
        };

        if s.timeline.is_none() {
            return self.own_clock(s.playing, rate, mono);
        }
        let current = m.now(mono);
        let estimate = estimate.unwrap();

        if s.playing != m.playing {
            // Pausing: a fresh sample is the exact pause point (taken while stopped, so
            // `estimate` is already unextrapolated); otherwise freeze where we are.
            // Resuming: a fresh sample says where it resumed; otherwise carry on.
            let pos = if fresh { estimate } else { current };
            return self.set(pos, s.playing, rate, mono);
        }
        if (rate - self.nominal_rate).abs() > 1e-6 {
            let pos = if fresh { estimate } else { current };
            return self.set(pos, s.playing, rate, mono);
        }
        if fresh {
            if !s.playing {
                // Seek while paused.
                return if (estimate - current).abs() < DEADBAND_MS { None } else { self.set(estimate, false, rate, mono) };
            }
            let diff = estimate - current;
            if diff.abs() < DEADBAND_MS {
                return self.end_slew_if_due(mono);
            }
            // Right after a jump the player is still settling (Spotify's first sample
            // after a seek is often ~150 ms off): nothing has been sung since, so a
            // backward fix costs nothing and slewing would only stretch the error out.
            let settling = self.jumped_at.is_some_and(|t| mono - t < SETTLE_MS);
            if diff > 0.0 || -diff > MAX_SLEW_MS || settling {
                return self.set(estimate, true, rate, mono);
            }
            // Small backward correction: slow down until it is absorbed.
            let slowed = rate * (1.0 - SLEW);
            self.slew_until = Some(mono + -diff / (rate * SLEW));
            self.model = Some(Model { base_ms: current, at_mono: mono, playing: true, rate: slowed });
            return Some(Write { position_ms: current, playing: true, rate: slowed });
        }
        self.end_slew_if_due(mono)
    }

    fn end_slew_if_due(&mut self, mono: f64) -> Option<Write> {
        let until = self.slew_until?;
        if mono < until {
            return None;
        }
        let m = self.model?;
        // Land exactly where the slew was aiming, not wherever the late poll found us.
        let pos = m.now(until) + (mono - until) * self.nominal_rate;
        let w = self.set(pos, m.playing, self.nominal_rate, mono);
        self.jumped_at = None; // landing a slew is not a jump
        w
    }

    /// No usable timeline: keep our own clock, only reacting to pause/resume/rate.
    fn own_clock(&mut self, playing: bool, rate: f64, mono: f64) -> Option<Write> {
        let m = self.model?;
        if m.playing == playing && (rate - self.nominal_rate).abs() < 1e-6 {
            return None;
        }
        self.set(m.now(mono), playing, rate, mono)
    }

    fn set(&mut self, pos: f64, playing: bool, rate: f64, mono: f64) -> Option<Write> {
        let pos = pos.max(0.0);
        self.nominal_rate = rate;
        self.slew_until = None;
        self.jumped_at = Some(mono);
        self.model = Some(Model { base_ms: pos, at_mono: mono, playing, rate });
        Some(Write { position_ms: pos, playing, rate })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const WALL0: f64 = 1_700_000_000_000.0;

    fn tl(pos: f64, updated_at_mono: f64) -> Option<Timeline> {
        Some(Timeline { position_ms: pos, updated_unix_ms: Some(WALL0 + updated_at_mono) })
    }
    fn playing(timeline: Option<Timeline>) -> Sample {
        Sample { playing: true, rate: 1.0, timeline }
    }
    fn paused(timeline: Option<Timeline>) -> Sample {
        Sample { playing: false, rate: 1.0, timeline }
    }
    /// Wall and mono clocks agree in these tests (wall = WALL0 + mono).
    fn up(c: &mut Clock, s: Sample, mono: f64) -> Option<Write> {
        c.update(&s, mono, WALL0 + mono)
    }

    #[test]
    fn stale_timeline_is_extrapolated() {
        let mut c = Clock::new();
        // Sample taken at mono 1000 saying 10 s; we first see it at mono 4000.
        let w = up(&mut c, playing(tl(10_000.0, 1000.0)), 4000.0).unwrap();
        assert_eq!(w.position_ms, 13_000.0);
        assert!(w.playing);
        assert_eq!(c.now(5000.0), Some(14_000.0));
    }

    #[test]
    fn repeated_sample_never_moves_the_clock_even_if_wall_time_steps() {
        let mut c = Clock::new();
        up(&mut c, playing(tl(0.0, 0.0)), 0.0);
        // Same sample, but the wall clock jumped 5 s ahead (NTP step).
        assert_eq!(c.update(&playing(tl(0.0, 0.0)), 1000.0, WALL0 + 6000.0), None);
        assert_eq!(c.now(1000.0), Some(1000.0));
    }

    #[test]
    fn jitter_is_ignored_and_forward_corrections_jump() {
        let mut c = Clock::new();
        up(&mut c, playing(tl(0.0, 0.0)), 0.0);
        // Fresh sample 20 ms ahead: jitter.
        assert_eq!(up(&mut c, playing(tl(2020.0, 2000.0)), 2000.0), None);
        // 300 ms ahead: jump.
        let w = up(&mut c, playing(tl(4300.0, 4000.0)), 4000.0).unwrap();
        assert_eq!(w.position_ms, 4300.0);
    }

    #[test]
    fn small_backward_correction_slews_without_going_back() {
        let mut c = Clock::new();
        up(&mut c, playing(tl(0.0, 0.0)), 0.0);
        // Player says we are 200 ms ahead of it.
        let w = up(&mut c, playing(tl(9800.0, 10_000.0)), 10_000.0).unwrap();
        assert_eq!(w.position_ms, 10_000.0, "never writes a smaller position");
        assert!((w.rate - 0.9).abs() < 1e-9);
        let end = c.deadline().unwrap();
        assert!((end - 12_000.0).abs() < 1e-6, "200 ms at 10% takes 2 s");
        // Monotonic through the slew.
        let mut last = 10_000.0;
        for t in (10_000..12_000).step_by(100) {
            let now = c.now(t as f64).unwrap();
            assert!(now >= last);
            last = now;
        }
        // At the deadline it lands on the player's position and runs at 1.0 again.
        let w = up(&mut c, playing(tl(9800.0, 10_000.0)), 12_000.0).unwrap();
        assert!((w.position_ms - 11_800.0).abs() < 1e-6);
        assert_eq!(w.rate, 1.0);
        assert_eq!(c.deadline(), None);
    }

    #[test]
    fn backward_fix_right_after_a_seek_is_immediate() {
        let mut c = Clock::new();
        up(&mut c, playing(tl(0.0, 0.0)), 0.0);
        // Seek to 20 s; the first sample overshoots by 170 ms, the next one corrects it.
        up(&mut c, playing(tl(20_170.0, 10_000.0)), 10_000.0);
        let w = up(&mut c, playing(tl(20_250.0, 10_250.0)), 10_250.0).unwrap();
        assert_eq!(w.position_ms, 20_250.0);
        assert_eq!(w.rate, 1.0);
        assert_eq!(c.deadline(), None);
    }

    #[test]
    fn late_slew_end_still_lands_on_target() {
        let mut c = Clock::new();
        up(&mut c, playing(tl(0.0, 0.0)), 0.0);
        up(&mut c, playing(tl(9800.0, 10_000.0)), 10_000.0);
        // Woken 300 ms after the deadline.
        let w = up(&mut c, playing(tl(9800.0, 10_000.0)), 12_300.0).unwrap();
        assert!((w.position_ms - 12_100.0).abs() < 1e-6);
    }

    #[test]
    fn big_backward_jump_is_a_seek() {
        let mut c = Clock::new();
        up(&mut c, playing(tl(60_000.0, 0.0)), 0.0);
        let w = up(&mut c, playing(tl(5000.0, 1000.0)), 1000.0).unwrap();
        assert_eq!(w.position_ms, 5000.0);
        assert_eq!(w.rate, 1.0);
    }

    #[test]
    fn pause_uses_a_fresh_sample_or_freezes_in_place() {
        let mut c = Clock::new();
        up(&mut c, playing(tl(0.0, 0.0)), 0.0);
        // Paused, and the player published where (no extrapolation while paused).
        let w = up(&mut c, paused(tl(3050.0, 3000.0)), 3100.0).unwrap();
        assert_eq!(w, Write { position_ms: 3050.0, playing: false, rate: 1.0 });
        assert_eq!(c.now(9000.0), Some(3050.0));

        // Paused without a new sample: freeze where the clock is.
        let mut c = Clock::new();
        up(&mut c, playing(tl(0.0, 0.0)), 0.0);
        let w = up(&mut c, paused(tl(0.0, 0.0)), 3000.0).unwrap();
        assert_eq!(w.position_ms, 3000.0);
        assert!(!w.playing);
    }

    #[test]
    fn resume_continues_from_the_pause_point() {
        let mut c = Clock::new();
        up(&mut c, playing(tl(0.0, 0.0)), 0.0);
        up(&mut c, paused(tl(0.0, 0.0)), 3000.0);
        // Resumed 10 s later, the timeline not republished yet.
        let w = up(&mut c, playing(tl(0.0, 0.0)), 13_000.0).unwrap();
        assert_eq!(w.position_ms, 3000.0);
        assert_eq!(c.now(14_000.0), Some(4000.0));
        // Then the player confirms it.
        assert_eq!(up(&mut c, playing(tl(3010.0, 13_000.0)), 14_000.0), None);
    }

    #[test]
    fn seek_while_paused_moves_the_frozen_clock() {
        let mut c = Clock::new();
        up(&mut c, paused(tl(1000.0, 0.0)), 0.0);
        let w = up(&mut c, paused(tl(50_000.0, 500.0)), 600.0).unwrap();
        assert_eq!(w.position_ms, 50_000.0);
        assert!(!w.playing);
    }

    #[test]
    fn rate_changes_apply() {
        let mut c = Clock::new();
        up(&mut c, playing(tl(0.0, 0.0)), 0.0);
        let w = c.update(&Sample { playing: true, rate: 2.0, timeline: tl(0.0, 0.0) }, 1000.0, WALL0 + 1000.0).unwrap();
        assert_eq!(w.position_ms, 1000.0);
        assert_eq!(w.rate, 2.0);
        assert_eq!(c.now(2000.0), Some(3000.0));
    }

    #[test]
    fn track_change_ignores_the_previous_songs_timeline() {
        let mut c = Clock::new();
        up(&mut c, playing(tl(100_000.0, 0.0)), 0.0);
        c.track_changed();
        // Same old timeline: start the new song from 0.
        let w = up(&mut c, playing(tl(100_000.0, 0.0)), 5000.0).unwrap();
        assert_eq!(w.position_ms, 0.0);
        assert_eq!(up(&mut c, playing(tl(100_000.0, 0.0)), 5500.0), None);
        assert_eq!(c.now(6000.0), Some(1000.0));
        // The new song's own timeline arrives (started at an offset): taken as is.
        let w = up(&mut c, playing(tl(30_000.0, 6000.0)), 6000.0).unwrap();
        assert_eq!(w.position_ms, 30_000.0);
        // While it settles, a small backward step applies at once...
        let w = up(&mut c, playing(tl(30_900.0, 7000.0)), 7000.0).unwrap();
        assert_eq!(w.position_ms, 30_900.0);
        // ...and once settled, it slews.
        let w = up(&mut c, playing(tl(39_800.0, 16_000.0)), 16_000.0).unwrap();
        assert_eq!(w.position_ms, 39_900.0);
        assert!(w.rate < 1.0);
    }

    #[test]
    fn track_change_accepts_an_unchanged_timeline_near_zero() {
        let mut c = Clock::new();
        // The player published the new song's timeline before its title.
        up(&mut c, playing(tl(200.0, 0.0)), 0.0);
        c.track_changed();
        let w = up(&mut c, playing(tl(200.0, 0.0)), 700.0).unwrap();
        assert_eq!(w.position_ms, 900.0);
    }

    #[test]
    fn players_without_a_timeline_get_our_own_clock() {
        let mut c = Clock::new();
        let w = up(&mut c, playing(None), 0.0).unwrap();
        assert_eq!(w.position_ms, 0.0);
        assert_eq!(up(&mut c, playing(None), 500.0), None);
        let w = up(&mut c, paused(None), 4000.0).unwrap();
        assert_eq!(w.position_ms, 4000.0);
        assert!(!w.playing);
        let w = up(&mut c, playing(None), 9000.0).unwrap();
        assert_eq!(w.position_ms, 4000.0);
        assert_eq!(c.now(10_000.0), Some(5000.0));
        // Track change: back to 0.
        c.track_changed();
        assert_eq!(up(&mut c, playing(None), 11_000.0).unwrap().position_ms, 0.0);
    }

    #[test]
    fn very_old_or_missing_update_time_is_not_extrapolated() {
        let mut c = Clock::new();
        let s = playing(Some(Timeline { position_ms: 1000.0, updated_unix_ms: None }));
        assert_eq!(up(&mut c, s, 0.0).unwrap().position_ms, 1000.0);
        let mut c = Clock::new();
        let s = playing(Some(Timeline { position_ms: 1000.0, updated_unix_ms: Some(WALL0 - 2.0 * 3_600_000.0) }));
        assert_eq!(up(&mut c, s, 0.0).unwrap().position_ms, 1000.0);
    }
}
