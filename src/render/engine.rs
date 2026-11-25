use std::ffi::c_void;
use std::mem::ManuallyDrop;

use windows::core::{Interface, Result};
use windows_numerics::Vector2;
use windows::Win32::Foundation::{HGLOBAL, HMODULE};
use windows::Win32::Graphics::Direct2D::Common::*;
use windows::Win32::Graphics::Direct2D::*;
use windows::Win32::Graphics::Direct3D::*;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::DirectWrite::*;
use windows::Win32::Graphics::Dxgi::IDXGIDevice;
use windows::Win32::Graphics::Imaging::*;
use windows::Win32::System::Com::StructuredStorage::{CreateStreamOnHGlobal, IPropertyBag2};
use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, STATFLAG_DEFAULT, STATSTG, STREAM_SEEK_SET};
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;

/// Direct3D 11 + Direct2D device context shared by the asset generator, preview,
/// overlay, and SVG rendering.  Rendering is always done into a D2D target and
/// read back through a CPU_READ staging bitmap; WIC is the only raster codec.
pub struct Engine {
    pub factory: IWICImagingFactory,
    _d2d: ID2D1Device,
    pub ctx: ID2D1DeviceContext5,
    pub dwrite: IDWriteFactory,
    _d3d: ID3D11Device,
    _d3dctx: ID3D11DeviceContext,
    _dxgi: IDXGIDevice,
}

pub struct Target {
    pub bitmap: ID2D1Bitmap1,
    staging: ID2D1Bitmap1,
    pub width: u32,
    pub height: u32,
}

#[derive(Clone, Copy, Debug)]
pub struct Rgba {
    pub r: f32,
    pub g: f32,
    pub b: f32,
    pub a: f32,
}

impl Rgba {
    pub fn rgb(hex: u32) -> Self {
        Self {
            r: ((hex >> 16) & 0xFF) as f32 / 255.0,
            g: ((hex >> 8) & 0xFF) as f32 / 255.0,
            b: (hex & 0xFF) as f32 / 255.0,
            a: 1.0,
        }
    }
    pub fn alpha(self, a: f32) -> Self {
        Self { a: a.clamp(0.0, 1.0), ..self }
    }
    pub fn to_d2d(self) -> D2D1_COLOR_F {
        D2D1_COLOR_F { r: self.r, g: self.g, b: self.b, a: self.a }
    }
}

pub fn parse_hex(value: &str) -> u32 {
    let v = value.trim().trim_start_matches('#');
    u32::from_str_radix(v, 16).unwrap_or(0xFFFFFF)
}

impl Engine {
    pub fn new() -> Result<Self> {
        unsafe {
            let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
            let levels = [
                D3D_FEATURE_LEVEL_11_1,
                D3D_FEATURE_LEVEL_11_0,
                D3D_FEATURE_LEVEL_10_1,
                D3D_FEATURE_LEVEL_10_0,
            ];
            let flags = D3D11_CREATE_DEVICE_BGRA_SUPPORT;
            let mut device: Option<ID3D11Device> = None;
            let mut d3dctx: Option<ID3D11DeviceContext> = None;
            let mut level = D3D_FEATURE_LEVEL_11_0;
            let mut created = D3D11CreateDevice(
                None,
                D3D_DRIVER_TYPE_HARDWARE,
                HMODULE::default(),
                flags,
                Some(&levels),
                D3D11_SDK_VERSION,
                Some(&mut device),
                Some(&mut level),
                Some(&mut d3dctx),
            );
            if created.is_err() {
                device = None;
                d3dctx = None;
                created = D3D11CreateDevice(
                    None,
                    D3D_DRIVER_TYPE_WARP,
                    HMODULE::default(),
                    flags,
                    Some(&levels),
                    D3D11_SDK_VERSION,
                    Some(&mut device),
                    Some(&mut level),
                    Some(&mut d3dctx),
                );
            }
            created?;
            let device = device.ok_or_else(|| windows::core::Error::from(windows::Win32::Foundation::E_FAIL))?;
            let d3dctx = d3dctx.ok_or_else(|| windows::core::Error::from(windows::Win32::Foundation::E_FAIL))?;
            let dxgi: IDXGIDevice = device.cast()?;
            let d2d = D2D1CreateDevice(&dxgi, None)?;
            let base = d2d.CreateDeviceContext(D2D1_DEVICE_CONTEXT_OPTIONS_NONE)?;
            base.SetDpi(96.0, 96.0);
            let ctx: ID2D1DeviceContext5 = base.cast()?;
            let factory: IWICImagingFactory = CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER)?;
            let dwrite: IDWriteFactory = DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED)?;
            Ok(Self { factory, _d2d: d2d, ctx, dwrite, _d3d: device, _d3dctx: d3dctx, _dxgi: dxgi })
        }
    }

    pub fn target(&self, width: u32, height: u32) -> Result<Target> {
        unsafe {
            let pixel_format = D2D1_PIXEL_FORMAT {
                format: DXGI_FORMAT_B8G8R8A8_UNORM,
                alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
            };
            let rt_props = D2D1_BITMAP_PROPERTIES1 {
                pixelFormat: pixel_format,
                dpiX: 96.0,
                dpiY: 96.0,
                bitmapOptions: D2D1_BITMAP_OPTIONS_TARGET,
                colorContext: ManuallyDrop::new(None),
            };
            let staging_props = D2D1_BITMAP_PROPERTIES1 {
                pixelFormat: pixel_format,
                dpiX: 96.0,
                dpiY: 96.0,
                bitmapOptions: D2D1_BITMAP_OPTIONS_CPU_READ | D2D1_BITMAP_OPTIONS_CANNOT_DRAW,
                colorContext: ManuallyDrop::new(None),
            };
            let bitmap = self.ctx.CreateBitmap(D2D_SIZE_U { width, height }, None, 0, &rt_props)?;
            let staging = self.ctx.CreateBitmap(D2D_SIZE_U { width, height }, None, 0, &staging_props)?;
            Ok(Target { bitmap, staging, width, height })
        }
    }

    pub fn begin(&self, target: &Target, clear: Rgba) {
        unsafe {
            self.ctx.SetTarget(&target.bitmap);
            self.ctx.BeginDraw();
            self.ctx.SetDpi(96.0, 96.0);
            self.ctx.Clear(Some(&clear.to_d2d()));
        }
    }

    pub fn end(&self) -> Result<()> {
        unsafe {
            let result = self.ctx.EndDraw(None, None);
            self.ctx.SetTarget(None::<&ID2D1Image>);
            result
        }
    }

    pub fn brush(&self, color: Rgba) -> Result<ID2D1SolidColorBrush> {
        unsafe { self.ctx.CreateSolidColorBrush(&color.to_d2d(), None) }
    }

    /// Returns (straight BGRA, premultiplied BGRA).  The straight copy is used by
    /// WIC/PNG and tests; the premultiplied copy feeds UpdateLayeredWindow.
    pub fn readback(&self, target: &Target) -> Result<(Vec<u8>, Vec<u8>)> {
        unsafe {
            target.staging.CopyFromBitmap(None, &target.bitmap, None)?;
            let mapped = target.staging.Map(D2D1_MAP_OPTIONS_READ)?;
            let row = (target.width * 4) as usize;
            let mut premul = Vec::with_capacity(row * target.height as usize);
            for y in 0..target.height as usize {
                let src = std::slice::from_raw_parts(mapped.bits.add(y * mapped.pitch as usize), row);
                premul.extend_from_slice(src);
            }
            target.staging.Unmap()?;
            Ok((unpremultiply(&premul), premul))
        }
    }

    pub fn render_png<F>(&self, width: u32, height: u32, clear: Rgba, draw: F) -> Result<Vec<u8>>
    where
        F: FnOnce(&ID2D1DeviceContext5) -> Result<()>,
    {
        let target = self.target(width, height)?;
        self.begin(&target, clear);
        let draw_result = draw(&self.ctx);
        let end_result = self.end();
        draw_result?;
        end_result?;
        let (straight, _) = self.readback(&target)?;
        self.encode_png(width, height, &straight)
    }

    pub fn encode_png(&self, width: u32, height: u32, bgra: &[u8]) -> Result<Vec<u8>> {
        unsafe {
            let stream = CreateStreamOnHGlobal(HGLOBAL::default(), true)?;
            let encoder = self.factory.CreateEncoder(&GUID_ContainerFormatPng, std::ptr::null())?;
            encoder.Initialize(&stream, WICBitmapEncoderNoCache)?;
            let mut frame: Option<IWICBitmapFrameEncode> = None;
            let mut options: Option<IPropertyBag2> = None;
            encoder.CreateNewFrame(&mut frame, &mut options)?;
            let frame = frame.ok_or_else(|| windows::core::Error::from(windows::Win32::Foundation::E_FAIL))?;
            frame.Initialize(None::<&IPropertyBag2>)?;
            frame.SetSize(width, height)?;
            let mut pixel_format = GUID_WICPixelFormat32bppBGRA;
            frame.SetPixelFormat(&mut pixel_format)?;
            frame.WritePixels(height, width * 4, bgra)?;
            frame.Commit()?;
            encoder.Commit()?;
            let mut stat = STATSTG::default();
            stream.Stat(&mut stat, STATFLAG_DEFAULT)?;
            let size = stat.cbSize as usize;
            stream.Seek(0, STREAM_SEEK_SET, None)?;
            let mut out = vec![0u8; size];
            let mut read = 0u32;
            stream.Read(out.as_mut_ptr() as *mut c_void, size as u32, Some(&mut read)).ok()?;
            out.truncate(read as usize);
            Ok(out)
        }
    }

    // Shared with the app's image decoder; the generator only needs the PNG path.
    #[allow(dead_code)]
    pub fn create_bitmap(&self, width: u32, height: u32, straight_bgra: &[u8]) -> Result<ID2D1Bitmap1> {
        unsafe {
            let premul = premultiply(straight_bgra);
            let props = D2D1_BITMAP_PROPERTIES1 {
                pixelFormat: D2D1_PIXEL_FORMAT {
                    format: DXGI_FORMAT_B8G8R8A8_UNORM,
                    alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
                },
                dpiX: 96.0,
                dpiY: 96.0,
                bitmapOptions: D2D1_BITMAP_OPTIONS_NONE,
                colorContext: ManuallyDrop::new(None),
            };
            self.ctx.CreateBitmap(
                D2D_SIZE_U { width, height },
                Some(premul.as_ptr() as *const c_void),
                width * 4,
                &props,
            )
        }
    }

    // These text helpers are only used by the embedded asset generator; keep
    // them available there without compiling generator-only code into the app.
    #[allow(dead_code)]
    pub fn text(&self, font: &str, size: f32, text: &str, color: Rgba, width: f32, height: f32, weight: DWRITE_FONT_WEIGHT) -> Result<IDWriteTextLayout> {
        unsafe {
            let family: Vec<u16> = font.encode_utf16().chain(Some(0)).collect();
            let locale: Vec<u16> = "en-US".encode_utf16().chain(Some(0)).collect();
            let format = self.dwrite.CreateTextFormat(
                windows::core::PCWSTR(family.as_ptr()),
                None::<&IDWriteFontCollection>,
                weight,
                DWRITE_FONT_STYLE_NORMAL,
                DWRITE_FONT_STRETCH_NORMAL,
                size,
                windows::core::PCWSTR(locale.as_ptr()),
            ).map_err(|e| { eprintln!("[text] CreateTextFormat: {e:?}"); e })?;
            format.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_CENTER).map_err(|e| { eprintln!("[text] align: {e:?}"); e })?;
            format.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER).map_err(|e| { eprintln!("[text] para: {e:?}"); e })?;
            let _ = color;
            let utf16: Vec<u16> = text.encode_utf16().collect();
            self.dwrite.CreateTextLayout(&utf16, &format, width, height).map_err(|e| { eprintln!("[text] layout: {e:?}"); e })
        }
    }

    #[allow(dead_code)]
    pub fn draw_text(&self, layout: &IDWriteTextLayout, color: Rgba, x: f32, y: f32, color_font: bool) -> Result<()> {
        unsafe {
            let brush = self.brush(color)?;
            self.ctx.DrawTextLayout(
                Vector2 { X: x, Y: y },
                layout,
                &brush,
                None::<&ID2D1SvgGlyphStyle>,
                0,
                if color_font { D2D1_DRAW_TEXT_OPTIONS_ENABLE_COLOR_FONT } else { D2D1_DRAW_TEXT_OPTIONS_NONE },
            );
            Ok(())
        }
    }
}

pub fn unpremultiply(src: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(src.len());
    for px in src.chunks_exact(4) {
        let a = px[3] as u32;
        if a == 0 {
            out.extend_from_slice(&[0, 0, 0, 0]);
        } else {
            let b = (((px[0] as u32) * 255 + a / 2) / a).min(255) as u8;
            let g = (((px[1] as u32) * 255 + a / 2) / a).min(255) as u8;
            let r = (((px[2] as u32) * 255 + a / 2) / a).min(255) as u8;
            out.extend_from_slice(&[b, g, r, a as u8]);
        }
    }
    out
}

#[allow(dead_code)]
pub fn premultiply(src: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(src.len());
    for px in src.chunks_exact(4) {
        let a = px[3] as u32;
        out.push(((px[0] as u32 * a + 127) / 255) as u8);
        out.push(((px[1] as u32 * a + 127) / 255) as u8);
        out.push(((px[2] as u32 * a + 127) / 255) as u8);
        out.push(px[3]);
    }
    out
}
