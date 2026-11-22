//! Spectrum analysis: mono samples in, smoothed log-spaced bands out.
//!
//! Ported from the prototype's `AudioEngine.Update`: a 2048-point Hann-windowed FFT
//! over the newest samples, 64 log-spaced bands from ~30 Hz to ~16 kHz, square-root
//! compression with a gentle treble lift, and fast-attack / slow-decay smoothing so
//! bars snap up and glide down.

use std::sync::Arc;

use realfft::num_complex::Complex;
use realfft::{RealFftPlanner, RealToComplex};
use syllable_core::audio::AudioFrame;

pub const BANDS: usize = 64;
pub const FFT_SIZE: usize = 2048;
const RING_SIZE: usize = FFT_SIZE * 4;
/// How many of the lowest bands make up `bass`.
const BASS_BANDS: usize = 6;
/// Decay time constant: how fast bars glide back down.
const DECAY_MS: f64 = 220.0;
const BAND_GAIN: f32 = 9.0;
const LEVEL_GAIN: f32 = 6.0;
/// Values may overshoot 1 a little, as in the prototype; consumers clamp.
const MAX_VALUE: f32 = 1.4;
const LOW_HZ: f64 = 30.0;
const HIGH_HZ: f64 = 16_000.0;

/// Inclusive FFT-bin ranges `(lo, hi)` for each band, log-spaced from ~30 Hz to
/// ~16 kHz (or Nyquist). Ranges never overlap and always move up; at low
/// frequencies a band may be a single bin.
pub fn band_map(sample_rate: u32, fft_size: usize, bands: usize) -> Vec<(usize, usize)> {
    let nyquist = sample_rate as f64 / 2.0;
    let hi_hz = HIGH_HZ.min(nyquist);
    let bin_hz = sample_rate as f64 / fft_size as f64;
    let last_bin = fft_size / 2 - 1;
    let mut prev = ((LOW_HZ / bin_hz) as usize).max(1);
    let mut map = Vec::with_capacity(bands);
    for b in 0..bands {
        let frac = (b + 1) as f64 / bands as f64;
        let hz = LOW_HZ * (hi_hz / LOW_HZ).powf(frac);
        let bin = ((hz / bin_hz) as usize).clamp(prev.min(last_bin), last_bin);
        map.push((prev.min(bin), prev.max(bin).min(last_bin)));
        prev = bin + 1;
    }
    map
}

/// The decay factor for a frame `dt_ms` long.
pub fn decay_for(dt_ms: f64) -> f32 {
    (-dt_ms / DECAY_MS).exp() as f32
}

/// Fast attack, slow decay: rise to a louder value at once, glide toward a quieter one.
pub fn smooth(prev: f32, target: f32, decay: f32) -> f32 {
    if target > prev {
        target
    } else {
        prev * decay + target * (1.0 - decay)
    }
}

/// Turn a band's mean FFT magnitude into its 0..~1.4 display value.
pub fn band_value(mean_magnitude: f32, band: usize, bands: usize) -> f32 {
    let lift = 0.6 + 1.4 * band as f32 / bands as f32;
    (mean_magnitude.sqrt() * BAND_GAIN * lift).clamp(0.0, MAX_VALUE)
}

pub struct Analyzer {
    ring: Vec<f32>,
    write: usize,
    sample_rate: u32,
    fft: Arc<dyn RealToComplex<f32>>,
    window: Vec<f32>,
    input: Vec<f32>,
    spectrum: Vec<Complex<f32>>,
    scratch: Vec<Complex<f32>>,
    map: Vec<(usize, usize)>,
    bands: Vec<f32>,
    bass: f32,
    level: f32,
}

impl Analyzer {
    pub fn new(sample_rate: u32) -> Self {
        let fft = RealFftPlanner::<f32>::new().plan_fft_forward(FFT_SIZE);
        let window = (0..FFT_SIZE)
            .map(|i| (0.5 * (1.0 - (2.0 * std::f64::consts::PI * i as f64 / (FFT_SIZE - 1) as f64).cos())) as f32)
            .collect();
        Self {
            ring: vec![0.0; RING_SIZE],
            write: 0,
            sample_rate,
            input: fft.make_input_vec(),
            spectrum: fft.make_output_vec(),
            scratch: fft.make_scratch_vec(),
            fft,
            window,
            map: band_map(sample_rate, FFT_SIZE, BANDS),
            bands: vec![0.0; BANDS],
            bass: 0.0,
            level: 0.0,
        }
    }

    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    /// A new device may run at another rate: remap the bands, keep the smoothed state.
    pub fn set_sample_rate(&mut self, sample_rate: u32) {
        if sample_rate != self.sample_rate && sample_rate > 0 {
            self.sample_rate = sample_rate;
            self.map = band_map(sample_rate, FFT_SIZE, BANDS);
        }
    }

    pub fn push(&mut self, mono: f32) {
        self.ring[self.write] = mono;
        self.write = (self.write + 1) % RING_SIZE;
    }

    /// Feed `count` samples of silence (the device sends nothing while no app plays).
    pub fn push_silence(&mut self, count: usize) {
        for _ in 0..count.min(RING_SIZE) {
            self.push(0.0);
        }
    }

    /// True once every smoothed value has decayed to (practically) nothing.
    pub fn is_quiet(&self) -> bool {
        self.level < 0.004 && self.bass < 0.004 && self.bands.iter().all(|&b| b < 0.004)
    }

    /// Analyse the newest samples and advance the smoothing by `dt_ms`.
    pub fn update(&mut self, dt_ms: f64) -> AudioFrame {
        let start = (self.write + RING_SIZE - FFT_SIZE) % RING_SIZE;
        let mut sum_sq = 0.0f64;
        for i in 0..FFT_SIZE {
            let s = self.ring[(start + i) % RING_SIZE];
            sum_sq += (s * s) as f64;
            self.input[i] = s * self.window[i];
        }
        let rms = (sum_sq / FFT_SIZE as f64).sqrt() as f32;

        if self.fft.process_with_scratch(&mut self.input, &mut self.spectrum, &mut self.scratch).is_err() {
            return self.frame();
        }

        let decay = decay_for(dt_ms);
        // The prototype's FFT scaled its output by 1/N; realfft does not.
        let norm = 1.0 / FFT_SIZE as f32;
        for (b, &(lo, hi)) in self.map.iter().enumerate() {
            let bins = &self.spectrum[lo..=hi];
            let mean = bins.iter().map(|c| c.norm()).sum::<f32>() * norm / bins.len() as f32;
            self.bands[b] = smooth(self.bands[b], band_value(mean, b, BANDS), decay);
        }

        let bass_raw = self.bands[..BASS_BANDS].iter().sum::<f32>() / BASS_BANDS as f32;
        self.bass = smooth(self.bass, bass_raw, decay);
        self.level = smooth(self.level, (rms * LEVEL_GAIN).clamp(0.0, MAX_VALUE), decay);
        self.frame()
    }

    pub fn frame(&self) -> AudioFrame {
        AudioFrame { bands: self.bands.clone(), bass: self.bass, level: self.level }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn band_of_hz(map: &[(usize, usize)], hz: f64, rate: u32) -> usize {
        let bin = (hz / (rate as f64 / FFT_SIZE as f64)).round() as usize;
        map.iter().position(|&(lo, hi)| bin >= lo && bin <= hi).expect("bin is in some band")
    }

    #[test]
    fn band_map_is_ordered_contiguous_and_in_range() {
        for rate in [44_100, 48_000, 96_000, 22_050] {
            let map = band_map(rate, FFT_SIZE, BANDS);
            assert_eq!(map.len(), BANDS);
            let bin_hz = rate as f64 / FFT_SIZE as f64;
            assert!(map[0].0 >= 1 && (map[0].0 as f64 * bin_hz) < 40.0f64.max(bin_hz + 1.0), "starts near 30 Hz at {rate}");
            let top_hz = map[BANDS - 1].1 as f64 * bin_hz;
            assert!(top_hz <= 16_100.0 && top_hz > 9_000.0f64.min(rate as f64 / 2.0 - 2.0 * bin_hz), "ends near 16 kHz/Nyquist at {rate}: {top_hz}");
            for (i, &(lo, hi)) in map.iter().enumerate() {
                assert!(lo <= hi && hi < FFT_SIZE / 2, "band {i} valid at {rate}");
                if i > 0 {
                    assert!(lo > map[i - 1].1, "band {i} starts after the previous one at {rate}");
                }
            }
        }
    }

    #[test]
    fn band_map_is_log_spaced() {
        let map = band_map(48_000, FFT_SIZE, BANDS);
        let width = |b: usize| map[b].1 - map[b].0 + 1;
        assert!(width(BANDS - 1) > 10 * width(20), "treble bands are much wider than mid bands");
        // 1 kHz should sit a bit past the middle of a 30 Hz..16 kHz log scale.
        let b = band_of_hz(&map, 1000.0, 48_000);
        assert!((30..45).contains(&b), "1 kHz in band {b}");
    }

    #[test]
    fn smoothing_attacks_instantly_and_decays_slowly() {
        let d = decay_for(16.0);
        assert!(d > 0.9 && d < 1.0);
        assert_eq!(smooth(0.2, 0.9, d), 0.9, "attack is immediate");
        let mut v = 1.0;
        for _ in 0..10 {
            v = smooth(v, 0.0, d);
        }
        // 160 ms at a 220 ms time constant: about exp(-160/220) = 0.48 left.
        assert!((v - (-160.0f32 / 220.0).exp()).abs() < 0.01, "decayed to {v}");
        // Frame-rate independent: one 160 ms step equals ten 16 ms steps.
        assert!((smooth(1.0, 0.0, decay_for(160.0)) - v).abs() < 1e-4);
    }

    #[test]
    fn band_value_is_compressed_lifted_and_clamped() {
        assert_eq!(band_value(0.0, 0, BANDS), 0.0);
        assert!(band_value(0.001, BANDS - 1, BANDS) > band_value(0.001, 0, BANDS), "treble lift");
        assert_eq!(band_value(10.0, 10, BANDS), MAX_VALUE);
    }

    #[test]
    fn a_sine_lights_its_own_band_and_silence_decays_to_quiet() {
        let rate = 48_000;
        let mut a = Analyzer::new(rate);
        let hz = 1000.0;
        for i in 0..FFT_SIZE * 2 {
            a.push((2.0 * std::f64::consts::PI * hz * i as f64 / rate as f64).sin() as f32 * 0.5);
        }
        let f = a.update(16.0);
        let peak = f.bands.iter().enumerate().max_by(|x, y| x.1.total_cmp(y.1)).unwrap().0;
        let expected = band_of_hz(&a.map, hz, rate);
        assert!(peak.abs_diff(expected) <= 1, "peak band {peak}, expected {expected}");
        assert!(f.bands[peak] > 0.5, "a loud tone shows: {}", f.bands[peak]);
        assert!(f.bands[0] < 0.2 && f.bands[BANDS - 1] < 0.2, "far bands stay low");
        assert!(f.level > 0.5);

        a.push_silence(FFT_SIZE);
        for _ in 0..200 {
            a.update(16.0);
        }
        assert!(a.is_quiet());
    }

    #[test]
    fn bass_follows_a_low_tone() {
        let rate = 44_100;
        let mut a = Analyzer::new(rate);
        for i in 0..FFT_SIZE {
            a.push((2.0 * std::f64::consts::PI * 60.0 * i as f64 / rate as f64).sin() as f32 * 0.8);
        }
        let f = a.update(16.0);
        assert!(f.bass > 0.3, "bass {}", f.bass);
    }
}
