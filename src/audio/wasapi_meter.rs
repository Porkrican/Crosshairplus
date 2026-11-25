use windows::core::Result;
use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
use windows::Win32::Media::Audio::Endpoints::IAudioMeterInformation;
use windows::Win32::Media::Audio::{
    eConsole, eRender, IMMDevice, IMMDeviceCollection, IMMDeviceEnumerator, MMDeviceEnumerator, DEVICE_STATE_ACTIVE,
};
use windows::Win32::System::Com::StructuredStorage::{PropVariantToStringAlloc, PROPVARIANT};
use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CoTaskMemFree, CLSCTX_ALL, COINIT_APARTMENTTHREADED, STGM_READ};

use super::AudioDevice;

pub fn enumerator() -> Result<IMMDeviceEnumerator> {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
    }
}

fn device_name(device: &IMMDevice) -> Option<String> {
    unsafe {
        let store = device.OpenPropertyStore(STGM_READ).ok()?;
        let prop: PROPVARIANT = store.GetValue(&PKEY_Device_FriendlyName).ok()?;
        let ptr = PropVariantToStringAlloc(&prop).ok()?;
        let value = ptr.to_string().ok();
        CoTaskMemFree(Some(ptr.0 as *const core::ffi::c_void));
        value.map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
    }
}

fn device_id(device: &IMMDevice) -> Option<String> {
    unsafe {
        let ptr = device.GetId().ok()?;
        let value = ptr.to_string().ok();
        CoTaskMemFree(Some(ptr.0 as *const core::ffi::c_void));
        value
    }
}

fn endpoints(enumerator: &IMMDeviceEnumerator) -> Option<IMMDeviceCollection> {
    unsafe { enumerator.EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE).ok() }
}

pub fn enumerate_render_devices() -> Result<Vec<AudioDevice>> {
    let enumerator = enumerator()?;
    let default_id = unsafe { enumerator.GetDefaultAudioEndpoint(eRender, eConsole).ok() }
        .as_ref()
        .and_then(device_id);
    let mut out = Vec::new();
    if let Some(collection) = endpoints(&enumerator) {
        let count = unsafe { collection.GetCount().unwrap_or(0) };
        for i in 0..count {
            let Ok(device) = (unsafe { collection.Item(i) }) else { continue };
            let Some(name) = device_name(&device) else { continue };
            if name.to_ascii_lowercase().contains("fxsound audio enhancer") {
                continue;
            }
            let id = device_id(&device).unwrap_or_else(|| name.clone());
            let is_default = default_id.as_deref() == Some(id.as_str());
            out.push(AudioDevice { id, name, is_default });
        }
    }
    Ok(out)
}

pub struct PeakMeter {
    enumerator: Option<IMMDeviceEnumerator>,
    meter: Option<IAudioMeterInformation>,
    current: Option<String>,
    stale: bool,
}

impl PeakMeter {
    pub fn new() -> Self {
        Self { enumerator: enumerator().ok(), meter: None, current: None, stale: true }
    }

    pub fn needs_refresh(&self) -> bool {
        self.stale || self.meter.is_none()
    }

    #[cfg(test)]
    pub fn is_active(&self) -> bool {
        self.meter.is_some()
    }

    pub fn refresh(&mut self, wanted_name: Option<&str>) {
        self.stale = false;
        if self.enumerator.is_none() {
            self.enumerator = enumerator().ok();
        }
        let Some(enumerator) = self.enumerator.as_ref() else {
            self.meter = None;
            return;
        };
        let mut target: Option<IMMDevice> = None;
        if let Some(wanted) = wanted_name.filter(|w| !w.trim().is_empty()) {
            if let Some(collection) = endpoints(enumerator) {
                let count = unsafe { collection.GetCount().unwrap_or(0) };
                for i in 0..count {
                    let Ok(device) = (unsafe { collection.Item(i) }) else { continue };
                    let name = device_name(&device).unwrap_or_default();
                    if name.eq_ignore_ascii_case(wanted) {
                        target = Some(device);
                        break;
                    }
                }
            }
        }
        if target.is_none() {
            target = unsafe { enumerator.GetDefaultAudioEndpoint(eRender, eConsole).ok() };
        }
        let Some(device) = target else {
            self.meter = None;
            return;
        };
        let id = device_id(&device);
        if self.meter.is_some() && id.is_some() && id == self.current {
            return;
        }
        self.current = id;
        self.meter = unsafe { device.Activate::<IAudioMeterInformation>(CLSCTX_ALL, None).ok() };
    }

    pub fn peak(&mut self) -> Option<f32> {
        let meter = self.meter.as_ref()?;
        match unsafe { meter.GetPeakValue() } {
            Ok(v) if v.is_finite() => Some(v.clamp(0.0, 1.0)),
            _ => {
                self.stale = true;
                None
            }
        }
    }
}

impl Default for PeakMeter {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_render_endpoint_exposes_loopback_meter() {
        let mut meter = PeakMeter::new();
        meter.refresh(None);
        assert!(meter.is_active(), "default render endpoint did not expose IAudioMeterInformation");
        assert!(meter.peak().is_some(), "IAudioMeterInformation::GetPeakValue returned no peak");
    }

    #[test]
    fn meter_refreshes_falls_back_and_clamps_peak() {
        let mut meter = PeakMeter::new();
        assert!(meter.needs_refresh(), "new meter did not request its first refresh");
        meter.refresh(Some("Crosshair+ definitely missing output device"));
        assert!(!meter.needs_refresh(), "refresh left the meter stale");
        assert!(meter.is_active(), "default endpoint fallback did not expose a meter");
        for _ in 0..4 {
            let peak = meter.peak().expect("fallback endpoint returned no peak");
            assert!(peak.is_finite(), "peak was not finite: {peak}");
            assert!((0.0..=1.0).contains(&peak), "peak escaped 0..=1: {peak}");
        }
        meter.refresh(None);
        assert!(meter.is_active(), "default-device refresh dropped the meter");
    }
}
