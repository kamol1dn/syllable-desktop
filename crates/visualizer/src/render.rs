//! Direct2D drawing onto a DirectComposition swap chain with premultiplied alpha,
//! so the window has true per-pixel transparency and the GPU does the work.

use std::mem::ManuallyDrop;

use anyhow::{Context as _, Result};
use windows::core::Interface;
use windows::Win32::Foundation::{HMODULE, HWND};
use windows::Win32::Graphics::Direct2D::Common::{
    D2D1_ALPHA_MODE_PREMULTIPLIED, D2D1_COLOR_F, D2D1_FIGURE_BEGIN_FILLED, D2D1_FIGURE_END_CLOSED, D2D1_GRADIENT_STOP,
    D2D1_PIXEL_FORMAT, D2D_RECT_F, D2D_SIZE_U,
};
use windows::Win32::Graphics::Direct2D::{
    D2D1CreateFactory, ID2D1Bitmap1, ID2D1Brush, ID2D1DeviceContext, ID2D1Factory1, ID2D1RenderTarget, D2D1_ANTIALIAS_MODE_PER_PRIMITIVE,
    D2D1_BITMAP_OPTIONS_CANNOT_DRAW, D2D1_BITMAP_OPTIONS_CPU_READ, D2D1_BITMAP_OPTIONS_TARGET, D2D1_MAP_OPTIONS_READ, D2D1_BITMAP_PROPERTIES1, D2D1_DEVICE_CONTEXT_OPTIONS_NONE,
    D2D1_EXTEND_MODE_CLAMP, D2D1_FACTORY_OPTIONS, D2D1_FACTORY_TYPE_SINGLE_THREADED, D2D1_GAMMA_2_2,
    D2D1_LAYER_OPTIONS1_NONE, D2D1_LAYER_PARAMETERS1, D2D1_LINEAR_GRADIENT_BRUSH_PROPERTIES,
    D2D1_QUADRATIC_BEZIER_SEGMENT, D2D1_RADIAL_GRADIENT_BRUSH_PROPERTIES, D2D1_ROUNDED_RECT,
};
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE, D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_WARP};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_SDK_VERSION,
};
use windows::Win32::Graphics::DirectComposition::{
    DCompositionCreateDevice, IDCompositionDevice, IDCompositionTarget, IDCompositionVisual,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_ALPHA_MODE_PREMULTIPLIED, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::{
    IDXGIDevice, IDXGIFactory2, IDXGISurface, IDXGISwapChain1, DXGI_PRESENT, DXGI_SCALING_STRETCH,
    DXGI_SWAP_CHAIN_DESC1, DXGI_SWAP_CHAIN_FLAG, DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL, DXGI_USAGE_RENDER_TARGET_OUTPUT,
};
use windows_numerics::{Matrix3x2, Vector2};

use crate::palette::{Palette, Rgb};
use crate::preset::{Fade, Fill, Frame, Painter, Preset, Rect, Rgba};

pub struct Renderer {
    factory: ID2D1Factory1,
    dc: ID2D1DeviceContext,
    swap: IDXGISwapChain1,
    target: Option<ID2D1Bitmap1>,
    dcomp: IDCompositionDevice,
    _comp_target: IDCompositionTarget,
    _visual: IDCompositionVisual,
    width: u32,
    height: u32,
    palette_brush: Option<(Palette, u32, ID2D1Brush)>,
    /// Debug: grab the next frame's pixels (premultiplied BGRA) into `captured`.
    pub capture_next: bool,
    pub captured: Option<Vec<u8>>,
}

fn color(rgb: Rgb, alpha: f32) -> D2D1_COLOR_F {
    D2D1_COLOR_F { r: rgb.0 as f32 / 255.0, g: rgb.1 as f32 / 255.0, b: rgb.2 as f32 / 255.0, a: alpha }
}

fn rt(dc: &ID2D1DeviceContext) -> &ID2D1RenderTarget {
    dc
}

fn pt(x: f32, y: f32) -> Vector2 {
    Vector2 { X: x, Y: y }
}

fn d3d_device() -> Result<ID3D11Device> {
    let mut last = None;
    for driver in [D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_WARP] {
        match create_d3d(driver) {
            Ok(d) => return Ok(d),
            Err(e) => last = Some(e),
        }
    }
    Err(last.unwrap())
}

fn create_d3d(driver: D3D_DRIVER_TYPE) -> Result<ID3D11Device> {
    let mut device = None;
    unsafe {
        D3D11CreateDevice(
            None,
            driver,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            None,
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            None,
        )?;
    }
    device.context("no D3D device")
}

impl Renderer {
    pub fn new(hwnd: HWND, width: u32, height: u32) -> Result<Self> {
        let (width, height) = (width.max(1), height.max(1));
        unsafe {
            let d3d = d3d_device()?;
            let dxgi: IDXGIDevice = d3d.cast()?;
            let factory2: IDXGIFactory2 = dxgi.GetAdapter()?.GetParent()?;
            let desc = DXGI_SWAP_CHAIN_DESC1 {
                Width: width,
                Height: height,
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
                BufferCount: 2,
                Scaling: DXGI_SCALING_STRETCH,
                SwapEffect: DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL,
                AlphaMode: DXGI_ALPHA_MODE_PREMULTIPLIED,
                ..Default::default()
            };
            let swap = factory2.CreateSwapChainForComposition(&dxgi, &desc, None).context("swap chain")?;

            let factory: ID2D1Factory1 =
                D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, Some(&D2D1_FACTORY_OPTIONS::default()))?;
            let dc = factory.CreateDevice(&dxgi)?.CreateDeviceContext(D2D1_DEVICE_CONTEXT_OPTIONS_NONE)?;
            dc.SetAntialiasMode(D2D1_ANTIALIAS_MODE_PER_PRIMITIVE);

            let dcomp: IDCompositionDevice = DCompositionCreateDevice(&dxgi).context("composition device")?;
            let comp_target = dcomp.CreateTargetForHwnd(hwnd, true)?;
            let visual = dcomp.CreateVisual()?;
            visual.SetContent(&swap)?;
            comp_target.SetRoot(&visual)?;
            dcomp.Commit()?;

            let mut r = Self {
                factory,
                dc,
                swap,
                target: None,
                dcomp,
                _comp_target: comp_target,
                _visual: visual,
                width,
                height,
                palette_brush: None,
                capture_next: false,
                captured: None,
            };
            r.bind_target()?;
            Ok(r)
        }
    }

    fn bind_target(&mut self) -> Result<()> {
        unsafe {
            let surface: IDXGISurface = self.swap.GetBuffer(0)?;
            let props = D2D1_BITMAP_PROPERTIES1 {
                pixelFormat: D2D1_PIXEL_FORMAT { format: DXGI_FORMAT_B8G8R8A8_UNORM, alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED },
                dpiX: 96.0,
                dpiY: 96.0,
                bitmapOptions: D2D1_BITMAP_OPTIONS_TARGET | D2D1_BITMAP_OPTIONS_CANNOT_DRAW,
                colorContext: ManuallyDrop::new(None),
            };
            let bitmap = self.dc.CreateBitmapFromDxgiSurface(&surface, Some(&props))?;
            self.dc.SetTarget(&bitmap);
            self.target = Some(bitmap);
        }
        Ok(())
    }

    pub fn resize(&mut self, width: u32, height: u32) -> Result<()> {
        let (width, height) = (width.max(1), height.max(1));
        if (width, height) == (self.width, self.height) {
            return Ok(());
        }
        unsafe {
            self.dc.SetTarget(None);
            self.target = None;
            self.swap.ResizeBuffers(2, width, height, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SWAP_CHAIN_FLAG(0))?;
        }
        self.width = width;
        self.height = height;
        self.palette_brush = None;
        self.bind_target()
    }

    /// Paint one frame. `flip` mirrors the surface vertically (visualizer on the top
    /// edge); `opacity` dims everything. An error means the device was lost: drop
    /// this renderer and build a new one.
    pub fn draw(&mut self, preset: Option<&dyn Preset>, frame: &Frame, flip: bool, opacity: f32) -> Result<()> {
        let brush = self.palette_brush(frame.palette)?;
        unsafe {
            self.dc.BeginDraw();
            self.dc.Clear(Some(&D2D1_COLOR_F { r: 0.0, g: 0.0, b: 0.0, a: 0.0 }));
            let transform = if flip {
                Matrix3x2 { M11: 1.0, M12: 0.0, M21: 0.0, M22: -1.0, M31: 0.0, M32: self.height as f32 }
            } else {
                Matrix3x2::identity()
            };
            self.dc.SetTransform(&transform);
            if let Some(preset) = preset {
                let mut painter = D2dPainter { factory: &self.factory, dc: &self.dc, palette: &brush, errors: 0 };
                let dim = opacity < 0.999;
                if dim {
                    painter.begin_group(None, opacity);
                }
                preset.render(&mut painter, frame);
                if dim {
                    painter.end_group();
                }
            }
            self.dc.SetTransform(&Matrix3x2::identity());
            self.dc.EndDraw(None, None).context("draw")?;
            if self.capture_next {
                self.capture_next = false;
                self.captured = self.capture().ok();
            }
            self.swap.Present(1, DXGI_PRESENT(0)).ok().context("present")?;
        }
        Ok(())
    }

    /// The album gradient across the full width, rebuilt only when the palette or width changes.
    fn palette_brush(&mut self, palette: &Palette) -> Result<ID2D1Brush> {
        if let Some((p, w, b)) = &self.palette_brush {
            if p == palette && *w == self.width {
                return Ok(b.clone());
            }
        }
        let stops = [
            D2D1_GRADIENT_STOP { position: 0.0, color: color(palette.a, 1.0) },
            D2D1_GRADIENT_STOP { position: 0.5, color: color(palette.b, 1.0) },
            D2D1_GRADIENT_STOP { position: 1.0, color: color(palette.c, 1.0) },
        ];
        let brush: ID2D1Brush = unsafe {
            let coll = rt(&self.dc).CreateGradientStopCollection(&stops, D2D1_GAMMA_2_2, D2D1_EXTEND_MODE_CLAMP)?;
            self.dc
                .CreateLinearGradientBrush(
                    &D2D1_LINEAR_GRADIENT_BRUSH_PROPERTIES { startPoint: pt(0.0, 0.0), endPoint: pt(self.width as f32, 0.0) },
                    None,
                    &coll,
                )?
                .cast()?
        };
        self.palette_brush = Some((*palette, self.width, brush.clone()));
        Ok(brush)
    }

    #[cfg(debug_assertions)] // only the debug frame dump needs it
    pub fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    fn capture(&self) -> Result<Vec<u8>> {
        unsafe {
            let target = self.target.as_ref().context("no target")?;
            let props = D2D1_BITMAP_PROPERTIES1 {
                pixelFormat: D2D1_PIXEL_FORMAT { format: DXGI_FORMAT_B8G8R8A8_UNORM, alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED },
                dpiX: 96.0,
                dpiY: 96.0,
                bitmapOptions: D2D1_BITMAP_OPTIONS_CPU_READ | D2D1_BITMAP_OPTIONS_CANNOT_DRAW,
                colorContext: ManuallyDrop::new(None),
            };
            let size = D2D_SIZE_U { width: self.width, height: self.height };
            let bmp = self.dc.CreateBitmap(size, None, 0, &props)?;
            bmp.CopyFromBitmap(None, target, None)?;
            let mapped = bmp.Map(D2D1_MAP_OPTIONS_READ)?;
            let row = self.width as usize * 4;
            let mut out = Vec::with_capacity(row * self.height as usize);
            for y in 0..self.height as usize {
                let src = std::slice::from_raw_parts(mapped.bits.add(y * mapped.pitch as usize), row);
                out.extend_from_slice(src);
            }
            bmp.Unmap()?;
            Ok(out)
        }
    }

    pub fn commit(&self) {
        unsafe {
            let _ = self.dcomp.Commit();
        }
    }
}

struct D2dPainter<'a> {
    factory: &'a ID2D1Factory1,
    dc: &'a ID2D1DeviceContext,
    palette: &'a ID2D1Brush,
    errors: u32,
}

impl D2dPainter<'_> {
    fn brush(&mut self, fill: Fill) -> Option<ID2D1Brush> {
        let made: windows::core::Result<ID2D1Brush> = unsafe {
            match fill {
                Fill::Palette => return Some(self.palette.clone()),
                Fill::Solid(Rgba { rgb, alpha }) => self.dc.CreateSolidColorBrush(&color(rgb, alpha), None).and_then(|b| b.cast()),
                Fill::Radial { cx, cy, rx, ry, color: Rgba { rgb, alpha } } => {
                    let stops = [
                        D2D1_GRADIENT_STOP { position: 0.0, color: color(rgb, alpha) },
                        D2D1_GRADIENT_STOP { position: 1.0, color: color(rgb, 0.0) },
                    ];
                    rt(&self.dc).CreateGradientStopCollection(&stops, D2D1_GAMMA_2_2, D2D1_EXTEND_MODE_CLAMP).and_then(|coll| {
                        self.dc
                            .CreateRadialGradientBrush(
                                &D2D1_RADIAL_GRADIENT_BRUSH_PROPERTIES {
                                    center: pt(cx, cy),
                                    gradientOriginOffset: pt(0.0, 0.0),
                                    radiusX: rx.max(1.0),
                                    radiusY: ry.max(1.0),
                                },
                                None,
                                &coll,
                            )
                            .and_then(|b| b.cast())
                    })
                }
            }
        };
        made.map_err(|_| self.errors += 1).ok()
    }

    fn fade_brush(&mut self, fade: Fade) -> Option<ID2D1Brush> {
        let span = fade.bottom - fade.fade_top;
        let solid = if span <= 0.0 { 0.0 } else { ((fade.bottom - fade.solid_top) / span).clamp(0.0, 1.0) };
        let white = Rgb(255, 255, 255);
        let stops = [
            D2D1_GRADIENT_STOP { position: 0.0, color: color(white, 1.0) },
            D2D1_GRADIENT_STOP { position: solid, color: color(white, 1.0) },
            D2D1_GRADIENT_STOP { position: 1.0, color: color(white, 0.0) },
        ];
        // A zero-length gradient is undefined in Direct2D: nudge the end apart.
        let end = if span.abs() < 0.5 { fade.bottom - 0.5 } else { fade.fade_top };
        unsafe {
            rt(&self.dc)
                .CreateGradientStopCollection(&stops, D2D1_GAMMA_2_2, D2D1_EXTEND_MODE_CLAMP)
                .and_then(|coll| {
                    self.dc.CreateLinearGradientBrush(
                        &D2D1_LINEAR_GRADIENT_BRUSH_PROPERTIES { startPoint: pt(0.0, fade.bottom), endPoint: pt(0.0, end) },
                        None,
                        &coll,
                    )
                })
                .and_then(|b| b.cast())
                .map_err(|_| self.errors += 1)
                .ok()
        }
    }
}

const INFINITE_RECT: D2D_RECT_F = D2D_RECT_F { left: -f32::MAX, top: -f32::MAX, right: f32::MAX, bottom: f32::MAX };

impl Painter for D2dPainter<'_> {
    fn begin_group(&mut self, fade: Option<Fade>, opacity: f32) {
        let mask = fade.and_then(|f| self.fade_brush(f));
        let params = D2D1_LAYER_PARAMETERS1 {
            contentBounds: INFINITE_RECT,
            geometricMask: ManuallyDrop::new(None),
            maskAntialiasMode: D2D1_ANTIALIAS_MODE_PER_PRIMITIVE,
            maskTransform: Matrix3x2::identity(),
            opacity: opacity.clamp(0.0, 1.0),
            opacityBrush: ManuallyDrop::new(mask),
            layerOptions: D2D1_LAYER_OPTIONS1_NONE,
        };
        unsafe {
            self.dc.PushLayer(&params, None);
        }
        // The layer holds its own reference to the mask brush.
        let mut params = params;
        unsafe {
            ManuallyDrop::drop(&mut params.opacityBrush);
            ManuallyDrop::drop(&mut params.geometricMask);
        }
    }

    fn end_group(&mut self) {
        unsafe {
            self.dc.PopLayer();
        }
    }

    fn rect(&mut self, r: Rect, fill: Fill) {
        if let Some(b) = self.brush(fill) {
            unsafe {
                self.dc.FillRectangle(&D2D_RECT_F { left: r.x, top: r.y, right: r.x + r.w, bottom: r.y + r.h }, &b);
            }
        }
    }

    fn rounded_rect(&mut self, r: Rect, radius: f32, fill: Fill) {
        if let Some(b) = self.brush(fill) {
            let rr = D2D1_ROUNDED_RECT {
                rect: D2D_RECT_F { left: r.x, top: r.y, right: r.x + r.w, bottom: r.y + r.h },
                radiusX: radius,
                radiusY: radius,
            };
            unsafe {
                self.dc.FillRoundedRectangle(&rr, &b);
            }
        }
    }

    fn area(&mut self, top: &[(f32, f32)], bottom: f32, smooth: bool, fill: Fill) {
        if top.len() < 2 {
            return;
        }
        let Some(brush) = self.brush(fill) else { return };
        let geometry = unsafe {
            (|| -> windows::core::Result<_> {
                let path = self.factory.CreatePathGeometry()?;
                let sink = path.Open()?;
                let (x0, y0) = top[0];
                let (xn, yn) = top[top.len() - 1];
                sink.BeginFigure(pt(x0, bottom), D2D1_FIGURE_BEGIN_FILLED);
                sink.AddLine(pt(x0, y0));
                if smooth && top.len() > 2 {
                    let mid = |a: (f32, f32), b: (f32, f32)| pt((a.0 + b.0) / 2.0, (a.1 + b.1) / 2.0);
                    sink.AddLine(mid(top[0], top[1]));
                    for i in 1..top.len() - 1 {
                        sink.AddQuadraticBezier(&D2D1_QUADRATIC_BEZIER_SEGMENT {
                            point1: pt(top[i].0, top[i].1),
                            point2: mid(top[i], top[i + 1]),
                        });
                    }
                    sink.AddLine(pt(xn, yn));
                } else {
                    for &(x, y) in &top[1..] {
                        sink.AddLine(pt(x, y));
                    }
                }
                sink.AddLine(pt(xn, bottom));
                sink.EndFigure(D2D1_FIGURE_END_CLOSED);
                sink.Close()?;
                Ok(path)
            })()
        };
        match geometry {
            Ok(g) => unsafe { self.dc.FillGeometry(&g, &brush, None) },
            Err(_) => self.errors += 1,
        }
    }
}
