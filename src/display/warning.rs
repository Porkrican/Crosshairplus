use crate::ipc::DisplayWarning;
use crate::monitors::MonitorInfo;

pub fn for_monitor(monitor: &MonitorInfo) -> Option<DisplayWarning> {
    if monitor.hdr {
        return Some(DisplayWarning {
            code: "hdr".into(),
            message: "Windows controls color, so calibration may not show.".into(),
        });
    }
    if monitor.advanced_color {
        return Some(DisplayWarning {
            code: "acm".into(),
            message: "It overrides calibration.".into(),
        });
    }
    if monitor.night_light {
        return Some(DisplayWarning {
            code: "night_light".into(),
            message: "Turn it off to see changes.".into(),
        });
    }
    None
}

pub fn night_light_active() -> bool {
    use windows::core::PCWSTR;
    use windows::Win32::System::Registry::{RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_BINARY};
    let path = "Software\\Microsoft\\Windows\\CurrentVersion\\CloudStore\\Store\\DefaultAccount\\Current\\default$windows.data.bluelightreduction.bluelightreductionstate\\windows.data.bluelightreduction.bluelightreductionstate";
    let wide: Vec<u16> = path.encode_utf16().chain(Some(0)).collect();
    let value: Vec<u16> = "Data".encode_utf16().chain(Some(0)).collect();
    let mut data = [0u8; 64];
    let mut size = data.len() as u32;
    unsafe {
        let status = RegGetValueW(
            HKEY_CURRENT_USER,
            PCWSTR(wide.as_ptr()),
            PCWSTR(value.as_ptr()),
            RRF_RT_REG_BINARY,
            None,
            Some(data.as_mut_ptr() as *mut core::ffi::c_void),
            Some(&mut size),
        );
        if status.is_ok() && size > 18 {
            // Windows stores the enabled flag in byte 18 (0x15 = on, 0x10 = off).
            return data[18] == 0x15;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn monitor(hdr: bool, advanced_color: bool, night_light: bool) -> MonitorInfo {
        MonitorInfo {
            name: "Test Display".into(),
            hdr,
            advanced_color,
            night_light,
            ..Default::default()
        }
    }

    #[test]
    fn warning_precedence_is_hdr_then_acm_then_night_light() {
        let hdr = for_monitor(&monitor(true, true, true)).unwrap();
        assert_eq!(hdr.code, "hdr");
        let acm = for_monitor(&monitor(false, true, true)).unwrap();
        assert_eq!(acm.code, "acm");
        let night = for_monitor(&monitor(false, false, true)).unwrap();
        assert_eq!(night.code, "night_light");
        assert!(for_monitor(&monitor(false, false, false)).is_none());
    }
}
