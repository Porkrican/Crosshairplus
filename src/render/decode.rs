use std::ffi::c_void;
use std::io::Cursor;
use std::path::Path;

use windows::core::{PCWSTR, Result};
use windows::Win32::Foundation::GENERIC_READ;
use windows::Win32::Graphics::Direct2D::Common::D2D_SIZE_F;
use windows::Win32::Graphics::Direct2D::{
    ID2D1Bitmap1, ID2D1SvgDocument, D2D1_SVG_ATTRIBUTE_POD_TYPE_LENGTH, D2D1_SVG_ATTRIBUTE_POD_TYPE_VIEWBOX,
    D2D1_SVG_LENGTH, D2D1_SVG_LENGTH_UNITS_NUMBER, D2D1_SVG_VIEWBOX,
};
use windows_numerics::Matrix3x2;
use windows::Win32::Graphics::Imaging::{
    IWICBitmapDecoder, IWICBitmapFrameDecode, WICBitmapDitherTypeNone, WICBitmapPaletteTypeCustom,
    WICDecodeMetadataCacheOnDemand, GUID_WICPixelFormat32bppPBGRA,
};
use windows::Win32::System::Com::StructuredStorage::{
    PropVariantToUInt32, PROPVARIANT,
};
use windows::Win32::UI::Shell::SHCreateMemStream;

use super::engine::{unpremultiply, Engine, Rgba};

pub struct Frame {
    pub bitmap: ID2D1Bitmap1,
    pub delay_ms: u32,
}

pub struct ImageAsset {
    pub width: u32,
    pub height: u32,
    pub frames: Vec<Frame>,
    pub source_kind: String,
}

#[derive(Clone, Debug)]
pub struct ImageControls {
    pub invert: bool,
    pub solid: bool,
    pub tint: bool,
    pub solid_color: String,
    pub opacity: f32,
    pub gif_speed: f32,
}

impl Default for ImageControls {
    fn default() -> Self {
        Self { invert: false, solid: false, tint: false, solid_color: "#FFFFFF".into(), opacity: 100.0, gif_speed: 1.0 }
    }
}

const MAX_WEBP_FRAMES: u32 = 500;
const MAX_WEBP_DIMENSION: u32 = 4096;
/// WIC decodes GIF/PNG/JPG/BMP without a size guard of its own. The cap keeps a
/// hostile or accidental giant file from asking for gigabytes of bitmap memory.
const MAX_IMAGE_DIMENSION: u32 = 4096;
/// Same animation budget as WebP, so a 10 000-frame GIF cannot build 10 000
/// bitmaps before the overlay ever draws one.
const MAX_GIF_FRAMES: u32 = 500;

fn dimensions_allowed(width: u32, height: u32) -> bool {
    width > 0 && height > 0 && width <= MAX_IMAGE_DIMENSION && height <= MAX_IMAGE_DIMENSION
}
const WEBP_MEMORY_LIMIT: usize = 64 * 1024 * 1024;

fn webp_error(error: impl std::fmt::Display) -> windows::core::Error {
    windows::core::Error::new(windows::Win32::Foundation::E_FAIL, format!("WebP: {error}"))
}

fn webp_pixels_to_bgra(pixels: &[u8], has_alpha: bool) -> Vec<u8> {
    let mut out = Vec::with_capacity(pixels.len() / if has_alpha { 4 } else { 3 } * 4);
    if has_alpha {
        for px in pixels.chunks_exact(4) {
            out.extend_from_slice(&[px[2], px[1], px[0], px[3]]);
        }
    } else {
        for px in pixels.chunks_exact(3) {
            out.extend_from_slice(&[px[2], px[1], px[0], 255]);
        }
    }
    out
}

/// image-webp 0.2.4 only clears a frame that disposes to the background when
/// the caller supplies one. Without it the previous frame stays on the canvas
/// and shows through the next frame (the “static ghost” behind an animated
/// crosshair). Alpha animations use a transparent canvas, matching browsers;
/// opaque animations keep the file's ANIM background hint.
///
/// Remove this workaround once image-webp releases the upstream fix
/// (image-rs/image PR #171, issue #2913).
fn webp_canvas_color(has_alpha: bool, hint: Option<[u8; 4]>) -> [u8; 4] {
    if has_alpha {
        [0, 0, 0, 0]
    } else {
        hint.unwrap_or([0, 0, 0, 0])
    }
}

fn decode_webp(engine: &Engine, path: &Path, controls: &ImageControls) -> Result<ImageAsset> {
    let bytes = std::fs::read(path).map_err(webp_error)?;
    let cursor = Cursor::new(bytes);
    let mut decoder = image_webp::WebPDecoder::new(cursor).map_err(webp_error)?;
    decoder.set_memory_limit(WEBP_MEMORY_LIMIT);
    let (width, height) = decoder.dimensions();
    if width == 0 || height == 0 || width > MAX_WEBP_DIMENSION || height > MAX_WEBP_DIMENSION {
        return Err(webp_error(format!("unsupported dimensions {width}x{height}")));
    }
    let animated = decoder.is_animated();
    let count = if animated { decoder.num_frames() } else { 1 };
    if count == 0 || count > MAX_WEBP_FRAMES {
        return Err(webp_error(format!("unsupported frame count {count}")));
    }
    if width as u64 * height as u64 * count as u64 > 64 * 1024 * 1024 {
        return Err(webp_error("animation is too large to decode safely"));
    }
    let output_size = decoder
        .output_buffer_size()
        .filter(|size| *size > 0 && *size <= WEBP_MEMORY_LIMIT)
        .ok_or_else(|| webp_error("frame is too large"))?;
    let mut buffer = vec![0u8; output_size];
    let has_alpha = decoder.has_alpha();
    if animated {
        let color = webp_canvas_color(has_alpha, decoder.background_color_hint());
        if let Err(err) = decoder.set_background_color(color) {
            // Decoding can still proceed; it only means the ghost workaround is
            // unavailable for this file. Never fail an otherwise valid import.
            crate::logging::log(&format!("webp: could not set the animation background: {err}"));
        }
    }
    let speed = controls.gif_speed.clamp(0.25, 2.0);
    let mut frames = Vec::with_capacity(count as usize);
    for _ in 0..count {
        let delay_ms = if animated {
            decoder.read_frame(&mut buffer).map_err(webp_error)?
        } else {
            decoder.read_image(&mut buffer).map_err(webp_error)?;
            0
        };
        let bgra = webp_pixels_to_bgra(&buffer, has_alpha);
        let processed = apply_controls(&bgra, controls);
        let bitmap = engine.create_bitmap(width, height, &processed)?;
        let delay_ms = if animated {
            ((delay_ms.max(1) as f32) / speed).round().max(20.0) as u32
        } else {
            0
        };
        frames.push(Frame { bitmap, delay_ms });
    }
    Ok(ImageAsset {
        width,
        height,
        frames,
        source_kind: if animated { "webp-animated".into() } else { "webp".into() },
    })
}

pub fn apply_controls(bgra: &[u8], controls: &ImageControls) -> Vec<u8> {
    let color = super::engine::parse_hex(&controls.solid_color);
    let cr = ((color >> 16) & 0xFF) as u8;
    let cg = ((color >> 8) & 0xFF) as u8;
    let cb = (color & 0xFF) as u8;
    let opacity = (controls.opacity / 100.0).clamp(0.0, 1.0);
    let mut out = Vec::with_capacity(bgra.len());
    for px in bgra.chunks_exact(4) {
        let mut b = px[0];
        let mut g = px[1];
        let mut r = px[2];
        let a = ((px[3] as f32) * opacity).round().clamp(0.0, 255.0) as u8;
        if controls.solid {
            b = cb;
            g = cg;
            r = cr;
        } else if controls.tint {
            // Multiply by the tint so brightness differences survive as shading
            // instead of flattening an opaque image to one solid block.
            b = ((b as u32 * cb as u32) / 255) as u8;
            g = ((g as u32 * cg as u32) / 255) as u8;
            r = ((r as u32 * cr as u32) / 255) as u8;
        } else if controls.invert {
            b = 255 - b;
            g = 255 - g;
            r = 255 - r;
        }
        if a == 0 {
            out.extend_from_slice(&[0, 0, 0, 0]);
        } else {
            out.extend_from_slice(&[b, g, r, a]);
        }
    }
    out
}

fn decode_raster_frame(engine: &Engine, decoder: &IWICBitmapDecoder, index: u32) -> Result<(u32, u32, Vec<u8>)> {
    unsafe {
        let frame: IWICBitmapFrameDecode = decoder.GetFrame(index)?;
        let mut width = 0u32;
        let mut height = 0u32;
        frame.GetSize(&mut width, &mut height)?;
        let converter = engine.factory.CreateFormatConverter()?;
        converter.Initialize(
            &frame,
            &GUID_WICPixelFormat32bppPBGRA,
            WICBitmapDitherTypeNone,
            None,
            0.0,
            WICBitmapPaletteTypeCustom,
        )?;
        let stride = width * 4;
        let mut buffer = vec![0u8; (stride * height) as usize];
        converter.CopyPixels(std::ptr::null(), stride, &mut buffer)?;
        Ok((width, height, unpremultiply(&buffer)))
    }
}

fn metadata_u32(frame: &IWICBitmapFrameDecode, query: &str) -> Option<u32> {
    unsafe {
        let reader = frame.GetMetadataQueryReader().ok()?;
        let wide: Vec<u16> = query.encode_utf16().chain(Some(0)).collect();
        let mut value = PROPVARIANT::default();
        if reader.GetMetadataByName(PCWSTR(wide.as_ptr()), &mut value).is_err() {
            return None;
        }
        let out = PropVariantToUInt32(&value).ok();
        let _ = windows::Win32::System::Com::StructuredStorage::PropVariantClear(&mut value);
        out
    }
}

fn blank_canvas(width: u32, height: u32) -> Vec<u8> {
    vec![0u8; (width * height * 4) as usize]
}

fn blend_frame(canvas: &mut [u8], canvas_w: u32, canvas_h: u32, frame: &[u8], fw: u32, fh: u32, left: u32, top: u32) {
    for y in 0..fh {
        let cy = top + y;
        if cy >= canvas_h {
            continue;
        }
        for x in 0..fw {
            let cx = left + x;
            if cx >= canvas_w {
                continue;
            }
            let si = ((y * fw + x) * 4) as usize;
            let di = ((cy * canvas_w + cx) * 4) as usize;
            let sa = frame[si + 3] as u32;
            if sa == 0 {
                continue;
            }
            if sa == 255 {
                canvas[di..di + 4].copy_from_slice(&frame[si..si + 4]);
                continue;
            }
            let da = canvas[di + 3] as u32;
            let out_a = sa + da * (255 - sa) / 255;
            for c in 0..3 {
                let sc = frame[si + c] as u32;
                let dc = canvas[di + c] as u32;
                canvas[di + c] = ((sc * sa + dc * da * (255 - sa) / 255) / out_a.max(1)) as u8;
            }
            canvas[di + 3] = out_a as u8;
        }
    }
}

fn decode_gif(engine: &Engine, path: &Path, controls: &ImageControls) -> Result<ImageAsset> {
    unsafe {
        let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        let decoder = engine.factory.CreateDecoderFromFilename(
            PCWSTR(wide.as_ptr()),
            None,
            GENERIC_READ,
            WICDecodeMetadataCacheOnDemand,
        )?;
        let count = decoder.GetFrameCount()?;
        if count == 0 {
            return Err(windows::core::Error::from(windows::Win32::Foundation::E_FAIL));
        }
        let first = decoder.GetFrame(0)?;
        let mut canvas_w = 0u32;
        let mut canvas_h = 0u32;
        first.GetSize(&mut canvas_w, &mut canvas_h)?;
        if !dimensions_allowed(canvas_w, canvas_h) {
            crate::logging::log(&format!("crosshair: refusing GIF with unsupported dimensions {canvas_w}x{canvas_h}"));
            return Err(windows::core::Error::from(windows::Win32::Foundation::E_FAIL));
        }
        if count > MAX_GIF_FRAMES {
            crate::logging::log(&format!("crosshair: refusing GIF with {count} frames (max {MAX_GIF_FRAMES})"));
            return Err(windows::core::Error::from(windows::Win32::Foundation::E_FAIL));
        }
        let mut canvas = blank_canvas(canvas_w, canvas_h);
        let mut previous_disposal = 0u32;
        let mut previous_rect = (0u32, 0u32, 0u32, 0u32);
        let mut saved_before_previous: Option<Vec<u8>> = None;
        let mut frames = Vec::new();
        for index in 0..count {
            let frame = decoder.GetFrame(index)?;
            let (fw, fh, pixels) = decode_raster_frame(engine, &decoder, index)?;
            let left = metadata_u32(&frame, "/imgdesc/Left").unwrap_or(0);
            let top = metadata_u32(&frame, "/imgdesc/Top").unwrap_or(0);
            let delay = metadata_u32(&frame, "/grctlext/Delay").unwrap_or(10).max(2);
            let disposal = metadata_u32(&frame, "/grctlext/Disposal").unwrap_or(0);
            if index > 0 {
                match previous_disposal {
                    2 => {
                        let (px, py, pw, ph) = previous_rect;
                        for y in py..(py + ph).min(canvas_h) {
                            for x in px..(px + pw).min(canvas_w) {
                                let i = ((y * canvas_w + x) * 4) as usize;
                                canvas[i..i + 4].fill(0);
                            }
                        }
                    }
                    3 => {
                        if let Some(saved) = &saved_before_previous {
                            canvas.copy_from_slice(saved);
                        }
                    }
                    _ => {}
                }
            }
            saved_before_previous = Some(canvas.clone());
            previous_rect = (left, top, fw, fh);
            previous_disposal = disposal;
            blend_frame(&mut canvas, canvas_w, canvas_h, &pixels, fw, fh, left, top);
            let processed = apply_controls(&canvas, controls);
            let bitmap = engine.create_bitmap(canvas_w, canvas_h, &processed)?;
            let speed = controls.gif_speed.clamp(0.25, 2.0);
            frames.push(Frame { bitmap, delay_ms: ((delay as f32 * 10.0) / speed).round().max(20.0) as u32 });
        }
        Ok(ImageAsset {
            width: canvas_w,
            height: canvas_h,
            frames,
            source_kind: "gif".into(),
        })
    }
}

unsafe fn svg_length(element: &windows::Win32::Graphics::Direct2D::ID2D1SvgElement, name: &str) -> Option<f32> {
    let name: Vec<u16> = name.encode_utf16().chain(Some(0)).collect();
    let mut length = D2D1_SVG_LENGTH::default();
    element
        .GetAttributeValue2(
            PCWSTR(name.as_ptr()),
            D2D1_SVG_ATTRIBUTE_POD_TYPE_LENGTH,
            &mut length as *mut _ as *mut c_void,
            std::mem::size_of::<D2D1_SVG_LENGTH>() as u32,
        )
        .ok()?;
    (length.units == D2D1_SVG_LENGTH_UNITS_NUMBER && length.value.is_finite() && length.value > 0.0)
        .then_some(length.value)
}

unsafe fn svg_intrinsic_size(document: &ID2D1SvgDocument) -> Option<(f32, f32)> {
    let root = document.GetRoot().ok()?;
    if let (Some(width), Some(height)) = (svg_length(&root, "width"), svg_length(&root, "height")) {
        return Some((width, height));
    }
    let name: Vec<u16> = "viewBox".encode_utf16().chain(Some(0)).collect();
    let mut viewbox = D2D1_SVG_VIEWBOX::default();
    root.GetAttributeValue2(
        PCWSTR(name.as_ptr()),
        D2D1_SVG_ATTRIBUTE_POD_TYPE_VIEWBOX,
        &mut viewbox as *mut _ as *mut c_void,
        std::mem::size_of::<D2D1_SVG_VIEWBOX>() as u32,
    )
    .ok()?;
    (viewbox.width.is_finite() && viewbox.height.is_finite() && viewbox.width > 0.0 && viewbox.height > 0.0)
        .then_some((viewbox.width, viewbox.height))
}

fn render_svg_rgba(engine: &Engine, bytes: &[u8], size: u32) -> Result<Vec<u8>> {
    let target = engine.target(size, size)?;
    engine.begin(&target, Rgba::rgb(0x000000).alpha(0.0));
    unsafe {
        let stream = SHCreateMemStream(Some(bytes)).ok_or_else(|| windows::core::Error::from(windows::Win32::Foundation::E_FAIL))?;
        let document = engine.ctx.CreateSvgDocument(&stream, D2D_SIZE_F { width: size as f32, height: size as f32 })?;
        // Direct2D draws an SVG document at its intrinsic size in the current
        // transform. Fit the intrinsic width/height (or viewBox) into the
        // requested square canvas so imported SVGs are not a tiny corner image.
        if let Some((width, height)) = svg_intrinsic_size(&document) {
            let scale = (size as f32 / width).min(size as f32 / height);
            if scale.is_finite() && scale > 0.0 {
                let offset_x = (size as f32 - width * scale) / 2.0;
                let offset_y = (size as f32 - height * scale) / 2.0;
                engine.ctx.SetTransform(&Matrix3x2 {
                    M11: scale,
                    M12: 0.0,
                    M21: 0.0,
                    M22: scale,
                    M31: offset_x,
                    M32: offset_y,
                });
            }
        }
        engine.ctx.DrawSvgDocument(&document);
        engine.ctx.SetTransform(&Matrix3x2 { M11: 1.0, M12: 0.0, M21: 0.0, M22: 1.0, M31: 0.0, M32: 0.0 });
    }
    engine.end()?;
    let (straight, _) = engine.readback(&target)?;
    Ok(straight)
}

fn decode_svg(engine: &Engine, path: &Path, controls: &ImageControls) -> Result<ImageAsset> {
    let bytes = std::fs::read(path).map_err(|e| windows::core::Error::new(windows::Win32::Foundation::E_FAIL, e.to_string()))?;
    let size = 512u32;
    let straight = render_svg_rgba(engine, &bytes, size)?;
    let processed = apply_controls(&straight, controls);
    let bitmap = engine.create_bitmap(size, size, &processed)?;
    Ok(ImageAsset {
        width: size,
        height: size,
        frames: vec![Frame { bitmap, delay_ms: 0 }],
        source_kind: "svg".into(),
    })
}

pub fn decode(engine: &Engine, path: &Path, controls: &ImageControls) -> Result<ImageAsset> {
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
    if ext == "svg" {
        return decode_svg(engine, path, controls);
    }
    if ext == "gif" {
        return decode_gif(engine, path, controls);
    }
    if ext == "webp" {
        return decode_webp(engine, path, controls);
    }
    unsafe {
        let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        let decoder = engine.factory.CreateDecoderFromFilename(
            PCWSTR(wide.as_ptr()),
            None,
            GENERIC_READ,
            WICDecodeMetadataCacheOnDemand,
        )?;
        let (width, height, pixels) = decode_raster_frame(engine, &decoder, 0)?;
        if !dimensions_allowed(width, height) {
            crate::logging::log(&format!("crosshair: refusing image with unsupported dimensions {width}x{height}"));
            return Err(windows::core::Error::from(windows::Win32::Foundation::E_FAIL));
        }
        let processed = apply_controls(&pixels, controls);
        let bitmap = engine.create_bitmap(width, height, &processed)?;
        Ok(ImageAsset {
            width,
            height,
            frames: vec![Frame { bitmap, delay_ms: 0 }],
            source_kind: ext,
        })
    }
}

use std::os::windows::ffi::OsStrExt;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rgba_and_rgb_convert_to_straight_bgra() {
        assert_eq!(webp_pixels_to_bgra(&[1, 2, 3, 4], true), vec![3, 2, 1, 4]);
        assert_eq!(webp_pixels_to_bgra(&[1, 2, 3], false), vec![3, 2, 1, 255]);
    }

    #[test]
    fn webp_canvas_color_keeps_alpha_animations_transparent() {
        assert_eq!(webp_canvas_color(true, Some([255, 255, 255, 255])), [0, 0, 0, 0]);
        assert_eq!(webp_canvas_color(false, Some([1, 2, 3, 255])), [1, 2, 3, 255]);
        assert_eq!(webp_canvas_color(false, None), [0, 0, 0, 0]);
    }

    #[test]
    fn animated_webp_disposed_frames_do_not_ghost() {
        let bytes = include_bytes!("../../assets/test.webp");
        let decode_frames = |configured: bool| {
            let mut decoder = image_webp::WebPDecoder::new(Cursor::new(bytes.as_slice())).unwrap();
            let has_alpha = decoder.has_alpha();
            if configured {
                let color = webp_canvas_color(has_alpha, decoder.background_color_hint());
                decoder.set_background_color(color).unwrap();
            }
            let size = decoder.output_buffer_size().unwrap();
            let count = decoder.num_frames();
            let mut frames = Vec::new();
            for _ in 0..count {
                let mut buffer = vec![0u8; size];
                decoder.read_frame(&mut buffer).unwrap();
                frames.push(buffer);
            }
            frames
        };
        let default = decode_frames(false);
        let fixed = decode_frames(true);
        assert_eq!(default.len(), fixed.len());
        let opaque = |frame: &[u8]| frame.chunks_exact(4).filter(|pixel| pixel[3] > 0).count();
        let default_opaque: usize = default.iter().map(|frame| opaque(frame)).sum();
        let fixed_opaque: usize = fixed.iter().map(|frame| opaque(frame)).sum();
        assert!(
            fixed_opaque < default_opaque,
            "the configured canvas should clear disposed pixels ({fixed_opaque} vs {default_opaque})"
        );
        let removed_ghost = default.iter().zip(&fixed).any(|(before, after)| {
            before
                .chunks_exact(4)
                .zip(after.chunks_exact(4))
                .any(|(old, new)| old[3] == 255 && new[3] == 0)
        });
        assert!(removed_ghost, "no opaque ghost pixel became transparent");
    }

    #[test]
    fn animated_webp_fixture_has_multiple_frames() {
        let bytes = include_bytes!("../../assets/test.webp");
        let mut decoder = image_webp::WebPDecoder::new(Cursor::new(bytes.as_slice())).unwrap();
        assert!(decoder.is_animated());
        assert!(decoder.num_frames() > 1);
        assert!(decoder.output_buffer_size().unwrap() > 0);
        let mut buffer = vec![0u8; decoder.output_buffer_size().unwrap()];
        let delay = decoder.read_frame(&mut buffer).unwrap();
        assert!(delay > 0);
        assert!(buffer.iter().any(|byte| *byte != 0));
        assert_eq!(webp_pixels_to_bgra(&buffer, decoder.has_alpha()).len(), buffer.len());
    }

    #[test]
    fn alpha_preserving_recolor_and_invert() {
        let src = [10u8, 20, 30, 128, 200, 100, 50, 0, 1, 2, 3, 255];
        let inverted = apply_controls(&src, &ImageControls { invert: true, solid: false, tint: false, solid_color: "#FFFFFF".into(), opacity: 100.0, gif_speed: 1.0 });
        assert_eq!(inverted[3], 128);
        assert_eq!(inverted[0], 245);
        assert_eq!(inverted[8 + 3], 255);
        let solid = apply_controls(&src, &ImageControls { invert: false, solid: true, tint: false, solid_color: "#FF0000".into(), opacity: 50.0, gif_speed: 1.0 });
        assert_eq!(solid[3], 64);
        assert_eq!(solid[2], 255);
        assert_eq!(solid[1], 0);
        assert_eq!(solid[0], 0);
    }

    #[test]
    fn image_dimension_guard_bounds_every_wic_path() {
        assert!(dimensions_allowed(1, 1));
        assert!(dimensions_allowed(4096, 4096));
        assert!(!dimensions_allowed(0, 1));
        assert!(!dimensions_allowed(1, 0));
        assert!(!dimensions_allowed(4097, 1024));
        assert!(!dimensions_allowed(1024, 9000));
    }

    #[test]
    fn oversized_gif_header_is_rejected_without_allocating_a_canvas() {
        let engine = Engine::new().unwrap();
        let mut gif = b"GIF89a".to_vec();
        gif.extend_from_slice(&[0x00, 0x20, 0x00, 0x20, 0x80, 0x00, 0x00]);
        gif.extend_from_slice(&[0x00; 12]);
        gif.push(0x3B);
        let path = std::env::temp_dir().join(format!(
            "crosshair-plus-bomb-{}-{}.gif",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::write(&path, gif).unwrap();
        assert!(decode(&engine, &path, &ImageControls::default()).is_err());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn tint_multiplies_rgb_and_preserves_alpha() {
        let src = [200u8, 100, 50, 128];
        let red = apply_controls(&src, &ImageControls { tint: true, solid_color: "#FF0000".into(), ..Default::default() });
        assert_eq!(red, vec![0, 0, 50, 128], "a red tint keeps the red channel and drops green/blue (BGRA input)");
        let white = apply_controls(&src, &ImageControls { tint: true, solid_color: "#FFFFFF".into(), ..Default::default() });
        assert_eq!(white, src.to_vec(), "a white tint leaves the image untouched");
        let grey = apply_controls(&src, &ImageControls { tint: true, solid_color: "#808080".into(), ..Default::default() });
        assert_eq!(grey, vec![100, 50, 25, 128], "half-grey halves every channel");
        let solid = apply_controls(&src, &ImageControls { solid: true, tint: true, solid_color: "#FF0000".into(), ..Default::default() });
        assert_eq!(solid, vec![0, 0, 255, 128], "solid color still wins over tint");
    }

    #[test]
    fn gif_decodes_the_first_frame_through_wic() {
        let engine = Engine::new().unwrap();
        let path = std::env::temp_dir().join(format!(
            "crosshair-plus-gif-{}-{}.gif",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let gif = [
            0x47, 0x49, 0x46, 0x38, 0x39, 0x61, 0x01, 0x00, 0x01, 0x00, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00,
            0xFF, 0xFF, 0xFF, 0x21, 0xF9, 0x04, 0x01, 0x00, 0x00, 0x00, 0x00, 0x2C, 0x00, 0x00, 0x00, 0x00,
            0x01, 0x00, 0x01, 0x00, 0x00, 0x02, 0x02, 0x44, 0x01, 0x00, 0x3B,
        ];
        std::fs::write(&path, gif).unwrap();
        let asset = decode(&engine, &path, &ImageControls::default()).unwrap();
        assert_eq!(asset.source_kind, "gif");
        assert_eq!(asset.width, 1);
        assert_eq!(asset.height, 1);
        assert_eq!(asset.frames.len(), 1);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn svg_decodes_through_the_d2d_svg_document_path() {
        let engine = Engine::new().unwrap();
        let path = std::env::temp_dir().join(format!(
            "crosshair-plus-svg-{}-{}.svg",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::write(
            &path,
            br##"<svg xmlns="http://www.w3.org/2000/svg" width="32" height="32" viewBox="0 0 32 32"><circle cx="16" cy="16" r="12" fill="#00E5FF"/></svg>"##,
        )
        .unwrap();
        let asset = decode(&engine, &path, &ImageControls::default()).unwrap();
        assert_eq!(asset.width, 512);
        assert_eq!(asset.height, 512);
        assert_eq!(asset.source_kind, "svg");
        assert_eq!(asset.frames.len(), 1);

        let bytes = std::fs::read(&path).unwrap();
        let pixels = render_svg_rgba(&engine, &bytes, 64).unwrap();
        let center = (32 * 64 + 32) * 4;
        assert!(pixels[center + 3] > 240, "SVG center was not opaque: {:?}", &pixels[center..center + 4]);
        assert!(pixels[center] > 200, "SVG center blue channel missing");
        assert!(pixels[center + 1] > 200, "SVG center green channel missing");
        assert!(pixels[center + 2] < 32, "SVG center red channel leaked");
        assert_eq!(pixels[3], 0, "SVG transparent corner gained alpha");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn animated_frames_are_recolored_to_the_silhouette_color() {
        use windows::Win32::Graphics::Direct2D::Common::D2D_RECT_F;
        use windows::Win32::Graphics::Direct2D::D2D1_INTERPOLATION_MODE_NEAREST_NEIGHBOR;
        let engine = Engine::new().unwrap();
        let controls = ImageControls {
            solid: true,
            solid_color: "#FF0000".into(),
            ..Default::default()
        };
        let asset = decode(&engine, Path::new("assets/test.webp"), &controls).unwrap();
        assert!(asset.frames.len() > 1, "the fixture must animate");
        let (width, height) = (asset.width, asset.height);
        let rect = D2D_RECT_F { left: 0.0, top: 0.0, right: width as f32, bottom: height as f32 };
        for (index, frame) in asset.frames.iter().enumerate() {
            let target = engine.target(width, height).unwrap();
            engine.begin(&target, Rgba::rgb(0).alpha(0.0));
            unsafe {
                engine
                    .ctx
                    .DrawBitmap(&frame.bitmap, Some(&rect), 1.0, D2D1_INTERPOLATION_MODE_NEAREST_NEIGHBOR, Some(&rect), None);
            }
            engine.end().unwrap();
            let (bgra, _) = engine.readback(&target).unwrap();
            let mut opaque = 0usize;
            for pixel in bgra.chunks_exact(4) {
                if pixel[3] == 0 {
                    continue;
                }
                opaque += 1;
                assert!(
                    pixel[0] < 8 && pixel[1] < 8 && pixel[2] > 247,
                    "frame {index} kept a non-silhouette pixel: {pixel:?}"
                );
            }
            assert!(opaque > 0, "frame {index} had no opaque pixels");
        }
    }

    fn two_frame_gif() -> Vec<u8> {
        let mut gif = Vec::new();
        gif.extend_from_slice(b"GIF89a");
        gif.extend_from_slice(&[0x01, 0x00, 0x01, 0x00, 0x80, 0x00, 0x00]);
        gif.extend_from_slice(&[0x00, 0x00, 0x00, 0xFF, 0xFF, 0xFF]);
        for delay in [10u8, 20u8] {
            gif.extend_from_slice(&[0x21, 0xF9, 0x04, 0x01, delay, 0x00, 0x00, 0x00]);
            gif.extend_from_slice(&[0x2C, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x01, 0x00, 0x00, 0x02, 0x02, 0x44, 0x01, 0x00]);
        }
        gif.push(0x3B);
        gif
    }

    #[test]
    fn bmp_and_ico_decode_as_static_images() {
        let engine = Engine::new().unwrap();
        let path = std::env::temp_dir().join(format!(
            "crosshair-plus-bmp-{}-{}.bmp",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let mut bmp = Vec::new();
        bmp.extend_from_slice(b"BM");
        bmp.extend_from_slice(&58u32.to_le_bytes());
        bmp.extend_from_slice(&[0, 0, 0, 0]);
        bmp.extend_from_slice(&54u32.to_le_bytes());
        bmp.extend_from_slice(&40u32.to_le_bytes());
        bmp.extend_from_slice(&1i32.to_le_bytes());
        bmp.extend_from_slice(&1i32.to_le_bytes());
        bmp.extend_from_slice(&1u16.to_le_bytes());
        bmp.extend_from_slice(&24u16.to_le_bytes());
        bmp.extend_from_slice(&0u32.to_le_bytes());
        bmp.extend_from_slice(&4u32.to_le_bytes());
        bmp.extend_from_slice(&[0; 16]);
        bmp.extend_from_slice(&[255, 0, 0, 0]);
        std::fs::write(&path, bmp).unwrap();
        let asset = decode(&engine, &path, &ImageControls::default()).unwrap();
        assert_eq!(asset.source_kind, "bmp");
        assert_eq!((asset.width, asset.height), (1, 1));
        assert_eq!(asset.frames.len(), 1);
        let _ = std::fs::remove_file(&path);

        let icon = Path::new("assets/icon.ico");
        let asset = decode(&engine, icon, &ImageControls::default()).unwrap();
        assert_eq!(asset.source_kind, "ico");
        assert_eq!(asset.frames.len(), 1);
        assert!(asset.width > 0 && asset.height > 0);
    }

    #[test]
    fn animated_webp_decodes_through_the_renderer_with_delays() {
        let engine = Engine::new().unwrap();
        let path = std::env::temp_dir().join(format!(
            "crosshair-plus-test-{}-{}.webp",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::write(&path, include_bytes!("../../assets/test.webp")).unwrap();
        let asset = decode(&engine, &path, &ImageControls::default()).unwrap();
        assert_eq!(asset.source_kind, "webp-animated");
        assert!(asset.frames.len() > 1);
        assert!(asset.frames.iter().all(|frame| frame.delay_ms > 0));
        assert!(asset.width > 0 && asset.height > 0);
        let fast = decode(&engine, &path, &ImageControls { gif_speed: 2.0, ..Default::default() }).unwrap();
        assert!(fast.frames.iter().zip(&asset.frames).all(|(fast, normal)| fast.delay_ms >= 20 && fast.delay_ms <= normal.delay_ms));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn animated_gif_import_preserves_frames_and_speed() {
        let engine = Engine::new().unwrap();
        let path = std::env::temp_dir().join(format!(
            "crosshair-plus-animated-gif-{}-{}.gif",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::write(&path, two_frame_gif()).unwrap();
        let asset = decode(&engine, &path, &ImageControls::default()).unwrap();
        assert_eq!(asset.source_kind, "gif");
        assert_eq!(asset.frames.len(), 2);
        assert_eq!(asset.frames[0].delay_ms, 100);
        assert_eq!(asset.frames[1].delay_ms, 200);

        let fast = decode(
            &engine,
            &path,
            &ImageControls { gif_speed: 2.0, ..Default::default() },
        )
        .unwrap();
        assert_eq!(fast.frames[0].delay_ms, 50);
        assert_eq!(fast.frames[1].delay_ms, 100);
        let _ = std::fs::remove_file(&path);
    }
}
