use std::collections::HashMap;

use serde::Serialize;
use windows::Win32::Devices::Display::{
    DisplayConfigGetDeviceInfo, GetDisplayConfigBufferSizes, QueryDisplayConfig, DISPLAYCONFIG_DEVICE_INFO_GET_ADVANCED_COLOR_INFO,
    DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME, DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME, DISPLAYCONFIG_GET_ADVANCED_COLOR_INFO,
    DISPLAYCONFIG_MODE_INFO, DISPLAYCONFIG_PATH_INFO, DISPLAYCONFIG_SOURCE_DEVICE_NAME, DISPLAYCONFIG_TARGET_DEVICE_NAME,
    QDC_ONLY_ACTIVE_PATHS,
};
use windows::core::{Interface, BOOL};
use windows::Win32::Foundation::{LPARAM, RECT};
use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIFactory1, IDXGIOutput6};
use windows::Win32::Graphics::Gdi::{
    EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORINFO, MONITORINFOEXW,
};
use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};

use crate::display::warning;

#[derive(Serialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct MonitorInfo {
    pub id: String,
    pub gdi: String,
    pub name: String,
    pub brand: String,
    pub model: String,
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub primary: bool,
    pub dpi: u32,
    pub scale: f32,
    pub hdr: bool,
    pub advanced_color: bool,
    /// Best-effort panel technology used by the `auto` display preset value.
    pub detected_panel: String,
    pub night_light: bool,
    pub backlight_supported: bool,
    pub backlight: Option<u32>,
}

struct RawMonitor {
    gdi: String,
    x: i32,
    y: i32,
    width: u32,
    height: u32,
    primary: bool,
    dpi: u32,
}

struct TargetInfo {
    friendly: String,
    device_path: String,
    hdr: bool,
    advanced_color: bool,
}

struct OutputPanel {
    x: i32,
    y: i32,
    width: u32,
    height: u32,
    luminance: crate::display::panel::Luminance,
}

fn output_panels() -> Vec<OutputPanel> {
    let mut panels = Vec::new();
    unsafe {
        let factory: IDXGIFactory1 = match CreateDXGIFactory1() {
            Ok(factory) => factory,
            Err(err) => {
                crate::logging::log(&format!("monitors: DXGI factory unavailable: {err}"));
                return panels;
            }
        };
        let mut adapter_index = 0u32;
        while let Ok(adapter) = factory.EnumAdapters1(adapter_index) {
            adapter_index += 1;
            let mut output_index = 0u32;
            while let Ok(output) = adapter.EnumOutputs(output_index) {
                output_index += 1;
                let Ok(output6) = output.cast::<IDXGIOutput6>() else { continue };
                let Ok(desc) = output6.GetDesc1() else { continue };
                let rect = desc.DesktopCoordinates;
                panels.push(OutputPanel {
                    x: rect.left,
                    y: rect.top,
                    width: (rect.right - rect.left).max(1) as u32,
                    height: (rect.bottom - rect.top).max(1) as u32,
                    luminance: crate::display::panel::Luminance {
                        min: desc.MinLuminance,
                        max: desc.MaxLuminance,
                    },
                });
            }
        }
    }
    panels
}

pub fn refresh(probes: &crate::display::ddc::BacklightProbes) -> Vec<MonitorInfo> {
    build(Some(probes))
}

/// Enumeration without DDC/WMI probes: safe before the first paint. The full
/// probe runs on a worker once the window exists.
pub fn quick() -> Vec<MonitorInfo> {
    build(None)
}

fn build(probes: Option<&crate::display::ddc::BacklightProbes>) -> Vec<MonitorInfo> {
    let probe_backlight = probes.is_some();
    let raw = enum_monitors();
    let targets = target_map();
    let panels = output_panels();
    let night = warning::night_light_active();
    let wmi_supported = probe_backlight && crate::display::ddc::wmi::supported();
    let wmi_brightness = if wmi_supported { crate::display::ddc::wmi::brightness() } else { None };
    let mut out: Vec<MonitorInfo> = raw
        .into_iter()
        .enumerate()
        .map(|(index, m)| {
            let target = targets.get(&m.gdi.to_ascii_lowercase());
            let (brand, model, device_path, friendly) = match target {
                Some(t) => {
                    let (brand, model) = crate::display::edid::identify(&t.device_path, &t.friendly);
                    (brand, model, t.device_path.clone(), t.friendly.clone())
                }
                None => {
                    let (brand, model) = crate::display::edid::identify("", "");
                    (brand, model, String::new(), String::new())
                }
            };
            let output = panels
                .iter()
                .find(|panel| {
                    panel.x == m.x
                        && panel.y == m.y
                        && panel.width == m.width
                        && panel.height == m.height
                })
                .or_else(|| panels.get(index));
            let detected_panel = crate::display::panel::detect(
                &format!("{brand} {model} {friendly}"),
                output.map(|panel| panel.luminance),
                target.map(|t| t.hdr || t.advanced_color).unwrap_or(false),
            );
            crate::logging::log(&format!(
                "monitors: {} panel={} ({}x{})",
                format!("{brand} {model}").trim(),
                detected_panel,
                m.width,
                m.height
            ));
            let id = if device_path.is_empty() { m.gdi.clone() } else { device_path.clone() };
            let (mut backlight_supported, mut backlight) = match probes {
                Some(probes) => match probes.probe(&m.gdi) {
                    Some(Some(value)) => (true, Some(value)),
                    _ => (false, None),
                },
                None => (false, None),
            };
            if !backlight_supported && m.primary && wmi_supported {
                backlight_supported = true;
                backlight = wmi_brightness;
            }
            MonitorInfo {
                id,
                gdi: m.gdi.clone(),
                name: if model.is_empty() { m.gdi.clone() } else { format!("{brand} {model}").trim().to_string() },
                brand,
                model,
                x: m.x,
                y: m.y,
                width: m.width,
                height: m.height,
                primary: m.primary,
                dpi: m.dpi,
                scale: scale_for_dpi(m.dpi),
                hdr: target.map(|t| t.hdr).unwrap_or(false),
                advanced_color: target.map(|t| t.advanced_color).unwrap_or(false),
                detected_panel,
                night_light: night,
                backlight_supported,
                backlight,
            }
        })
        .collect();
    out.sort_by(|a, b| b.primary.cmp(&a.primary).then(a.x.cmp(&b.x)).then(a.y.cmp(&b.y)));
    if out.is_empty() {
        out.push(MonitorInfo {
            id: "primary".into(),
            gdi: "\\\\.\\DISPLAY1".into(),
            name: "Primary display".into(),
            brand: String::new(),
            model: "Primary display".into(),
            width: 1920,
            height: 1080,
            primary: true,
            dpi: 96,
            scale: 1.0,
            detected_panel: "led".into(),
            ..Default::default()
        });
    }
    out
}

fn enum_monitors() -> Vec<RawMonitor> {
    let mut raw: Vec<RawMonitor> = Vec::new();
    unsafe extern "system" fn callback(hmonitor: HMONITOR, _hdc: HDC, _rect: *mut RECT, lparam: LPARAM) -> BOOL {
        let list = &mut *(lparam.0 as *mut Vec<RawMonitor>);
        unsafe {
            let mut info = MONITORINFOEXW::default();
            info.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
            if GetMonitorInfoW(hmonitor, &mut info as *mut _ as *mut MONITORINFO).as_bool() {
                let end = info.szDevice.iter().position(|c| *c == 0).unwrap_or(info.szDevice.len());
                let gdi = String::from_utf16_lossy(&info.szDevice[..end]);
                let mut dpi_x = 96u32;
                let mut dpi_y = 96u32;
                let _ = GetDpiForMonitor(hmonitor, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y);
                list.push(RawMonitor {
                    gdi,
                    x: info.monitorInfo.rcMonitor.left,
                    y: info.monitorInfo.rcMonitor.top,
                    width: (info.monitorInfo.rcMonitor.right - info.monitorInfo.rcMonitor.left).max(1) as u32,
                    height: (info.monitorInfo.rcMonitor.bottom - info.monitorInfo.rcMonitor.top).max(1) as u32,
                    primary: info.monitorInfo.dwFlags & 1 != 0,
                    dpi: dpi_x.max(96),
                });
            }
        }
        true.into()
    }
    let ok = unsafe { EnumDisplayMonitors(None, None, Some(callback), LPARAM(&mut raw as *mut _ as isize)) };
    if !ok.as_bool() {
        crate::logging::log("monitors: EnumDisplayMonitors failed; using the fallback display");
    }
    raw
}

fn target_map() -> HashMap<String, TargetInfo> {
    let mut map = HashMap::new();
    unsafe {
        let mut path_count = 0u32;
        let mut mode_count = 0u32;
        let status = GetDisplayConfigBufferSizes(QDC_ONLY_ACTIVE_PATHS, &mut path_count, &mut mode_count);
        if status.is_err() {
            crate::logging::log(&format!("monitors: display config buffer size query failed: {}", status.0));
            return map;
        }
        let mut paths = vec![DISPLAYCONFIG_PATH_INFO::default(); path_count as usize];
        let mut modes = vec![DISPLAYCONFIG_MODE_INFO::default(); mode_count as usize];
        let status = QueryDisplayConfig(
            QDC_ONLY_ACTIVE_PATHS,
            &mut path_count,
            paths.as_mut_ptr(),
            &mut mode_count,
            modes.as_mut_ptr(),
            None,
        );
        if status.is_err() {
            crate::logging::log(&format!("monitors: display config query failed: {}", status.0));
            return map;
        }
        for path in paths.into_iter().take(path_count as usize) {
            let mut source = DISPLAYCONFIG_SOURCE_DEVICE_NAME::default();
            source.header.r#type = DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME;
            source.header.size = std::mem::size_of::<DISPLAYCONFIG_SOURCE_DEVICE_NAME>() as u32;
            source.header.adapterId = path.sourceInfo.adapterId;
            source.header.id = path.sourceInfo.id;
            if DisplayConfigGetDeviceInfo(&mut source.header) != 0 {
                continue;
            }
            let end = source.viewGdiDeviceName.iter().position(|c| *c == 0).unwrap_or(source.viewGdiDeviceName.len());
            let gdi = String::from_utf16_lossy(&source.viewGdiDeviceName[..end]);

            let mut target = DISPLAYCONFIG_TARGET_DEVICE_NAME::default();
            target.header.r#type = DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME;
            target.header.size = std::mem::size_of::<DISPLAYCONFIG_TARGET_DEVICE_NAME>() as u32;
            target.header.adapterId = path.targetInfo.adapterId;
            target.header.id = path.targetInfo.id;
            if DisplayConfigGetDeviceInfo(&mut target.header) != 0 {
                continue;
            }
            let friendly_end = target.monitorFriendlyDeviceName.iter().position(|c| *c == 0).unwrap_or(64);
            let path_end = target.monitorDevicePath.iter().position(|c| *c == 0).unwrap_or(128);
            let friendly = String::from_utf16_lossy(&target.monitorFriendlyDeviceName[..friendly_end]).trim().to_string();
            let device_path = String::from_utf16_lossy(&target.monitorDevicePath[..path_end]).trim().to_string();

            let mut advanced = DISPLAYCONFIG_GET_ADVANCED_COLOR_INFO::default();
            advanced.header.r#type = DISPLAYCONFIG_DEVICE_INFO_GET_ADVANCED_COLOR_INFO;
            advanced.header.size = std::mem::size_of::<DISPLAYCONFIG_GET_ADVANCED_COLOR_INFO>() as u32;
            advanced.header.adapterId = path.targetInfo.adapterId;
            advanced.header.id = path.targetInfo.id;
            let (hdr, advanced_color) = if DisplayConfigGetDeviceInfo(&mut advanced.header) == 0 {
                let bits = advanced.Anonymous.value;
                (bits & 0b10 != 0, bits & 0b100 != 0)
            } else {
                (false, false)
            };
            map.insert(gdi.to_ascii_lowercase(), TargetInfo { friendly, device_path, hdr, advanced_color });
        }
    }
    map
}

pub fn scale_for_dpi(dpi: u32) -> f32 {
    (dpi as f32 / 96.0).max(0.5)
}

pub fn find<'a>(monitors: &'a [MonitorInfo], id: Option<&str>) -> &'a MonitorInfo {
    id.and_then(|id| monitors.iter().find(|m| m.id == id))
        .or_else(|| monitors.iter().find(|m| m.primary))
        .unwrap_or(&monitors[0])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn monitor(id: &str, x: i32, dpi: u32, primary: bool) -> MonitorInfo {
        MonitorInfo {
            id: id.into(),
            gdi: format!("\\\\.\\DISPLAY{}", x.unsigned_abs() + 1),
            name: id.into(),
            x,
            y: 0,
            width: 1920,
            height: 1080,
            primary,
            dpi,
            scale: scale_for_dpi(dpi),
            ..Default::default()
        }
    }

    #[test]
    fn mixed_dpi_layout_selects_the_requested_or_primary_display() {
        let monitors = vec![
            monitor("secondary", 2560, 144, false),
            monitor("primary", 0, 120, true),
            monitor("left", -1920, 96, false),
        ];
        assert_eq!(find(&monitors, Some("secondary")).id, "secondary");
        assert_eq!(find(&monitors, None).id, "primary");
        assert_eq!(find(&monitors, Some("missing")).id, "primary");
        assert!((scale_for_dpi(96) - 1.0).abs() < f32::EPSILON);
        assert!((scale_for_dpi(120) - 1.25).abs() < f32::EPSILON);
        assert!((scale_for_dpi(144) - 1.5).abs() < f32::EPSILON);
        assert_eq!(scale_for_dpi(1), 0.5);
    }
}
