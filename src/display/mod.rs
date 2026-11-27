pub mod curves;
pub mod ddc;
pub mod edid;
pub mod panel;
pub mod recovery;
pub mod runtime;
pub mod warning;

use std::collections::HashMap;
use std::ffi::c_void;

use windows::core::PCWSTR;
use windows::Win32::Graphics::Gdi::{CreateDCW, DeleteDC, HDC};
use windows::Win32::UI::ColorSystem::GetDeviceGammaRamp;

use crate::config::{DisplayConfig, DisplayValues};
use crate::ipc::DisplayWarning;
use crate::monitors::{self, MonitorInfo};

use curves::Luts;

pub struct DisplayEngine {
    baseline: HashMap<String, (String, [u16; 768])>,
    applied_ids: Vec<String>,
    state: DisplayRuntimeState,
    dry_run: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DisplayRuntimeState {
    pub enabled: bool,
    pub applied: bool,
    pub needs_restore: bool,
    pub apply_calls: u64,
    pub restore_calls: u64,
    pub applied_ids: Vec<String>,
}

pub struct ApplyOutcome {
    pub luts: Luts,
    pub applied: bool,
    pub warning: Option<DisplayWarning>,
}

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(Some(0)).collect()
}

fn hdc_for(gdi: &str) -> Option<HDC> {
    let driver = wide("DISPLAY");
    let device = wide(gdi);
    unsafe {
        let hdc = CreateDCW(PCWSTR(driver.as_ptr()), PCWSTR(device.as_ptr()), PCWSTR::null(), None);
        if hdc.0.is_null() {
            None
        } else {
            Some(hdc)
        }
    }
}

fn read_ramp(gdi: &str) -> Option<[u16; 768]> {
    let hdc = hdc_for(gdi)?;
    let mut ramp = [0u16; 768];
    let ok = unsafe { GetDeviceGammaRamp(hdc, ramp.as_mut_ptr() as *mut c_void).as_bool() };
    unsafe {
        let _ = DeleteDC(hdc);
    }
    if ok {
        Some(ramp)
    } else {
        None
    }
}

fn write_ramp(gdi: &str, ramp: &[u16; 768]) -> bool {
    let Some(hdc) = hdc_for(gdi) else { return false };
    let ok = unsafe { windows::Win32::UI::ColorSystem::SetDeviceGammaRamp(hdc, ramp.as_ptr() as *const c_void).as_bool() };
    unsafe {
        let _ = DeleteDC(hdc);
    }
    ok
}

fn verify_ramp(gdi: &str, expected: &[u16; 768]) -> bool {
    let Some(actual) = read_ramp(gdi) else { return false };
    expected.iter().zip(actual.iter()).all(|(a, b)| (*a as i32 - *b as i32).abs() <= 257)
}

impl DisplayEngine {
    pub fn new(monitors: &[MonitorInfo]) -> Self {
        let mut baseline = HashMap::new();
        for monitor in monitors {
            if let Some(ramp) = read_ramp(&monitor.gdi) {
                baseline.insert(monitor.id.clone(), (monitor.gdi.clone(), ramp));
            } else {
                crate::logging::log(&format!("display: baseline unavailable for {}; it will be read on first apply", monitor.name));
            }
        }
        Self {
            baseline,
            applied_ids: Vec::new(),
            state: DisplayRuntimeState::default(),
            dry_run: false,
        }
    }

    /// An engine that never reads or writes an OS gamma ramp. Capture and every
    /// automated harness use this so their shutdown restore cannot put a real
    /// baseline back on the desktop.
    pub fn preview_only() -> Self {
        Self {
            baseline: HashMap::new(),
            applied_ids: Vec::new(),
            state: DisplayRuntimeState::default(),
            dry_run: true,
        }
    }

    #[cfg(test)]
    pub fn headless() -> Self {
        Self::preview_only()
    }

    #[cfg(test)]
    pub fn state(&self) -> DisplayRuntimeState {
        self.state.clone()
    }

    pub fn ensure_baselines(&mut self, monitors: &[MonitorInfo]) {
        if self.dry_run {
            return;
        }
        for monitor in monitors {
            self.baseline.entry(monitor.id.clone()).or_insert_with(|| {
                let ramp = read_ramp(&monitor.gdi).unwrap_or_else(|| {
                    crate::logging::log(&format!("display: baseline ramp unavailable for {}; using identity", monitor.name));
                    identity_ramp()
                });
                (monitor.gdi.clone(), ramp)
            });
        }
    }

    pub fn apply(&mut self, monitors: &[MonitorInfo], config: &DisplayConfig) -> ApplyOutcome {
        self.state.enabled = config.enabled;
        self.state.apply_calls += 1;
        if !config.enabled {
            self.restore_all();
            return ApplyOutcome { luts: Luts::neutral(), applied: false, warning: None };
        }
        if self.dry_run {
            let luts = curves::build(&DisplayValues::default());
            self.applied_ids.clear();
            self.applied_ids.push("headless".into());
            self.state.applied = true;
            self.state.needs_restore = true;
            self.state.applied_ids = vec!["headless".into()];
            return ApplyOutcome { luts, applied: true, warning: None };
        }
        self.ensure_baselines(monitors);
        let selected = monitors::find(monitors, config.selected_monitor.as_deref()).clone();
        let values = config.values_for(&selected.id);
        let luts = curves::build_for(&values, Some(&selected));
        if config.mode == "this" {
            let warning = warning::for_monitor(&selected);
            let blocked = warning.as_ref().map(|w| w.code == "hdr" || w.code == "acm").unwrap_or(false);
            let applied = if blocked {
                false
            } else {
                self.set_one(&selected, &luts)
            };
            return ApplyOutcome { luts, applied, warning };
        }

        let mut applied_any = false;
        let mut warning: Option<DisplayWarning> = None;
        for monitor in monitors {
            if let Some(w) = warning::for_monitor(monitor) {
                if warning.is_none() {
                    warning = Some(w.clone());
                }
                if w.code == "hdr" || w.code == "acm" {
                    continue;
                }
            }
            let values = config.values_for(&monitor.id);
            let luts = curves::build_for(&values, Some(monitor));
            if self.set_one(monitor, &luts) {
                applied_any = true;
            }
        }
        ApplyOutcome { luts, applied: applied_any, warning }
    }

    fn set_one(&mut self, monitor: &MonitorInfo, luts: &Luts) -> bool {
        if !self.baseline.contains_key(&monitor.id) {
            let ramp = read_ramp(&monitor.gdi).unwrap_or_else(|| {
                crate::logging::log(&format!("display: baseline ramp unavailable for {}; using identity", monitor.name));
                identity_ramp()
            });
            self.baseline.insert(monitor.id.clone(), (monitor.gdi.clone(), ramp));
        }
        let ramp = luts.to_ramp();
        if !write_ramp(&monitor.gdi, &ramp) {
            crate::logging::log(&format!("display: SetDeviceGammaRamp failed for {}", monitor.name));
            return false;
        }
        let verified = verify_ramp(&monitor.gdi, &ramp);
        crate::logging::log(&format!("display: applied {} verified={}", monitor.name, verified));
        if verified && !self.applied_ids.contains(&monitor.id) {
            self.applied_ids.push(monitor.id.clone());
        }
        self.state.applied = !self.applied_ids.is_empty();
        self.state.needs_restore = self.state.applied;
        self.state.applied_ids = self.applied_ids.clone();
        verified
    }

    pub fn restore_all(&mut self) {
        self.state.restore_calls += 1;
        self.state.applied = false;
        self.state.needs_restore = false;
        let mut restored = 0;
        let mut failed = 0;
        for (_id, (gdi, ramp)) in &self.baseline {
            if write_ramp(gdi, ramp) {
                restored += 1;
            } else {
                failed += 1;
            }
        }
        if restored > 0 {
            crate::logging::log(&format!("display: restored {restored} OS ramp(s)"));
        }
        if failed > 0 {
            crate::logging::log(&format!("display: {failed} OS ramp restore(s) failed"));
        }
        self.applied_ids.clear();
        self.state.applied_ids.clear();
    }

    /// Write identity ramps for a temporary Hold-to-Compare look. This never
    /// touches saved baselines, `applied_ids`, `applied` or `needs_restore`;
    /// the following `apply`/`restore` owns the real state transition. HDR and
    /// ACM displays are skipped for the same reason `apply` skips them.
    pub fn preview_neutral(&self, monitors: &[MonitorInfo], config: &DisplayConfig) -> usize {
        if monitors.is_empty() {
            return 0;
        }
        let identity = identity_ramp();
        let mut written = 0;
        for monitor in selected_monitors(monitors, config) {
            if let Some(code) = neutral_preview_blocked(monitor) {
                crate::logging::log(&format!(
                    "display: neutral preview skipped {} ({code})",
                    monitor.name
                ));
                continue;
            }
            // A dry-run engine still walks the real selection and blocking
            // policy, but it never opens a display DC or writes a ramp.
            if self.dry_run {
                continue;
            }
            if write_ramp(&monitor.gdi, &identity) {
                written += 1;
            } else {
                crate::logging::log(&format!("display: neutral preview failed for {}", monitor.name));
            }
        }
        crate::logging::log(&format!("display: neutral preview written to {written} monitor(s)"));
        written
    }

    /// Pure preview path used by capture mode. It computes the same LUT that
    /// `apply` would build, without reading or writing an OS gamma ramp.
    pub fn preview(config: &DisplayConfig, monitor: &MonitorInfo) -> Luts {
        curves::build_for(&config.values_for(&monitor.id), Some(monitor))
    }

    #[cfg(test)]
    pub fn preview_values(values: &DisplayValues) -> Luts {
        curves::build(values)
    }
}

fn selected_monitors<'a>(monitors: &'a [MonitorInfo], config: &DisplayConfig) -> Vec<&'a MonitorInfo> {
    if config.mode == "this" {
        vec![monitors::find(monitors, config.selected_monitor.as_deref())]
    } else {
        monitors.iter().collect()
    }
}

fn neutral_preview_blocked(monitor: &MonitorInfo) -> Option<&'static str> {
    let warning = warning::for_monitor(monitor)?;
    match warning.code.as_str() {
        "hdr" => Some("hdr"),
        "acm" => Some("acm"),
        _ => None,
    }
}

fn identity_ramp() -> [u16; 768] {
    let mut ramp = [0u16; 768];
    for i in 0..256 {
        ramp[i] = i as u16 * 257;
        ramp[256 + i] = i as u16 * 257;
        ramp[512 + i] = i as u16 * 257;
    }
    ramp
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_ramp_is_bijective() {
        let ramp = identity_ramp();
        assert_eq!(ramp[0], 0);
        assert_eq!(ramp[255], 65535);
        assert_eq!(ramp[256 + 128], 128 * 257);
    }

    #[test]
    fn preview_neutral_matches_identity() {
        let luts = DisplayEngine::preview_values(&DisplayValues::default());
        assert_eq!(luts.r[64], 64);
        assert_eq!(luts.g[255], 255);
    }

    #[test]
    fn preview_only_engine_never_captures_a_real_baseline() {
        let monitors = vec![MonitorInfo::default()];
        let mut engine = DisplayEngine::preview_only();
        engine.ensure_baselines(&monitors);
        assert!(engine.baseline.is_empty(), "a preview-only engine must not read OS ramps into a restore baseline");
        engine.restore_all();
        assert_eq!(engine.state().restore_calls, 1);
        assert!(engine.baseline.is_empty(), "restoring a preview-only engine must have nothing to write");
    }

    #[test]
    fn neutral_preview_selects_one_display_and_blocks_hdr_and_acm() {
        let primary = MonitorInfo { id: "a".into(), name: "Primary".into(), primary: true, ..Default::default() };
        let second = MonitorInfo { id: "b".into(), name: "Second".into(), ..Default::default() };
        let hdr = MonitorInfo { id: "hdr".into(), name: "HDR".into(), hdr: true, ..Default::default() };
        let acm = MonitorInfo { id: "acm".into(), name: "ACM".into(), advanced_color: true, ..Default::default() };
        let monitors = [primary, second, hdr, acm];
        let this = DisplayConfig { mode: "this".into(), selected_monitor: Some("b".into()), ..Default::default() };
        let targets = selected_monitors(&monitors, &this);
        assert_eq!(targets.len(), 1, "single-display compare must target only the selected display");
        assert_eq!(targets[0].id, "b");
        let all = DisplayConfig { mode: "all".into(), ..Default::default() };
        assert_eq!(selected_monitors(&monitors, &all).len(), monitors.len());
        assert_eq!(neutral_preview_blocked(&monitors[0]), None);
        assert_eq!(neutral_preview_blocked(&monitors[1]), None);
        assert_eq!(neutral_preview_blocked(&monitors[2]), Some("hdr"));
        assert_eq!(neutral_preview_blocked(&monitors[3]), Some("acm"));
    }

    #[test]
    fn neutral_preview_skips_hdr_and_acm_without_state_changes() {
        let engine = DisplayEngine::headless();
        let before = engine.state();
        let hdr = MonitorInfo { name: "HDR".into(), hdr: true, ..Default::default() };
        let acm = MonitorInfo { name: "ACM".into(), advanced_color: true, ..Default::default() };
        let config = DisplayConfig { mode: "all".into(), ..Default::default() };
        assert_eq!(engine.preview_neutral(&[hdr, acm], &config), 0);
        assert_eq!(engine.state(), before, "neutral preview must not change restore state");
    }

    #[test]
    fn neutral_preview_keeps_applied_and_restore_flags() {
        let mut engine = DisplayEngine::headless();
        engine.apply(&[], &DisplayConfig { enabled: true, ..Default::default() });
        let before = engine.state();
        assert!(before.applied && before.needs_restore);
        assert_eq!(engine.preview_neutral(&[MonitorInfo::default()], &DisplayConfig::default()), 0);
        assert_eq!(engine.state(), before);
    }
}
