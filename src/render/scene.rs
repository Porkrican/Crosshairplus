use windows::core::Result;
use windows::Win32::Foundation::{GENERIC_READ, HGLOBAL};
use windows::Win32::Graphics::Imaging::{
    IWICImagingFactory, WICBitmapDitherTypeNone, WICBitmapPaletteTypeCustom, WICDecodeMetadataCacheOnDemand,
    GUID_WICPixelFormat32bppPBGRA,
};
use windows::Win32::System::Com::StructuredStorage::CreateStreamOnHGlobal;
use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED};

use super::engine::unpremultiply;

pub const ICON_PNG: &[u8] = include_bytes!("../../assets/icon.png");
pub const ICON_ICO: &[u8] = include_bytes!("../../assets/icon.ico");
pub const SCENE_DAY: &[u8] = include_bytes!("../../assets/scene_day.png");
pub const SCENE_NIGHT: &[u8] = include_bytes!("../../assets/scene_night.png");

pub fn preset_png(style: &str) -> &'static [u8] {
    match style {
        "dot" => include_bytes!("../../assets/presets/dot.png"),
        "ring" => include_bytes!("../../assets/presets/ring.png"),
        "circle_dot" => include_bytes!("../../assets/presets/circle_dot.png"),
        "triangle" => include_bytes!("../../assets/presets/triangle.png"),
        "peak" => include_bytes!("../../assets/presets/peak.png"),
        "tsight" => include_bytes!("../../assets/presets/tsight.png"),
        _ => include_bytes!("../../assets/presets/plus.png"),
    }
}

pub fn icon_rgba(size: u32) -> Result<(Vec<u8>, u32)> {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let factory: IWICImagingFactory = CoCreateInstance(
            &windows::Win32::Graphics::Imaging::CLSID_WICImagingFactory,
            None,
            CLSCTX_INPROC_SERVER,
        )?;
        let stream = CreateStreamOnHGlobal(HGLOBAL::default(), true)?;
        let mut written = 0u32;
        stream.Write(ICON_PNG.as_ptr() as *const core::ffi::c_void, ICON_PNG.len() as u32, Some(&mut written)).ok()?;
        stream.Seek(0, windows::Win32::System::Com::STREAM_SEEK_SET, None)?;
        let decoder = factory.CreateDecoderFromStream(&stream, std::ptr::null(), WICDecodeMetadataCacheOnDemand)?;
        let frame = decoder.GetFrame(0)?;
        let mut width = 0u32;
        let mut height = 0u32;
        frame.GetSize(&mut width, &mut height)?;
        let converter = factory.CreateFormatConverter()?;
        converter.Initialize(
            &frame,
            &GUID_WICPixelFormat32bppPBGRA,
            WICBitmapDitherTypeNone,
            None,
            0.0,
            WICBitmapPaletteTypeCustom,
        )?;
        let mut buffer = vec![0u8; (width * height * 4) as usize];
        converter.CopyPixels(std::ptr::null(), width * 4, &mut buffer)?;
        let straight = unpremultiply(&buffer);
        let (out, dim) = downscale(&straight, width, height, size);
        let _ = GENERIC_READ;
        Ok((out, dim))
    }
}

fn downscale(src: &[u8], width: u32, height: u32, target: u32) -> (Vec<u8>, u32) {
    let target = target.clamp(16, 256).min(width).min(height);
    if target == width && target == height {
        return (src.to_vec(), target);
    }
    let mut out = vec![0u8; (target * target * 4) as usize];
    let scale_x = width as f32 / target as f32;
    let scale_y = height as f32 / target as f32;
    for y in 0..target {
        for x in 0..target {
            let sx = ((x as f32 + 0.5) * scale_x).floor().min((width - 1) as f32) as u32;
            let sy = ((y as f32 + 0.5) * scale_y).floor().min((height - 1) as f32) as u32;
            let si = ((sy * width + sx) * 4) as usize;
            let di = ((y * target + x) * 4) as usize;
            out[di..di + 4].copy_from_slice(&src[si..si + 4]);
        }
    }
    (out, target)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_assets_are_png() {
        assert_eq!(&ICON_PNG[..8], b"\x89PNG\r\n\x1a\n");
        assert_eq!(&SCENE_DAY[..8], b"\x89PNG\r\n\x1a\n");
        assert_eq!(&SCENE_NIGHT[..8], b"\x89PNG\r\n\x1a\n");
        assert!(ICON_ICO.starts_with(&[0, 0, 1, 0]));
        assert!(preset_png("ring").len() > 100);
    }

    fn png_dimensions(bytes: &[u8]) -> (u32, u32) {
        assert_eq!(&bytes[..8], b"\x89PNG\r\n\x1a\n");
        (
            u32::from_be_bytes(bytes[16..20].try_into().unwrap()),
            u32::from_be_bytes(bytes[20..24].try_into().unwrap()),
        )
    }

    #[test]
    fn embedded_scenes_and_presets_have_their_exact_sizes() {
        assert_eq!(png_dimensions(SCENE_DAY), (1280, 720));
        assert_eq!(png_dimensions(SCENE_NIGHT), (1280, 720));
        for style in ["plus", "dot", "ring", "circle_dot", "triangle", "peak", "tsight"] {
            assert_eq!(png_dimensions(preset_png(style)), (128, 128), "{style}");
        }
    }

    #[test]
    fn embedded_ico_contains_every_bundled_size() {
        let count = u16::from_le_bytes([ICON_ICO[4], ICON_ICO[5]]) as usize;
        assert_eq!(count, 7);
        let mut sizes = Vec::new();
        for index in 0..count {
            let at = 6 + index * 16;
            let width = if ICON_ICO[at] == 0 { 256 } else { ICON_ICO[at] as u32 };
            let height = if ICON_ICO[at + 1] == 0 { 256 } else { ICON_ICO[at + 1] as u32 };
            assert_eq!(width, height);
            sizes.push(width);
        }
        assert_eq!(sizes, vec![16, 24, 32, 48, 64, 128, 256]);
    }

    #[test]
    fn wic_tray_icon_decodes_and_has_visible_pixels() {
        let (rgba, size) = icon_rgba(32).unwrap();
        assert_eq!(size, 32);
        assert_eq!(rgba.len(), 32 * 32 * 4);
        assert!(rgba.chunks_exact(4).any(|px| px[3] > 0), "tray icon must contain alpha pixels");
    }
}
