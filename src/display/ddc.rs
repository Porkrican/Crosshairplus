use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use windows::core::BOOL;
use windows::Win32::Devices::Display::{
    DestroyPhysicalMonitors, GetMonitorBrightness, GetNumberOfPhysicalMonitorsFromHMONITOR, GetPhysicalMonitorsFromHMONITOR,
    SetMonitorBrightness, PHYSICAL_MONITOR,
};
use windows::Win32::Foundation::{LPARAM, RECT};
use windows::Win32::Graphics::Gdi::{
    EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORINFO, MONITORINFOEXW,
};

fn find_hmonitor(gdi: &str) -> Option<HMONITOR> {
    struct Search<'a> {
        gdi: &'a str,
        found: Option<HMONITOR>,
    }
    unsafe extern "system" fn callback(hmonitor: HMONITOR, _hdc: HDC, _rect: *mut RECT, lparam: LPARAM) -> BOOL {
        let search = &mut *(lparam.0 as *mut Search);
        unsafe {
            let mut info = MONITORINFOEXW::default();
            info.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
            if GetMonitorInfoW(hmonitor, &mut info as *mut _ as *mut MONITORINFO).as_bool() {
                let end = info.szDevice.iter().position(|c| *c == 0).unwrap_or(info.szDevice.len());
                let name = String::from_utf16_lossy(&info.szDevice[..end]);
                if name.eq_ignore_ascii_case(search.gdi) {
                    search.found = Some(hmonitor);
                    return false.into();
                }
            }
        }
        true.into()
    }
    let mut search = Search { gdi, found: None };
    unsafe {
        let _ = EnumDisplayMonitors(None, None, Some(callback), LPARAM(&mut search as *mut _ as isize));
    }
    search.found
}

fn physical(gdi: &str) -> Option<(Vec<PHYSICAL_MONITOR>, (u32, u32, u32))> {
    let hmonitor = find_hmonitor(gdi)?;
    unsafe {
        let mut count = 0u32;
        if GetNumberOfPhysicalMonitorsFromHMONITOR(hmonitor, &mut count).is_err() || count == 0 {
            return None;
        }
        let mut monitors: Vec<PHYSICAL_MONITOR> = (0..count).map(|_| std::mem::zeroed()).collect();
        if GetPhysicalMonitorsFromHMONITOR(hmonitor, &mut monitors).is_err() {
            return None;
        }
        let mut min = 0u32;
        let mut current = 0u32;
        let mut max = 0u32;
        if GetMonitorBrightness(monitors[0].hPhysicalMonitor, &mut min, &mut current, &mut max) == 0 {
            let _ = DestroyPhysicalMonitors(&monitors);
            return None;
        }
        Some((monitors, (min, current, max)))
    }
}

fn percent_from_raw(min: u32, current: u32, max: u32) -> Option<u32> {
    if max <= min {
        return None;
    }
    Some((((current.saturating_sub(min)) as f32 / (max - min) as f32) * 100.0).round().clamp(0.0, 100.0) as u32)
}

fn raw_from_percent(min: u32, max: u32, percent: u32) -> Option<u32> {
    if max <= min {
        return None;
    }
    Some(min + (((max - min) as f32) * percent.clamp(0, 100) as f32 / 100.0).round() as u32)
}

pub fn brightness_for_gdi(gdi: &str) -> Option<u32> {
    let (monitors, (min, current, max)) = physical(gdi)?;
    unsafe {
        let _ = DestroyPhysicalMonitors(&monitors);
    }
    percent_from_raw(min, current, max)
}

pub fn set_brightness_for_gdi(gdi: &str, percent: u32) -> bool {
    let Some((monitors, (min, _, max))) = physical(gdi) else { return false };
    let Some(target) = raw_from_percent(min, max, percent) else {
        unsafe {
            let _ = DestroyPhysicalMonitors(&monitors);
        }
        return false;
    };
    let ok = unsafe { SetMonitorBrightness(monitors[0].hPhysicalMonitor, target) != 0 };
    unsafe {
        let _ = DestroyPhysicalMonitors(&monitors);
    }
    ok
}

pub struct BacklightPacer {
    tx: mpsc::Sender<(String, u32)>,
}

impl BacklightPacer {
    pub fn start() -> Self {
        let (tx, rx) = mpsc::channel::<(String, u32)>();
        thread::spawn(move || {
            let mut last = Instant::now() - Duration::from_secs(1);
            while let Ok(first) = rx.recv() {
                let mut pending = first;
                let deadline = Instant::now() + Duration::from_millis(60);
                loop {
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    if remaining.is_zero() {
                        break;
                    }
                    match rx.recv_timeout(remaining) {
                        Ok(next) => pending = next,
                        Err(_) => break,
                    }
                }
                let wait = Duration::from_millis(50).saturating_sub(last.elapsed());
                if !wait.is_zero() {
                    thread::sleep(wait);
                }
                if !set_brightness_for_gdi(&pending.0, pending.1) {
                    if !wmi::set(pending.1) {
                        crate::logging::log(&format!("display: backlight {}% rejected for {}", pending.1, pending.0));
                    }
                }
                last = Instant::now();
            }
        });
        Self { tx }
    }
    pub fn set(&self, gdi: String, percent: u32) {
        if self.tx.send((gdi, percent)).is_err() {
            crate::logging::log("display: backlight worker unavailable; request dropped");
        }
    }
}

/// WMI backlight fallback for laptop panels without DDC/CI.  Windows exposes
/// WmiMonitorBrightnessMethods in root\wmi; only used when DDC/CI is absent.
pub mod wmi {
    use windows::core::{BSTR, PCWSTR};
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED,
    };
    use windows::Win32::System::Variant::{VariantClear, VARIANT, VT_I4, VT_UI1, VT_UI4};
    use windows::Win32::System::Wmi::{
        IWbemClassObject, IWbemLocator, IWbemServices, WbemLocator, WBEM_FLAG_FORWARD_ONLY,
        WBEM_FLAG_RETURN_IMMEDIATELY, WBEM_GENERIC_FLAG_TYPE,
    };

    fn services() -> Option<IWbemServices> {
        unsafe {
            let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
            let locator: IWbemLocator = CoCreateInstance(&WbemLocator, None, CLSCTX_INPROC_SERVER).ok()?;
            locator
                .ConnectServer(
                    &BSTR::from("ROOT\\WMI"),
                    &BSTR::new(),
                    &BSTR::new(),
                    &BSTR::new(),
                    0,
                    &BSTR::new(),
                    None,
                )
                .ok()
        }
    }

    fn first_object(services: &IWbemServices, query: &str) -> Option<IWbemClassObject> {
        unsafe {
            let enumerator = services
                .ExecQuery(
                    &BSTR::from("WQL"),
                    &BSTR::from(query),
                    WBEM_FLAG_FORWARD_ONLY | WBEM_FLAG_RETURN_IMMEDIATELY,
                    None,
                )
                .ok()?;
            let mut objects: [Option<IWbemClassObject>; 1] = [None];
            let mut returned = 0u32;
            if enumerator.Next(100, &mut objects, &mut returned).is_err() {
                return None;
            }
            objects[0].take()
        }
    }

    pub fn supported() -> bool {
        services().and_then(|s| first_object(&s, "SELECT * FROM WmiMonitorBrightness")).is_some()
    }

    pub fn brightness() -> Option<u32> {
        let services = services()?;
        let instance = first_object(&services, "SELECT CurrentBrightness FROM WmiMonitorBrightness")?;
        let name: Vec<u16> = "CurrentBrightness".encode_utf16().chain(Some(0)).collect();
        let mut value = VARIANT::default();
        unsafe {
            if instance.Get(PCWSTR(name.as_ptr()), 0, &mut value, None, None).is_err() {
                return None;
            }
            let inner = &*value.Anonymous.Anonymous;
            let level = match inner.vt {
                VT_UI1 => inner.Anonymous.bVal as u32,
                VT_UI4 => inner.Anonymous.ulVal,
                VT_I4 => inner.Anonymous.lVal.max(0) as u32,
                _ => {
                    let _ = VariantClear(&mut value);
                    return None;
                }
            };
            let _ = VariantClear(&mut value);
            Some(level.min(100))
        }
    }

    fn variant_ui4(value: u32) -> VARIANT {
        let mut variant = VARIANT::default();
        unsafe {
            let inner = &mut *variant.Anonymous.Anonymous;
            inner.vt = VT_UI4;
            inner.Anonymous.ulVal = value;
        }
        variant
    }

    fn normalized_percent(percent: u32) -> u8 {
        percent.clamp(0, 100) as u8
    }

    fn variant_ui1(value: u8) -> VARIANT {
        let mut variant = VARIANT::default();
        unsafe {
            let inner = &mut *variant.Anonymous.Anonymous;
            inner.vt = VT_UI1;
            inner.Anonymous.bVal = value;
        }
        variant
    }

    pub fn set(percent: u32) -> bool {
        let Some(services) = services() else { return false };
        unsafe {
            let Some(instance) = first_object(&services, "SELECT * FROM WmiMonitorBrightnessMethods") else { return false };
            let path = match instance.GetObjectText(0) {
                Ok(path) => path,
                Err(_) => return false,
            };
            let mut class: Option<IWbemClassObject> = None;
            if services
                .GetObject(
                    &BSTR::from("WmiMonitorBrightnessMethods"),
                    WBEM_GENERIC_FLAG_TYPE(0),
                    None,
                    Some(&mut class),
                    None,
                )
                .is_err()
            {
                return false;
            }
            let Some(class) = class else { return false };
            let mut in_def: Option<IWbemClassObject> = None;
            let mut out_def: Option<IWbemClassObject> = None;
            let method: Vec<u16> = "WmiSetBrightness".encode_utf16().chain(Some(0)).collect();
            if class
                .GetMethod(PCWSTR(method.as_ptr()), 0, &mut in_def, &mut out_def)
                .is_err()
            {
                return false;
            }
            let Some(in_def) = in_def else { return false };
            let Some(params) = in_def.SpawnInstance(0).ok() else { return false };
            let timeout_name: Vec<u16> = "Timeout".encode_utf16().chain(Some(0)).collect();
            let brightness_name: Vec<u16> = "Brightness".encode_utf16().chain(Some(0)).collect();
            let timeout = variant_ui4(0);
            let brightness = variant_ui1(normalized_percent(percent));
            if params.Put(PCWSTR(timeout_name.as_ptr()), 0, &timeout, 0).is_err() {
                return false;
            }
            if params.Put(PCWSTR(brightness_name.as_ptr()), 0, &brightness, 0).is_err() {
                return false;
            }
            let mut out: Option<IWbemClassObject> = None;
            services
                .ExecMethod(
                    &path,
                    &BSTR::from("WmiSetBrightness"),
                    WBEM_GENERIC_FLAG_TYPE(0),
                    None,
                    &params,
                    Some(&mut out),
                    None,
                )
                .is_ok()
        }
    }

    #[cfg(test)]
    mod tests {
        use super::normalized_percent;

        #[test]
        fn wmi_brightness_is_clamped_to_byte_range() {
            assert_eq!(normalized_percent(0), 0);
            assert_eq!(normalized_percent(37), 37);
            assert_eq!(normalized_percent(100), 100);
            assert_eq!(normalized_percent(250), 100);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{percent_from_raw, raw_from_percent};

    #[test]
    fn ddc_brightness_round_trips_and_rejects_an_empty_range() {
        assert_eq!(percent_from_raw(0, 0, 100), Some(0));
        assert_eq!(percent_from_raw(0, 50, 100), Some(50));
        assert_eq!(percent_from_raw(10, 10, 110), Some(0));
        assert_eq!(percent_from_raw(0, 200, 100), Some(100));
        assert_eq!(percent_from_raw(50, 50, 50), None);

        assert_eq!(raw_from_percent(0, 100, 0), Some(0));
        assert_eq!(raw_from_percent(0, 100, 50), Some(50));
        assert_eq!(raw_from_percent(0, 100, 100), Some(100));
        assert_eq!(raw_from_percent(0, 100, 250), Some(100));
        assert_eq!(raw_from_percent(200, 100, 50), None);
    }
}
