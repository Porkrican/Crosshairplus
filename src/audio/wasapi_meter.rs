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

/// A usable peak is finite and inside the mixer's range. WASAPI can report
/// values outside it while an endpoint settles, so both ends are clamped
/// instead of being published as-is.
fn clamp_peak(value: f32) -> Option<f32> {
    if value.is_finite() {
        Some(value.clamp(0.0, 1.0))
    } else {
        None
    }
}

/// Test-only probe: the hardware tests below assert real loopback metering only
/// where a playback endpoint exists, and the documented no-device behaviour
/// everywhere else. CI runners have no audio hardware at all.
#[cfg(test)]
fn default_render_endpoint_available() -> bool {
    enumerator()
        .ok()
        .map(|enumerator| unsafe { enumerator.GetDefaultAudioEndpoint(eRender, eConsole) }.is_ok())
        .unwrap_or(false)
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
        let reading = unsafe { meter.GetPeakValue() }.ok().and_then(clamp_peak);
        if reading.is_none() {
            self.stale = true;
        }
        reading
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
    fn peak_clamping_rejects_non_finite_values_and_bounds_the_range() {
        assert_eq!(clamp_peak(0.0), Some(0.0));
        assert_eq!(clamp_peak(0.25), Some(0.25));
        assert_eq!(clamp_peak(1.0), Some(1.0));
        assert_eq!(clamp_peak(1.5), Some(1.0), "a hot mixer reading must clamp to 1");
        assert_eq!(clamp_peak(-0.25), Some(0.0), "a negative reading must clamp to 0");
        assert_eq!(clamp_peak(f32::NAN), None, "NaN is not a peak");
        assert_eq!(clamp_peak(f32::INFINITY), None, "infinity is not a peak");
        assert_eq!(clamp_peak(f32::NEG_INFINITY), None, "-infinity is not a peak");
    }

    #[test]
    fn default_render_endpoint_exposes_loopback_meter() {
        let mut meter = PeakMeter::new();
        meter.refresh(None);
        if !default_render_endpoint_available() {
            // CI runners and other audio-less sessions have no render endpoint.
            // The documented behaviour there is "no meter", never a panic.
            assert!(!meter.is_active(), "a session without a render endpoint exposed a meter");
            assert!(meter.peak().is_none(), "a session without a render endpoint returned a peak");
            assert!(meter.needs_refresh(), "a device-less refresh must keep polling for a later endpoint");
            return;
        }
        assert!(meter.is_active(), "default render endpoint did not expose IAudioMeterInformation");
        assert!(meter.peak().is_some(), "IAudioMeterInformation::GetPeakValue returned no peak");
    }

    #[test]
    fn meter_refreshes_falls_back_and_clamps_peak() {
        let mut meter = PeakMeter::new();
        assert!(meter.needs_refresh(), "new meter did not request its first refresh");
        meter.refresh(Some("Crosshair+ definitely missing output device"));
        if !default_render_endpoint_available() {
            assert!(!meter.is_active(), "the missing-device fallback invented a meter");
            assert!(meter.peak().is_none(), "the missing-device fallback returned a peak");
            assert!(meter.needs_refresh(), "a device-less refresh must keep polling for a later endpoint");
            meter.refresh(None);
            assert!(!meter.is_active(), "a no-endpoint refresh exposed a meter");
            return;
        }
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
