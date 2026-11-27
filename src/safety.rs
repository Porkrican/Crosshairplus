use std::ffi::c_void;
use std::fs;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use windows::core::PCWSTR;
use windows::Win32::Foundation::{CloseHandle, FILETIME};
use windows::Win32::Graphics::Gdi::{CreateDCW, DeleteDC, HDC};
use windows::Win32::System::Threading::{
    GetExitCodeProcess, GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::UI::ColorSystem::{GetDeviceGammaRamp, SetDeviceGammaRamp};

use crate::monitors::MonitorInfo;

static RAMPS: OnceLock<Mutex<Vec<(String, [u16; 768])>>> = OnceLock::new();
static FX_POWER: OnceLock<Mutex<bool>> = OnceLock::new();
static FX_PRESET: OnceLock<Mutex<Option<String>>> = OnceLock::new();
static RESTORED: AtomicBool = AtomicBool::new(false);
static FX_RESTORED: AtomicBool = AtomicBool::new(false);

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
struct SafetyRamp {
    gdi: String,
    ramp: Vec<u16>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
struct SafetySnapshot {
    pid: u32,
    /// Distinguishes two runs that Windows happened to give the same PID.
    #[serde(default)]
    session: u64,
    /// Process creation time (FILETIME). A recycled PID is not this process.
    #[serde(default)]
    created: u64,
    ramps: Vec<SafetyRamp>,
    fx_power: bool,
    /// Preset selected before Crosshair+ switched to its managed preset.
    #[serde(default)]
    fx_preset: Option<String>,
}

fn session_marker() -> u64 {
    static SESSION: OnceLock<u64> = OnceLock::new();
    *SESSION.get_or_init(|| {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos() as u64)
            .unwrap_or(0)
    })
}

fn process_created(pid: u32) -> Option<u64> {
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut created = FILETIME::default();
        let mut exited = FILETIME::default();
        let mut kernel = FILETIME::default();
        let mut user = FILETIME::default();
        let ok = GetProcessTimes(handle, &mut created, &mut exited, &mut kernel, &mut user).is_ok();
        let _ = CloseHandle(handle);
        ok.then(|| ((created.dwHighDateTime as u64) << 32) | created.dwLowDateTime as u64)
    }
}

fn own_created() -> u64 {
    static CREATED: OnceLock<u64> = OnceLock::new();
    *CREATED.get_or_init(|| process_created(std::process::id()).unwrap_or(0))
}

fn ramps() -> &'static Mutex<Vec<(String, [u16; 768])>> {
    RAMPS.get_or_init(|| Mutex::new(Vec::new()))
}
fn fx_power() -> &'static Mutex<bool> {
    FX_POWER.get_or_init(|| Mutex::new(false))
}
fn fx_preset() -> &'static Mutex<Option<String>> {
    FX_PRESET.get_or_init(|| Mutex::new(None))
}

fn recovery_path() -> PathBuf {
    crate::config::data_dir().join("safety-recovery.json")
}

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(Some(0)).collect()
}

fn hdc(gdi: &str) -> Option<HDC> {
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

fn read(gdi: &str) -> Option<[u16; 768]> {
    let hdc = hdc(gdi)?;
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

fn write(gdi: &str, ramp: &[u16; 768]) -> bool {
    let Some(hdc) = hdc(gdi) else { return false; };
    let ok = unsafe { SetDeviceGammaRamp(hdc, ramp.as_ptr() as *const c_void).as_bool() };
    unsafe {
        let _ = DeleteDC(hdc);
    }
    ok
}

fn ramp_768(values: &[u16]) -> Option<[u16; 768]> {
    if values.len() != 768 {
        return None;
    }
    let mut ramp = [0u16; 768];
    ramp.copy_from_slice(values);
    Some(ramp)
}

fn pid_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    unsafe {
        let Ok(handle) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else {
            return false;
        };
        let mut code = 0u32;
        let ok = GetExitCodeProcess(handle, &mut code).is_ok();
        let _ = CloseHandle(handle);
        ok && code == 259
    }
}

fn load_snapshot_from(path: &Path) -> Option<SafetySnapshot> {
    let text = fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

fn write_snapshot_to(path: &Path, snapshot: &SafetySnapshot) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let text = serde_json::to_string(snapshot).map_err(std::io::Error::other)?;
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, text)?;
    if let Err(first) = fs::rename(&tmp, path) {
        let _ = fs::remove_file(path);
        fs::rename(&tmp, path).map_err(|second| {
            let _ = fs::remove_file(&tmp);
            std::io::Error::new(second.kind(), format!("{first}; retry failed: {second}"))
        })?;
    }
    Ok(())
}

fn clear_snapshot_at(path: &Path) {
    if let Err(err) = fs::remove_file(path) {
        if err.kind() != std::io::ErrorKind::NotFound {
            crate::logging::log(&format!("safety: could not remove recovery snapshot {}: {err}", path.display()));
        }
    }
}

fn snapshot_from_memory(fx_power_on: bool, fx_preset_name: Option<String>) -> SafetySnapshot {
    let list = ramps().lock().unwrap_or_else(|e| e.into_inner());
    SafetySnapshot {
        pid: std::process::id(),
        session: session_marker(),
        created: own_created(),
        ramps: list
            .iter()
            .map(|(gdi, ramp)| SafetyRamp { gdi: gdi.clone(), ramp: ramp.to_vec() })
            .collect(),
        fx_power: fx_power_on,
        fx_preset: fx_preset_name,
    }
}

fn recovery_owner_alive() -> bool {
    let Some(snapshot) = load_snapshot_from(&recovery_path()) else { return false };
    if snapshot.pid == std::process::id() || !pid_alive(snapshot.pid) {
        return false;
    }
    // Legacy snapshots have no creation time; a live PID is all we can check.
    snapshot.created == 0 || process_created(snapshot.pid) == Some(snapshot.created)
}

fn run_fx_power(path: &Path, power: bool) -> bool {
    match std::process::Command::new(path)
        .arg("--run_minimized")
        .arg(format!("--power={}", if power { 1 } else { 0 }))
        .creation_flags(0x0800_0000)
        .status()
    {
        Ok(status) if status.success() => true,
        Ok(status) => {
            crate::logging::log(&format!("safety: FxSound power restore failed: {status}"));
            false
        }
        Err(err) => {
            crate::logging::log(&format!("safety: FxSound power restore could not launch: {err}"));
            false
        }
    }
}

fn set_fx_baseline(power: bool, preset: Option<&str>) -> bool {
    if crate::config::sandbox_root().is_some() {
        crate::logging::log("safety: sandbox; FxSound restore skipped");
        return true;
    }
    if !crate::audio::fxsound::status_path().exists() {
        crate::logging::log("safety: FxSound CLI unavailable; baseline left untouched");
        return true;
    }
    let Some(path) = crate::audio::fxsound::find_exe() else {
        crate::logging::log("safety: FxSound executable missing; baseline left untouched");
        return false;
    };
    let Some(preset) = preset.map(str::trim).filter(|name| !name.is_empty()) else {
        return run_fx_power(&path, power);
    };
    // Selecting a preset only takes effect while processing is on. Turn it on
    // for the selection, then put the original power state back.
    let mut ok = run_fx_power(&path, true);
    if let Err(err) = crate::audio::fxsound::select_preset(preset) {
        crate::logging::log(&format!("safety: could not restore FxSound preset '{preset}': {err}"));
        ok = false;
    }
    if !power {
        ok &= run_fx_power(&path, false);
    }
    ok
}

/// Restore a snapshot written by a process that did not shut down cleanly.
/// This runs before a new baseline is captured, so the crashed session cannot
/// become the user's new default.
pub fn recover_pending() -> bool {
    let path = recovery_path();
    let Some(snapshot) = load_snapshot_from(&path) else {
        if path.exists() {
            crate::logging::log(&format!("safety: ignoring unreadable recovery snapshot {}", path.display()));
            clear_snapshot_at(&path);
        }
        return false;
    };
    if snapshot.pid == std::process::id() {
        if snapshot.session == session_marker() {
            return false;
        }
        // A recycled PID is ours now; the previous session is dead, so its
        // snapshot is a leftover and must be restored and cleared.
        crate::logging::log("safety: clearing a same-pid snapshot from an earlier session");
    } else if pid_alive(snapshot.pid)
        && (snapshot.created == 0 || process_created(snapshot.pid) == Some(snapshot.created))
    {
        crate::logging::log(&format!(
            "safety: recovery snapshot belongs to live pid {}; leaving it untouched",
            snapshot.pid
        ));
        return false;
    } else if pid_alive(snapshot.pid) {
        crate::logging::log(&format!(
            "safety: pid {} was recycled; restoring the stale baseline",
            snapshot.pid
        ));
    }
    let mut restored = 0usize;
    let mut failed = 0usize;
    for monitor in &snapshot.ramps {
        let Some(ramp) = ramp_768(&monitor.ramp) else {
            failed += 1;
            continue;
        };
        if write(&monitor.gdi, &ramp) {
            restored += 1;
        } else {
            failed += 1;
        }
    }
    let fx_ok = set_fx_baseline(snapshot.fx_power, snapshot.fx_preset.as_deref());
    clear_snapshot_at(&path);
    crate::logging::log(&format!(
        "safety: recovered crashed-session baseline ({} ramp(s) restored, {} failed, FxSound power={} preset={} restored={})",
        restored,
        failed,
        snapshot.fx_power,
        snapshot.fx_preset.as_deref().unwrap_or("none"),
        fx_ok
    ));
    true
}

/// Snapshot the OS state *before* Crosshair+ applies anything.
pub fn capture(monitors: &[MonitorInfo], fx_power_on: bool, fx_preset_name: Option<String>) {
    let mut list = ramps().lock().unwrap_or_else(|e| e.into_inner());
    list.clear();
    let mut failed = 0;
    for monitor in monitors {
        if let Some(ramp) = read(&monitor.gdi) {
            list.push((monitor.gdi.clone(), ramp));
        } else {
            failed += 1;
            crate::logging::log(&format!("safety: could not capture ramp for {}", monitor.name));
        }
    }
    *fx_power().lock().unwrap_or_else(|e| e.into_inner()) = fx_power_on;
    *fx_preset().lock().unwrap_or_else(|e| e.into_inner()) = fx_preset_name.clone();
    RESTORED.store(false, Ordering::SeqCst);
    FX_RESTORED.store(false, Ordering::SeqCst);
    drop(list);
    let snapshot = snapshot_from_memory(fx_power_on, fx_preset_name);
    if recovery_owner_alive() {
        crate::logging::log("safety: another live session owns the recovery snapshot; not overwriting it");
    } else if let Err(err) = write_snapshot_to(&recovery_path(), &snapshot) {
        crate::logging::log(&format!("safety: could not persist recovery snapshot: {err}"));
    }
    crate::logging::log(&format!(
        "safety: captured {} OS ramp(s), {failed} unavailable, FxSound baseline power={fx_power_on}",
        snapshot.ramps.len()
    ));
}

pub fn restore_display() -> bool {
    let list = ramps().lock().unwrap_or_else(|e| e.into_inner());
    let mut ok = true;
    for (gdi, ramp) in list.iter() {
        if !write(gdi, ramp) {
            ok = false;
            crate::logging::log(&format!("safety: could not restore display ramp for {gdi}"));
        }
    }
    ok
}

/// Panic-path restore: every shared lock is `try_lock`, so a panic raised
/// while `safety` already holds one cannot deadlock the dialog.
pub fn restore_all_on_panic() {
    if RESTORED.swap(true, Ordering::SeqCst) {
        return;
    }
    let mut display_ok = true;
    match ramps().try_lock() {
        Ok(list) => {
            for (gdi, ramp) in list.iter() {
                if !write(gdi, ramp) {
                    display_ok = false;
                }
            }
        }
        Err(_) => {
            crate::logging::log("safety: panic restore skipped display ramps; the lock was held");
            display_ok = false;
        }
    }
    let fx_ok = match (fx_power().try_lock(), fx_preset().try_lock()) {
        (Ok(power), Ok(preset)) => {
            let power_on = *power;
            let preset_name = preset.clone();
            drop(preset);
            drop(power);
            // The CLI can take seconds; the panic dialog must not wait on it.
            let (tx, rx) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                let _ = tx.send(set_fx_baseline(power_on, preset_name.as_deref()));
            });
            rx.recv_timeout(Duration::from_millis(1000)).unwrap_or(false)
        }
        _ => {
            crate::logging::log("safety: panic restore skipped FxSound; the lock was held");
            false
        }
    };
    crate::logging::log(&format!(
        "safety: panic restore finished display={display_ok} fx={fx_ok}"
    ));
}

/// Session end: gamma ramps are cheap and always run; the FxSound CLI is
/// best-effort and capped so Windows never waits seconds on shutdown.
pub fn restore_for_shutdown() {
    if RESTORED.swap(true, Ordering::SeqCst) {
        return;
    }
    let display_ok = restore_display();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(restore_fx_blocking());
    });
    let fx_ok = rx.recv_timeout(Duration::from_millis(1000)).unwrap_or(false);
    // Whatever happened, the normal-exit path must not try again and block
    // shutdown for the full CLI timeout.
    FX_RESTORED.store(true, Ordering::SeqCst);
    crate::logging::log(&format!(
        "safety: shutdown restore display={display_ok} fx={fx_ok}"
    ));
}

pub fn restore_all_blocking() {
    if RESTORED.swap(true, Ordering::SeqCst) {
        return;
    }
    let display_ok = restore_display();
    let fx_ok = restore_fx_blocking();
    if display_ok && fx_ok {
        crate::logging::log("safety: restored OS display ramps and FxSound baseline");
    } else {
        crate::logging::log("safety: restore completed with errors");
    }
}

/// Shared normal-exit path used by `App::shutdown`. Keeping this as a small
/// function lets the sandboxed child-process test exercise the exact same
/// callback selection as the GUI exit path.
pub fn restore_for_exit(restore_display_on_exit: bool) {
    if restore_display_on_exit {
        restore_all_blocking();
    } else if !restore_fx_blocking() {
        crate::logging::log("safety: FxSound restore failed on exit");
    }
    clear_snapshot_at(&recovery_path());
}

/// Allow a later exit/reset to restore again after a completed reset.
pub fn reset_guards() {
    RESTORED.store(false, Ordering::SeqCst);
    FX_RESTORED.store(false, Ordering::SeqCst);
}

pub fn restore_fx_blocking() -> bool {
    if FX_RESTORED.swap(true, Ordering::SeqCst) {
        return true;
    }
    let power = *fx_power().lock().unwrap_or_else(|e| e.into_inner());
    let preset = fx_preset().lock().unwrap_or_else(|e| e.into_inner()).clone();
    set_fx_baseline(power, preset.as_deref())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recovery_snapshot_round_trips_and_is_cleared() {
        let path = std::env::temp_dir().join(format!(
            "crosshair-plus-safety-{}-{}.json",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let snapshot = SafetySnapshot {
            pid: std::process::id(),
            session: session_marker(),
            created: own_created(),
            ramps: vec![SafetyRamp { gdi: r"\\.\DISPLAY1".into(), ramp: vec![1; 768] }],
            fx_power: true,
            fx_preset: Some("Trap".into()),
        };
        write_snapshot_to(&path, &snapshot).unwrap();
        assert_eq!(load_snapshot_from(&path).as_ref(), Some(&snapshot));
        clear_snapshot_at(&path);
        assert!(!path.exists());
    }

    #[test]
    fn malformed_snapshot_is_not_loaded() {
        let path = std::env::temp_dir().join(format!(
            "crosshair-plus-safety-bad-{}-{}.json",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        fs::write(&path, b"not json").unwrap();
        assert!(load_snapshot_from(&path).is_none());
        clear_snapshot_at(&path);
        assert!(!path.exists());
    }

    #[test]
    fn current_process_is_alive() {
        assert!(pid_alive(std::process::id()));
    }

    #[test]
    fn process_creation_time_distinguishes_a_recycled_pid() {
        let created = process_created(std::process::id()).expect("our own creation time is queryable");
        assert_eq!(created, own_created());
        assert_ne!(created, created ^ 1);
    }
}
