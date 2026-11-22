//! Decode cover bytes (JPEG/PNG/…) into a small RGBA thumbnail with WIC.

use anyhow::{Context as _, Result};
use windows::Win32::Graphics::Imaging::{
    CLSID_WICImagingFactory, GUID_WICPixelFormat32bppRGBA, IWICImagingFactory, WICBitmapDitherTypeNone,
    WICBitmapInterpolationModeFant, WICBitmapPaletteTypeCustom, WICDecodeMetadataCacheOnDemand,
};
use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED};

use crate::palette::Palette;

/// Longest side of the thumbnail the palette is computed from.
const THUMB: u32 = 96;

/// The palette of an encoded cover image; the default palette when it cannot be read.
pub fn palette_from_image(bytes: &[u8]) -> Palette {
    match thumbnail(bytes) {
        Ok((px, w, h)) => Palette::from_rgba(&px, w, h),
        Err(e) => {
            syllable_core::log!("visualizer: cover decode failed: {e:#}");
            Palette::default()
        }
    }
}

/// RGBA pixels of the image scaled down to at most `THUMB` on its longer side.
pub fn thumbnail(bytes: &[u8]) -> Result<(Vec<u8>, u32, u32)> {
    anyhow::ensure!(bytes.len() > 64, "image too small ({} bytes)", bytes.len());
    unsafe {
        // Harmless if this thread already joined COM.
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let factory: IWICImagingFactory = CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER)?;
        let stream = factory.CreateStream()?;
        stream.InitializeFromMemory(bytes).context("stream")?;
        let decoder = factory
            .CreateDecoderFromStream(&stream, std::ptr::null(), WICDecodeMetadataCacheOnDemand)
            .context("not an image")?;
        let frame = decoder.GetFrame(0)?;
        let (mut w, mut h) = (0u32, 0u32);
        frame.GetSize(&mut w, &mut h)?;
        anyhow::ensure!(w > 0 && h > 0, "empty image");
        let k = (THUMB as f64 / w.max(h) as f64).min(1.0);
        let (tw, th) = (((w as f64 * k) as u32).max(1), ((h as f64 * k) as u32).max(1));
        let scaler = factory.CreateBitmapScaler()?;
        scaler.Initialize(&frame, tw, th, WICBitmapInterpolationModeFant)?;
        let converter = factory.CreateFormatConverter()?;
        converter.Initialize(&scaler, &GUID_WICPixelFormat32bppRGBA, WICBitmapDitherTypeNone, None, 0.0, WICBitmapPaletteTypeCustom)?;
        let mut px = vec![0u8; (tw * th * 4) as usize];
        converter.CopyPixels(std::ptr::null(), tw * 4, &mut px)?;
        Ok((px, tw, th))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 2x1 PNG: one red pixel, one blue pixel (hand-built, zlib stored block).
    fn tiny_png() -> Vec<u8> {
        fn crc(data: &[u8]) -> u32 {
            let mut c = 0xFFFF_FFFFu32;
            for &b in data {
                c ^= b as u32;
                for _ in 0..8 {
                    c = if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
                }
            }
            !c
        }
        fn chunk(out: &mut Vec<u8>, kind: &[u8], data: &[u8]) {
            out.extend_from_slice(&(data.len() as u32).to_be_bytes());
            let mut body = kind.to_vec();
            body.extend_from_slice(data);
            out.extend_from_slice(&body);
            out.extend_from_slice(&crc(&body).to_be_bytes());
        }
        let raw = [0u8, 220, 20, 20, 20, 40, 220]; // filter byte + 2 RGB pixels
        let (mut a, mut b) = (1u32, 0u32);
        for &x in &raw {
            a = (a + x as u32) % 65_521;
            b = (b + a) % 65_521;
        }
        let mut z = vec![0x78, 0x01, 0x01, raw.len() as u8, 0, !(raw.len() as u8), 0xFF];
        z.extend_from_slice(&raw);
        z.extend_from_slice(&((b << 16) | a).to_be_bytes());
        let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
        chunk(&mut png, b"IHDR", &[0, 0, 0, 2, 0, 0, 0, 1, 8, 2, 0, 0, 0]);
        chunk(&mut png, b"IDAT", &z);
        chunk(&mut png, b"IEND", &[]);
        png
    }

    #[test]
    fn decodes_a_png_and_builds_its_palette() {
        let png = tiny_png();
        let (px, w, h) = thumbnail(&png).expect("decodes");
        assert_eq!((w, h), (2, 1));
        assert_eq!(&px[..4], &[220, 20, 20, 255]);
        let p = palette_from_image(&png);
        assert_ne!(p, Palette::default());
        assert!(p.a.0 > p.a.2 || p.a.2 > p.a.0, "picked a coloured hue: {:?}", p.a);
    }

    #[test]
    fn garbage_gives_the_default_palette() {
        assert_eq!(palette_from_image(&[0u8; 500]), Palette::default());
        assert_eq!(palette_from_image(b"short"), Palette::default());
    }
}

#[cfg(test)]
mod file_check {
    /// `SYLLABLE_TEST_COVER=<file> cargo test -p syllable-visualizer -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn print_palette_of_a_file() {
        let path = std::env::var("SYLLABLE_TEST_COVER").expect("set SYLLABLE_TEST_COVER");
        let bytes = std::fs::read(path).unwrap();
        let (px, w, h) = super::thumbnail(&bytes).unwrap();
        println!("thumb {w}x{h}, first pixel {:?}", &px[..4]);
        println!("{:?}", super::palette_from_image(&bytes));
    }
}
