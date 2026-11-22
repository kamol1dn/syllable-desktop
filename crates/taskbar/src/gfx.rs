//! Drawing: Direct2D renders into a 32-bit premultiplied DIB, which
//! `UpdateLayeredWindow` hands to DWM as the window's per-pixel-alpha content. That
//! works the same for a layered child of the taskbar and for a floating top-level
//! window, and keeps the window click-through (layered + `WS_EX_TRANSPARENT`).

use std::ffi::c_void;

use windows::core::*;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Direct2D::Common::*;
use windows::Win32::Graphics::Direct2D::*;
use windows::Win32::Graphics::DirectWrite::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::UI::WindowsAndMessaging::*;

/// The process-wide (well, thread-wide: everything here lives on the strip thread)
/// Direct2D and DirectWrite factories.
pub struct Factories {
    pub d2d: ID2D1Factory,
    pub dwrite: IDWriteFactory,
}

impl Factories {
    pub fn new() -> Result<Self> {
        unsafe {
            let d2d: ID2D1Factory = D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None)?;
            let dwrite: IDWriteFactory = DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED)?;
            Ok(Self { d2d, dwrite })
        }
    }

    /// A single-line text format. Falls back to Segoe UI when the family is missing
    /// (DirectWrite would otherwise silently pick a system default).
    pub fn text_format(&self, family: &str, weight: DWRITE_FONT_WEIGHT, size_px: f32) -> Result<IDWriteTextFormat> {
        let family = if family_exists(&self.dwrite, family) { family } else { "Segoe UI" };
        let fam = HSTRING::from(family);
        unsafe {
            let f = self.dwrite.CreateTextFormat(
                &fam,
                None,
                weight,
                DWRITE_FONT_STYLE_NORMAL,
                DWRITE_FONT_STRETCH_NORMAL,
                size_px.max(4.0),
                w!(""),
            )?;
            f.SetWordWrapping(DWRITE_WORD_WRAPPING_NO_WRAP)?;
            f.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_NEAR)?;
            Ok(f)
        }
    }

    pub fn layout(&self, text: &str, format: &IDWriteTextFormat) -> Result<TextLayout> {
        let wide: Vec<u16> = text.encode_utf16().collect();
        unsafe {
            let layout = self.dwrite.CreateTextLayout(&wide, format, 100_000.0, 1000.0)?;
            let mut m = DWRITE_TEXT_METRICS::default();
            layout.GetMetrics(&mut m)?;
            Ok(TextLayout {
                width: m.widthIncludingTrailingWhitespace,
                height: m.height,
                layout,
            })
        }
    }
}

fn family_exists(dwrite: &IDWriteFactory, family: &str) -> bool {
    unsafe {
        let mut fonts = None;
        if dwrite.GetSystemFontCollection(&mut fonts, false).is_err() {
            return true;
        }
        let Some(fonts) = fonts else { return true };
        let mut index = 0;
        let mut exists = BOOL(0);
        let name = HSTRING::from(family);
        fonts.FindFamilyName(&name, &mut index, &mut exists).is_ok() && exists.as_bool()
    }
}

pub struct TextLayout {
    pub layout: IDWriteTextLayout,
    pub width: f32,
    pub height: f32,
}

impl TextLayout {
    /// X of the caret after UTF-16 position `pos` (its trailing edge).
    pub fn x_after(&self, pos: u32) -> f32 {
        if pos == 0 {
            return 0.0;
        }
        let (mut x, mut y) = (0.0f32, 0.0f32);
        let mut m = DWRITE_HIT_TEST_METRICS::default();
        unsafe {
            if self.layout.HitTestTextPosition(pos - 1, true, &mut x, &mut y, &mut m).is_err() {
                return self.width;
            }
        }
        x
    }
}

/// An offscreen 32-bit surface with a Direct2D render target bound to it.
pub struct Canvas {
    pub rt: ID2D1DCRenderTarget,
    dc: HDC,
    bitmap: HBITMAP,
    old: HGDIOBJ,
    bits: *mut c_void,
    pub width: i32,
    pub height: i32,
}

impl Canvas {
    pub fn new(f: &Factories, width: i32, height: i32) -> Result<Self> {
        unsafe {
            let props = D2D1_RENDER_TARGET_PROPERTIES {
                // Software: for a strip this small it costs no more CPU than the GPU
                // path (which reads every frame back into the DIB), and it never hits
                // device loss.
                r#type: D2D1_RENDER_TARGET_TYPE_SOFTWARE,
                pixelFormat: D2D1_PIXEL_FORMAT { format: DXGI_FORMAT_B8G8R8A8_UNORM, alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED },
                dpiX: 96.0,
                dpiY: 96.0,
                usage: D2D1_RENDER_TARGET_USAGE_NONE,
                minLevel: D2D1_FEATURE_LEVEL_DEFAULT,
            };
            let rt = f.d2d.CreateDCRenderTarget(&props)?;
            rt.SetTextAntialiasMode(D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE);
            let dc = CreateCompatibleDC(None);
            let mut canvas = Self { rt, dc, bitmap: HBITMAP::default(), old: HGDIOBJ::default(), bits: std::ptr::null_mut(), width: 0, height: 0 };
            canvas.resize(width, height)?;
            Ok(canvas)
        }
    }

    /// Re-size the backing bitmap. The render target (and every brush made from it)
    /// stays valid: it is only re-bound to the new bitmap.
    pub fn resize(&mut self, width: i32, height: i32) -> Result<()> {
        let (width, height) = (width.max(1), height.max(1));
        if width == self.width && height == self.height {
            return Ok(());
        }
        unsafe {
            let info = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: width,
                    biHeight: -height,
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB.0,
                    ..Default::default()
                },
                ..Default::default()
            };
            let mut bits: *mut c_void = std::ptr::null_mut();
            let bitmap = CreateDIBSection(Some(self.dc), &info, DIB_RGB_COLORS, &mut bits, None, 0)?;
            let prev = SelectObject(self.dc, bitmap.into());
            if self.bitmap.is_invalid() {
                self.old = prev;
            } else {
                let _ = DeleteObject(self.bitmap.into());
            }
            self.bitmap = bitmap;
            self.bits = bits;
            self.width = width;
            self.height = height;
            self.rt.BindDC(self.dc, &RECT { left: 0, top: 0, right: width, bottom: height })?;
        }
        Ok(())
    }

    pub fn begin(&self) {
        unsafe {
            self.rt.BeginDraw();
            self.rt.SetTransform(&identity());
            self.rt.Clear(Some(&D2D1_COLOR_F { r: 0.0, g: 0.0, b: 0.0, a: 0.0 }));
        }
    }

    /// Start a frame that only repaints columns `lo..hi`; the rest of the bitmap keeps
    /// the previous frame. Pop the extra clip before `end`.
    pub fn begin_partial(&self, lo: f32, hi: f32) {
        unsafe {
            self.rt.BeginDraw();
            self.rt.SetTransform(&identity());
            self.rt.PushAxisAlignedClip(
                &D2D_RECT_F { left: lo, top: 0.0, right: hi, bottom: self.height as f32 },
                D2D1_ANTIALIAS_MODE_ALIASED,
            );
            self.rt.Clear(Some(&D2D1_COLOR_F { r: 0.0, g: 0.0, b: 0.0, a: 0.0 }));
        }
    }

    /// Finish drawing. An error means the target must be rebuilt (device loss).
    pub fn end(&self) -> Result<()> {
        unsafe { self.rt.EndDraw(None, None) }
    }

    /// Give the pixels to `hwnd` (a layered window) with an overall opacity. With
    /// `dirty` (a column range), only that part is copied to DWM.
    pub fn present(&self, hwnd: HWND, opacity: f32, dirty: Option<(f32, f32)>) -> bool {
        let blend = BLENDFUNCTION {
            BlendOp: AC_SRC_OVER as u8,
            BlendFlags: 0,
            SourceConstantAlpha: (opacity.clamp(0.0, 1.0) * 255.0).round() as u8,
            AlphaFormat: AC_SRC_ALPHA as u8,
        };
        let size = SIZE { cx: self.width, cy: self.height };
        let src = POINT { x: 0, y: 0 };
        let rect = dirty.map(|(lo, hi)| RECT { left: lo as i32, top: 0, right: hi as i32, bottom: self.height });
        let info = UPDATELAYEREDWINDOWINFO {
            cbSize: std::mem::size_of::<UPDATELAYEREDWINDOWINFO>() as u32,
            hdcDst: HDC::default(),
            pptDst: std::ptr::null(),
            psize: &size,
            hdcSrc: self.dc,
            pptSrc: &src,
            crKey: COLORREF(0),
            pblend: &blend,
            dwFlags: ULW_ALPHA,
            prcDirty: rect.as_ref().map_or(std::ptr::null(), |r| r as *const RECT),
        };
        unsafe { UpdateLayeredWindowIndirect(hwnd, &info).as_bool() }
    }
}

impl Canvas {
    /// Debug aid: the premultiplied BGRA pixels as a 32-bit top-down BMP file.
    pub fn save_bmp(&self, path: &std::path::Path) -> std::io::Result<()> {
        if self.bits.is_null() {
            return Ok(());
        }
        let len = (self.width * self.height * 4) as usize;
        let px = unsafe { std::slice::from_raw_parts(self.bits as *const u8, len) };
        let mut f = Vec::with_capacity(len + 54);
        f.extend_from_slice(b"BM");
        f.extend_from_slice(&(54 + len as u32).to_le_bytes());
        f.extend_from_slice(&[0; 4]);
        f.extend_from_slice(&54u32.to_le_bytes());
        f.extend_from_slice(&40u32.to_le_bytes());
        f.extend_from_slice(&self.width.to_le_bytes());
        f.extend_from_slice(&(-self.height).to_le_bytes());
        f.extend_from_slice(&1u16.to_le_bytes());
        f.extend_from_slice(&32u16.to_le_bytes());
        f.extend_from_slice(&[0; 24]);
        f.extend_from_slice(px);
        std::fs::write(path, f)
    }
}

impl Drop for Canvas {
    fn drop(&mut self) {
        unsafe {
            SelectObject(self.dc, self.old);
            let _ = DeleteObject(self.bitmap.into());
            let _ = DeleteDC(self.dc);
        }
    }
}

pub fn identity() -> windows_numerics::Matrix3x2 {
    windows_numerics::Matrix3x2::identity()
}

pub fn white(a: f32) -> D2D1_COLOR_F {
    D2D1_COLOR_F { r: 1.0, g: 1.0, b: 1.0, a }
}

pub fn black(a: f32) -> D2D1_COLOR_F {
    D2D1_COLOR_F { r: 0.0, g: 0.0, b: 0.0, a }
}

/// A two-stop horizontal gradient whose stops are moved per frame (the karaoke sweep).
pub fn sweep_brush(rt: &ID2D1RenderTarget, sung: D2D1_COLOR_F, unsung: D2D1_COLOR_F) -> Result<ID2D1LinearGradientBrush> {
    unsafe {
        let stops = [
            D2D1_GRADIENT_STOP { position: 0.0, color: sung },
            D2D1_GRADIENT_STOP { position: 1.0, color: unsung },
        ];
        let coll = rt.CreateGradientStopCollection(&stops, D2D1_GAMMA_2_2, D2D1_EXTEND_MODE_CLAMP)?;
        rt.CreateLinearGradientBrush(
            &D2D1_LINEAR_GRADIENT_BRUSH_PROPERTIES {
                startPoint: windows_numerics::Vector2 { X: 0.0, Y: 0.0 },
                endPoint: windows_numerics::Vector2 { X: 1.0, Y: 0.0 },
            },
            None,
            &coll,
        )
    }
}

pub fn solid(rt: &ID2D1RenderTarget, c: D2D1_COLOR_F) -> Result<ID2D1SolidColorBrush> {
    unsafe { rt.CreateSolidColorBrush(&c, None) }
}

pub fn v2(x: f32, y: f32) -> windows_numerics::Vector2 {
    windows_numerics::Vector2 { X: x, Y: y }
}
