//! WASAPI loopback capture of the default render device.

use std::ffi::c_void;

use anyhow::{bail, Context as _, Result};
use windows::Win32::Media::Audio::{
    eConsole, eRender, IAudioCaptureClient, IAudioClient, IMMDeviceEnumerator, MMDeviceEnumerator,
    AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_LOOPBACK, WAVEFORMATEX,
};
use windows::Win32::System::Com::{CoCreateInstance, CoTaskMemFree, CLSCTX_ALL};

const WAVE_FORMAT_PCM: u16 = 1;
const WAVE_FORMAT_IEEE_FLOAT: u16 = 3;
const WAVE_FORMAT_EXTENSIBLE: u16 = 0xFFFE;
/// Buffer the device keeps for us between polls: 200 ms in 100-ns units.
const BUFFER_HNS: i64 = 2_000_000;

#[derive(Clone, Copy, Debug, PartialEq)]
enum SampleKind {
    F32,
    I16,
    I24,
    I32,
}

pub struct Capture {
    enumerator: IMMDeviceEnumerator,
    client: IAudioClient,
    capture: IAudioCaptureClient,
    device_id: String,
    pub sample_rate: u32,
    pub channels: usize,
    kind: SampleKind,
}

/// The id of the current default render device (to notice when it changes).
pub fn default_device_id(enumerator: &IMMDeviceEnumerator) -> Result<String> {
    unsafe {
        let device = enumerator.GetDefaultAudioEndpoint(eRender, eConsole)?;
        let id = device.GetId()?;
        let text = id.to_string().unwrap_or_default();
        CoTaskMemFree(Some(id.0 as *const c_void));
        Ok(text)
    }
}

impl Capture {
    /// Open the default render device in loopback mode and start capturing.
    /// The calling thread must have COM initialised.
    pub fn open() -> Result<Self> {
        unsafe {
            let enumerator: IMMDeviceEnumerator =
                CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).context("device enumerator")?;
            let device = enumerator.GetDefaultAudioEndpoint(eRender, eConsole).context("no default output device")?;
            let id = device.GetId()?;
            let device_id = id.to_string().unwrap_or_default();
            CoTaskMemFree(Some(id.0 as *const c_void));

            let client: IAudioClient = device.Activate(CLSCTX_ALL, None).context("activate audio client")?;
            let fmt = client.GetMixFormat().context("mix format")?;
            let parsed = parse_format(fmt);
            let init = client.Initialize(AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_LOOPBACK, BUFFER_HNS, 0, fmt, None);
            CoTaskMemFree(Some(fmt as *const c_void));
            let (sample_rate, channels, kind) = parsed?;
            init.context("initialise loopback")?;

            let capture: IAudioCaptureClient = client.GetService().context("capture client")?;
            client.Start().context("start capture")?;
            Ok(Self { enumerator, client, capture, device_id, sample_rate, channels, kind })
        }
    }

    pub fn device_id(&self) -> &str {
        &self.device_id
    }

    /// True when the system's default output is no longer the device we capture.
    pub fn default_changed(&self) -> bool {
        default_device_id(&self.enumerator).map(|id| id != self.device_id).unwrap_or(true)
    }

    /// Drain every packet the device has ready, handing each frame's mono mix to
    /// `sink`. Returns the number of frames read. Errors when the device went away.
    pub fn read(&mut self, mut sink: impl FnMut(f32)) -> Result<usize> {
        let mut total = 0;
        unsafe {
            loop {
                let packet = self.capture.GetNextPacketSize()?;
                if packet == 0 {
                    break;
                }
                let mut data: *mut u8 = std::ptr::null_mut();
                let mut frames = 0u32;
                let mut flags = 0u32;
                self.capture.GetBuffer(&mut data, &mut frames, &mut flags, None, None)?;
                let n = frames as usize;
                if flags & (AUDCLNT_BUFFERFLAGS_SILENT.0 as u32) != 0 || data.is_null() {
                    for _ in 0..n {
                        sink(0.0);
                    }
                } else {
                    let bytes = std::slice::from_raw_parts(data, n * self.channels * self.kind.bytes());
                    mix_to_mono(bytes, self.channels, self.kind, &mut sink);
                }
                self.capture.ReleaseBuffer(frames)?;
                total += n;
            }
        }
        Ok(total)
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        unsafe {
            let _ = self.client.Stop();
        }
    }
}

impl SampleKind {
    fn bytes(self) -> usize {
        match self {
            SampleKind::F32 | SampleKind::I32 => 4,
            SampleKind::I16 => 2,
            SampleKind::I24 => 3,
        }
    }
}

/// Read the mix format: rate, channels and sample encoding.
unsafe fn parse_format(fmt: *const WAVEFORMATEX) -> Result<(u32, usize, SampleKind)> {
    let f = std::ptr::read_unaligned(fmt);
    let tag = f.wFormatTag;
    let bits = f.wBitsPerSample;
    let channels = f.nChannels as usize;
    let rate = f.nSamplesPerSec;
    let is_float = match tag {
        WAVE_FORMAT_IEEE_FLOAT => true,
        WAVE_FORMAT_PCM => false,
        WAVE_FORMAT_EXTENSIBLE => {
            // WAVEFORMATEXTENSIBLE: SubFormat GUID at byte 24; its first field is the format tag.
            let sub = std::ptr::read_unaligned((fmt as *const u8).add(24) as *const u32);
            match sub as u16 {
                WAVE_FORMAT_IEEE_FLOAT => true,
                WAVE_FORMAT_PCM => false,
                other => bail!("unsupported sub-format {other:#x}"),
            }
        }
        other => bail!("unsupported format tag {other:#x}"),
    };
    let kind = match (is_float, bits) {
        (true, 32) => SampleKind::F32,
        (false, 16) => SampleKind::I16,
        (false, 24) => SampleKind::I24,
        (false, 32) => SampleKind::I32,
        _ => bail!("unsupported sample format: float={is_float} bits={bits}"),
    };
    if channels == 0 || rate == 0 {
        bail!("empty mix format");
    }
    Ok((rate, channels, kind))
}

fn mix_to_mono(bytes: &[u8], channels: usize, kind: SampleKind, sink: &mut impl FnMut(f32)) {
    let width = kind.bytes();
    for frame in bytes.chunks_exact(width * channels) {
        let mut sum = 0.0f32;
        for s in frame.chunks_exact(width) {
            sum += match kind {
                SampleKind::F32 => f32::from_le_bytes([s[0], s[1], s[2], s[3]]),
                SampleKind::I16 => i16::from_le_bytes([s[0], s[1]]) as f32 / 32_768.0,
                SampleKind::I24 => (i32::from_le_bytes([0, s[0], s[1], s[2]]) >> 8) as f32 / 8_388_608.0,
                SampleKind::I32 => i32::from_le_bytes([s[0], s[1], s[2], s[3]]) as f32 / 2_147_483_648.0,
            };
        }
        sink(sum / channels as f32);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mixes_interleaved_frames_to_mono() {
        let mut out = Vec::new();
        let bytes: Vec<u8> = [0.5f32, -0.5, 1.0, 0.0].iter().flat_map(|v| v.to_le_bytes()).collect();
        mix_to_mono(&bytes, 2, SampleKind::F32, &mut |v| out.push(v));
        assert_eq!(out, vec![0.0, 0.5]);

        out.clear();
        let bytes: Vec<u8> = [16_384i16, 16_384].iter().flat_map(|v| v.to_le_bytes()).collect();
        mix_to_mono(&bytes, 2, SampleKind::I16, &mut |v| out.push(v));
        assert_eq!(out, vec![0.5]);

        out.clear();
        mix_to_mono(&[0x00, 0x00, 0xC0], 1, SampleKind::I24, &mut |v| out.push(v));
        assert_eq!(out, vec![-0.5]);
    }
}
