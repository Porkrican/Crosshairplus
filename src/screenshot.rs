use std::ffi::c_void;
use std::sync::mpsc;

use windows::Win32::Foundation::HGLOBAL;
use windows::Win32::Graphics::Imaging::{
    IWICImagingFactory, WICBitmapDitherTypeNone, WICBitmapEncoderNoCache, WICBitmapInterpolationModeHighQualityCubic,
    WICBitmapPaletteTypeCustom, WICDecodeMetadataCacheOnLoad, GUID_ContainerFormatPng, GUID_WICPixelFormat32bppBGRA,
    GUID_WICPixelFormat32bppPBGRA,
};
use windows::Win32::System::Com::StructuredStorage::{CreateStreamOnHGlobal, IPropertyBag2};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, IStream, CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, STATFLAG_DEFAULT, STATSTG,
    STREAM_SEEK_SET,
};
use webview2_com::Microsoft::Web::WebView2::Win32::COREWEBVIEW2_CAPTURE_PREVIEW_IMAGE_FORMAT_PNG;
use webview2_com::CapturePreviewCompletedHandler;
use wry::WebViewExtWindows;

fn read_stream(stream: &IStream) -> windows::core::Result<Vec<u8>> {
    unsafe {
        let mut stat = STATSTG::default();
        stream.Stat(&mut stat, STATFLAG_DEFAULT)?;
        let size = stat.cbSize as usize;
        stream.Seek(0, STREAM_SEEK_SET, None)?;
        let mut bytes = vec![0u8; size];
        let mut read = 0u32;
        stream.Read(bytes.as_mut_ptr() as *mut c_void, size as u32, Some(&mut read)).ok()?;
        bytes.truncate(read as usize);
        Ok(bytes)
    }
}

/// Starts a WebView2 capture without pumping the outer event loop. The caller
/// polls the receiver; this avoids re-entering tao's event handler.
pub fn capture_async(
    webview: &wry::WebView,
) -> windows::core::Result<mpsc::Receiver<Result<Vec<u8>, String>>> {
    let stream: IStream = unsafe { CreateStreamOnHGlobal(HGLOBAL::default(), true)? };
    let (tx, rx) = mpsc::channel();
    let stream_read = stream.clone();
    let handler = CapturePreviewCompletedHandler::create(Box::new(move |result| {
        let result = match result {
            Ok(()) => read_stream(&stream_read).map_err(|err| err.to_string()),
            Err(err) => Err(err.to_string()),
        };
        let _ = tx.send(result);
        Ok(())
    }));
    let webview2 = webview.webview();
    unsafe {
        webview2.CapturePreview(COREWEBVIEW2_CAPTURE_PREVIEW_IMAGE_FORMAT_PNG, &stream, &handler)?;
    }
    Ok(rx)
}

pub fn save_captured(bytes: Vec<u8>, path: &std::path::Path, width: u32, height: u32) -> windows::core::Result<()> {
    let mut bytes = bytes;
    if let Some((current_w, current_h)) = png_size(&bytes) {
        if current_w != width || current_h != height {
            bytes = resize_png(&bytes, width, height)?;
        }
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|err| {
            windows::core::Error::new(windows::Win32::Foundation::E_FAIL, err.to_string())
        })?;
    }
    std::fs::write(path, &bytes).map_err(|err| {
        windows::core::Error::new(windows::Win32::Foundation::E_FAIL, err.to_string())
    })?;
    crate::logging::log(&format!("screenshot: wrote {} ({} bytes)", path.display(), bytes.len()));
    Ok(())
}

pub fn resize_png(bytes: &[u8], width: u32, height: u32) -> windows::core::Result<Vec<u8>> {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let factory: IWICImagingFactory = CoCreateInstance(
            &windows::Win32::Graphics::Imaging::CLSID_WICImagingFactory,
            None,
            CLSCTX_INPROC_SERVER,
        )?;
        let input = CreateStreamOnHGlobal(HGLOBAL::default(), true)?;
        let mut written = 0u32;
        input.Write(bytes.as_ptr() as *const c_void, bytes.len() as u32, Some(&mut written)).ok()?;
        input.Seek(0, STREAM_SEEK_SET, None)?;
        let decoder = factory.CreateDecoderFromStream(&input, std::ptr::null(), WICDecodeMetadataCacheOnLoad)?;
        let frame = decoder.GetFrame(0)?;
        let scaler = factory.CreateBitmapScaler()?;
        scaler.Initialize(&frame, width, height, WICBitmapInterpolationModeHighQualityCubic)?;
        let converter = factory.CreateFormatConverter()?;
        converter.Initialize(
            &scaler,
            &GUID_WICPixelFormat32bppPBGRA,
            WICBitmapDitherTypeNone,
            None,
            0.0,
            WICBitmapPaletteTypeCustom,
        )?;
        let stride = width * 4;
        let mut pixels = vec![0u8; (stride * height) as usize];
        converter.CopyPixels(std::ptr::null(), stride, &mut pixels)?;

        let output = CreateStreamOnHGlobal(HGLOBAL::default(), true)?;
        let encoder = factory.CreateEncoder(&GUID_ContainerFormatPng, std::ptr::null())?;
        encoder.Initialize(&output, WICBitmapEncoderNoCache)?;
        let mut frame_encode = None;
        let mut options: Option<IPropertyBag2> = None;
        encoder.CreateNewFrame(&mut frame_encode, &mut options)?;
        let frame_encode = frame_encode.ok_or_else(|| windows::core::Error::from(windows::Win32::Foundation::E_FAIL))?;
        frame_encode.Initialize(None::<&IPropertyBag2>)?;
        frame_encode.SetSize(width, height)?;
        let mut format = GUID_WICPixelFormat32bppBGRA;
        frame_encode.SetPixelFormat(&mut format)?;
        frame_encode.WritePixels(height, stride, &pixels)?;
        frame_encode.Commit()?;
        encoder.Commit()?;
        let mut stat = STATSTG::default();
        output.Stat(&mut stat, STATFLAG_DEFAULT)?;
        let size = stat.cbSize as usize;
        output.Seek(0, STREAM_SEEK_SET, None)?;
        let mut result = vec![0u8; size];
        let mut read = 0u32;
        output.Read(result.as_mut_ptr() as *mut c_void, size as u32, Some(&mut read)).ok()?;
        result.truncate(read as usize);
        Ok(result)
    }
}

/// Encode raw top-down BGRA pixels as a PNG. Used for the small application
/// icons shown in the Auto-launch list.
pub fn encode_bgra_png(width: u32, height: u32, pixels: &[u8]) -> windows::core::Result<Vec<u8>> {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let factory: IWICImagingFactory = CoCreateInstance(
            &windows::Win32::Graphics::Imaging::CLSID_WICImagingFactory,
            None,
            CLSCTX_INPROC_SERVER,
        )?;
        let output = CreateStreamOnHGlobal(HGLOBAL::default(), true)?;
        let encoder = factory.CreateEncoder(&GUID_ContainerFormatPng, std::ptr::null())?;
        encoder.Initialize(&output, WICBitmapEncoderNoCache)?;
        let mut frame_encode = None;
        let mut options: Option<IPropertyBag2> = None;
        encoder.CreateNewFrame(&mut frame_encode, &mut options)?;
        let frame_encode = frame_encode.ok_or_else(|| windows::core::Error::from(windows::Win32::Foundation::E_FAIL))?;
        frame_encode.Initialize(None::<&IPropertyBag2>)?;
        frame_encode.SetSize(width, height)?;
        let mut format = GUID_WICPixelFormat32bppBGRA;
        frame_encode.SetPixelFormat(&mut format)?;
        frame_encode.WritePixels(height, width * 4, pixels)?;
        frame_encode.Commit()?;
        encoder.Commit()?;
        let mut stat = STATSTG::default();
        output.Stat(&mut stat, STATFLAG_DEFAULT)?;
        output.Seek(0, STREAM_SEEK_SET, None)?;
        let mut result = vec![0u8; stat.cbSize as usize];
        let mut read = 0u32;
        output.Read(result.as_mut_ptr() as *mut c_void, result.len() as u32, Some(&mut read)).ok()?;
        result.truncate(read as usize);
        Ok(result)
    }
}

pub fn png_size(bytes: &[u8]) -> Option<(u32, u32)> {
    if bytes.len() < 24 || &bytes[..8] != b"\x89PNG\r\n\x1a\n" {
        return None;
    }
    let width = u32::from_be_bytes([bytes[16], bytes[17], bytes[18], bytes[19]]);
    let height = u32::from_be_bytes([bytes[20], bytes[21], bytes[22], bytes[23]]);
    Some((width, height))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn png_header_reports_exact_capture_dimensions() {
        let mut png = [0u8; 24];
        png[..8].copy_from_slice(b"\x89PNG\r\n\x1a\n");
        png[16..20].copy_from_slice(&1200u32.to_be_bytes());
        png[20..24].copy_from_slice(&780u32.to_be_bytes());
        assert_eq!(png_size(&png), Some((1200, 780)));
        assert_eq!(png_size(b"not a png"), None);
    }
}
