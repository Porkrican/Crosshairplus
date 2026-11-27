use std::fs;
use std::os::windows::process::CommandExt;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tao::event_loop::EventLoopProxy;

use crate::app::AppEvent;
use crate::audio::dsp_math;
use crate::audio::verify;
use crate::audio::{AudioDevice, AudioRuntime, FxSoundStatus};
use crate::config::AudioConfig;

const CREATE_NO_WINDOW: u32 = 0x0800_0000;
pub const DOWNLOAD_URL: &str = "https://www.fxsound.com/download";
const RELEASE_API: &str = "https://api.github.com/repos/fxsound2/fxsound-app/releases/latest";
/// The one preset Crosshair+ is allowed to create and select. Keeping the
/// automation on a dedicated preset leaves every user preset untouched.
pub const MANAGED_PRESET: &str = "Crosshair+";
const MANAGED_PRESET_FILE: &str = "Crosshair+.fac";
/// FxSound's fixed 10-band map. The app's internal BAND_FREQS use a different
/// anchor set, but FxSound applies gains by index to these actual centers.
pub(crate) const FXSOUND_10_BANDS: [f32; 10] = [62.5, 100.0, 180.0, 290.0, 650.0, 1200.0, 2200.0, 4550.0, 6363.0, 16000.0];

/// Opt-in stub mode for `--verify-unstick`: the value replaces FxSound's live
/// `%APPDATA%\FxSound` tree, and the stub itself is this executable re-entered
/// with `--fxsound-stub`. Nothing here is reachable from a normal launch or a
/// normal verification mode.
pub(crate) const STUB_DIR_ENV: &str = "CROSSHAIRPLUS_FXSOUND_STUB_DIR";
/// The harness stamps its PID here so a stub it leaves behind exits on its own
/// instead of idling forever.
pub(crate) const STUB_PARENT_ENV: &str = "CROSSHAIRPLUS_FXSOUND_STUB_PARENT";
pub(crate) fn stub_dir() -> Option<PathBuf> {
    std::env::var_os(STUB_DIR_ENV)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}
pub(crate) fn stub_mode() -> bool {
    stub_dir().is_some()
}
fn fxsound_data_dir() -> PathBuf {
    if let Some(dir) = stub_dir() {
        return dir;
    }
    let appdata = std::env::var("APPDATA").unwrap_or_else(|_| ".".into());
    PathBuf::from(appdata).join("FxSound")
}
pub fn settings_path() -> PathBuf {
    fxsound_data_dir().join("FxSound.settings")
}
pub fn status_path() -> PathBuf {
    fxsound_data_dir().join("status.json")
}

pub(crate) fn presets_dir() -> PathBuf {
    settings_path().parent().unwrap_or_else(|| Path::new(".")).join("Presets")
}
fn managed_preset_path() -> PathBuf {
    presets_dir().join(MANAGED_PRESET_FILE)
}
fn preset_owner_path() -> PathBuf {
    crate::config::data_dir().join("fxsound-preset.json")
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
struct PresetOwnership {
    name: String,
    file: String,
    fingerprint: String,
}

/// A small non-cryptographic fingerprint is enough here: it detects a user edit
/// to the managed preset without adding a hashing dependency.
fn fingerprint(bytes: &[u8]) -> String {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for byte in bytes {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

fn load_preset_ownership(path: &Path) -> Option<PresetOwnership> {
    let text = fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

fn save_preset_ownership_at(path: &Path, ownership: &PresetOwnership) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let text = serde_json::to_string_pretty(ownership).map_err(|e| e.to_string())?;
    fs::write(&path, text).map_err(|e| format!("could not write {}: {e}", path.display()))
}

fn same_file(left: &str, right: &Path) -> bool {
    left.eq_ignore_ascii_case(&right.to_string_lossy())
}

fn preset_is_owned_at(path: &Path, bytes: &[u8], owner_path: &Path) -> bool {
    load_preset_ownership(owner_path)
        .map(|owner| {
            owner.name == MANAGED_PRESET
                && same_file(&owner.file, path)
                && owner.fingerprint == fingerprint(bytes)
        })
        .unwrap_or(false)
}

fn status_has_preset(name: &str) -> bool {
    let Ok(value) = read_status_json() else { return false };
    let selected = value.get("selected_preset").and_then(Value::as_str) == Some(name);
    let listed = value
        .get("presets")
        .and_then(|presets| presets.get("user_defined"))
        .and_then(Value::as_array)
        .map(|presets| {
            presets
                .iter()
                .any(|preset| preset.get("name").and_then(Value::as_str) == Some(name))
        })
        .unwrap_or(false);
    selected || listed
}

/// The verifier runs inside a sandbox, so `config::data_dir()` would point at
/// the sandbox copy. Its ownership-marker check must read the real one.
fn live_preset_owner_path() -> PathBuf {
    let appdata = std::env::var("APPDATA").unwrap_or_else(|_| ".".into());
    PathBuf::from(appdata).join("CrosshairPlus").join("fxsound-preset.json")
}

/// Read-only inspection for `--verify-fxsound`: it reads the managed `.fac`,
/// the live ownership marker and the existing `status.json`, but never launches
/// FxSound, runs `--status`, or writes any of those files.
pub fn managed_preset_report() -> Value {
    let path = managed_preset_path();
    let marker = live_preset_owner_path();
    let bytes = fs::read(&path).ok();
    let ownership = load_preset_ownership(&marker);
    serde_json::json!({
        "file": path.to_string_lossy(),
        "filePresent": path.is_file(),
        "marker": marker.to_string_lossy(),
        "markerPresent": marker.is_file(),
        "markerValid": ownership.is_some(),
        "markerName": ownership.as_ref().map(|owner| owner.name.clone()),
        "fingerprintMatches": bytes.as_ref().map(|bytes| preset_is_owned_at(&path, bytes, &marker)).unwrap_or(false),
        "listedInStatus": status_has_preset(MANAGED_PRESET),
        "statusPath": status_path().to_string_lossy(),
        "statusPresent": status_path().is_file(),
    })
}

fn fmt_gain(value: f32) -> String {
    let text = format!("{value:.3}");
    let text = text.trim_end_matches('0').trim_end_matches('.');
    if text == "-0" { "0".into() } else { text.into() }
}

/// Serialize the same small `.fac` shape FxSound ships for its factory presets.
/// Crosshair+ writes only a base preset; the live, fine-grained curve still
/// reaches FxSound through the CLI and lands in `AutoSave\Crosshair+.fac`.
fn managed_preset_text(audio: &AudioConfig) -> String {
    let (freqs, gains) = dsp_math::fx_bands(audio);
    let mut text = String::from(
        "CLASS1 : Effect Type\r\n9: Version\r\nCrosshair+\r\n0: Double Params Flag\r\n1: Total number of elements\r\n76: Main 0\r\n0: Main 1\r\n0: Main 2\r\n89: Main 3\r\n38: Main 4\r\n76: Main 5\r\n0: Element Number\r\n   0: Param 0\r\n   0: Param 1\r\n   0: Param 2\r\n   0: Param 3\r\n   0: Param 4\r\n   0: Param 5\r\n   0: Param 6\r\n7: Number of Application Dependent Integers\r\n0: Number of Application Dependent Reals\r\n0: Number of Application Dependent Strings\r\n1: Integer[0]\r\n1: Integer[1]\r\n1: Integer[2]\r\n1: Integer[3]\r\n1: Integer[4]\r\n0: Integer[5]\r\n2: Integer[6]\r\n",
    );
    text.push_str(&format!("{}: Number of EQ Bands\r\n1: On/Off Flag\r\n", freqs.len()));
    let display_freqs: Vec<f32> = if freqs.len() == FXSOUND_10_BANDS.len() {
        FXSOUND_10_BANDS.to_vec()
    } else {
        freqs.clone()
    };
    for (index, (freq, gain)) in display_freqs.iter().zip(gains.iter()).enumerate() {
        text.push_str(&format!(
            "Band {}\r\n   {}: CF\r\n   {}: Boost/Cut\r\n",
            index + 1,
            fmt_gain(*freq),
            fmt_gain(*gain)
        ));
    }
    text
}

fn ensure_original_backup() -> Result<(), String> {
    backup_settings()
}

fn restart_for_managed_preset(force: bool) -> Result<(), String> {
    let before = status_mtime();
    crate::logging::log(if force {
        "fxsound: managed preset replaced; forcing one FxSound restart"
    } else {
        "fxsound: Crosshair+ preset not listed; restarting once"
    });
    stop_fxsound();
    if !wait_until_stopped(Duration::from_secs(4)) {
        return Err("FxSound did not stop while loading the Crosshair+ preset.".into());
    }
    start_fxsound_minimized()?;
    if !wait_until_running(Duration::from_secs(5)) {
        return Err("FxSound did not start while loading the Crosshair+ preset.".into());
    }
    let _ = wait_until_status_ready(before, Duration::from_secs(3));
    hide_fxsound_window();
    if !status_has_preset(MANAGED_PRESET) {
        let _ = refresh_status();
    }
    if status_has_preset(MANAGED_PRESET) {
        crate::logging::log("fxsound: Crosshair+ preset loaded after restart");
        Ok(())
    } else {
        Err("FxSound did not load the new Crosshair+ preset after a restart.".into())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PresetConflict {
    UserFile(PathBuf),
    StatusOnly,
}

fn managed_preset_conflict_at(path: &Path, owner_path: &Path, listed: bool) -> Option<PresetConflict> {
    if path.is_file() {
        let existing = fs::read(path).unwrap_or_default();
        if !preset_is_owned_at(path, &existing, owner_path) {
            return Some(PresetConflict::UserFile(path.to_path_buf()));
        }
    } else if listed {
        return Some(PresetConflict::StatusOnly);
    }
    None
}

pub fn managed_preset_conflict() -> Option<PresetConflict> {
    managed_preset_conflict_at(&managed_preset_path(), &preset_owner_path(), status_has_preset(MANAGED_PRESET))
}

fn install_managed_preset_at(path: &Path, owner_path: &Path, audio: &AudioConfig) -> Result<PathBuf, String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let bytes = managed_preset_text(audio);
    let tmp = path.with_extension("fac.tmp");
    fs::write(&tmp, &bytes).map_err(|e| format!("could not create {}: {e}", tmp.display()))?;
    fs::rename(&tmp, path).map_err(|e| format!("could not install {}: {e}", path.display()))?;
    save_preset_ownership_at(owner_path, &PresetOwnership {
        name: MANAGED_PRESET.into(),
        file: path.to_string_lossy().to_string(),
        fingerprint: fingerprint(bytes.as_bytes()),
    })?;
    crate::logging::log(&format!("fxsound: created owned preset {}", path.display()));
    Ok(path.to_path_buf())
}

fn backup_user_preset(path: &Path, backup_dir: &Path) -> Result<PathBuf, String> {
    fs::create_dir_all(backup_dir).map_err(|e| format!("could not create {}: {e}", backup_dir.display()))?;
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let mut dest = backup_dir.join(format!("Crosshair+-{stamp}.fac"));
    let mut suffix = 1u32;
    while dest.exists() {
        dest = backup_dir.join(format!("Crosshair+-{stamp}-{suffix}.fac"));
        suffix += 1;
    }
    fs::copy(path, &dest).map_err(|e| format!("could not back up {}: {e}", path.display()))?;
    let original = fs::read(path).map_err(|e| format!("could not read {}: {e}", path.display()))?;
    let copied = fs::read(&dest).map_err(|e| format!("could not verify backup {}: {e}", dest.display()))?;
    if original != copied {
        return Err(format!("backup verification failed for {}", dest.display()));
    }
    crate::logging::log(&format!("fxsound: user preset backed up to {}", dest.display()));
    Ok(dest)
}

fn repair_managed_preset_at(
    path: &Path,
    owner_path: &Path,
    backup_dir: &Path,
    audio: &AudioConfig,
    listed: bool,
) -> Result<Option<PathBuf>, String> {
    match managed_preset_conflict_at(path, owner_path, listed) {
        None => Ok(None),
        Some(PresetConflict::StatusOnly) => {
            install_managed_preset_at(path, owner_path, audio)?;
            Ok(None)
        }
        Some(PresetConflict::UserFile(conflict_path)) => {
            let backup = backup_user_preset(&conflict_path, backup_dir)?;
            fs::remove_file(&conflict_path).map_err(|e| {
                format!("could not remove the user preset {} after backing it up: {e}", conflict_path.display())
            })?;
            match install_managed_preset_at(path, owner_path, audio) {
                Ok(_) => Ok(Some(backup)),
                Err(err) => {
                    if let Err(restore) = fs::copy(&backup, &conflict_path) {
                        crate::logging::log(&format!("fxsound: could not restore the user preset: {restore}"));
                    }
                    Err(format!("could not recreate the managed preset: {err}"))
                }
            }
        }
    }
}

pub fn presets_folder() -> Result<PathBuf, String> {
    let path = presets_dir();
    fs::create_dir_all(&path).map_err(|e| format!("could not create {}: {e}", path.display()))?;
    Ok(path)
}

/// Create the dedicated Crosshair+ preset once, or verify the existing one is
/// still the file Crosshair+ created. A user-created preset with this name is
/// never overwritten. If FxSound is running and does not hot-reload the new
/// `.fac`, restart it once so `--preset=Crosshair+` can succeed.
pub fn ensure_crosshair_preset(audio: &AudioConfig) -> Result<(), String> {
    ensure_original_backup()?;
    match managed_preset_conflict() {
        Some(PresetConflict::UserFile(path)) => {
            return Err(format!(
                "FxSound already has a preset named Crosshair+ that Crosshair+ did not create. Rename or remove {} before using managed sound.",
                path.display()
            ));
        }
        Some(PresetConflict::StatusOnly) => {
            return Err("FxSound lists a Crosshair+ preset that is not owned by Crosshair+. Rename or remove it before using managed sound.".into());
        }
        None => {}
    }
    let path = managed_preset_path();
    if !path.exists() {
        install_managed_preset_at(&path, &preset_owner_path(), audio)?;
    }
    if running() && !status_has_preset(MANAGED_PRESET) {
        // Do not restart on a stale status file. FxSound normally has the
        // managed preset loaded already; only a confirmed miss is actionable.
        let fresh = refresh_status();
        if preset_restart_needed(&fresh, status_has_preset(MANAGED_PRESET)) {
            restart_for_managed_preset(false)?;
        } else {
            crate::logging::log("fxsound: managed preset was already loaded after status refresh");
        }
    }
    Ok(())
}

fn preset_restart_needed(fresh: &FxSoundStatus, listed: bool) -> bool {
    fresh.running && !listed
}

/// Explicit recovery for a user-owned preset. The backup is verified before
/// the original is removed; if installing the managed file fails, the original
/// is restored. FxSound is restarted once so a stale loaded preset cannot keep
/// the old user curve under the same name.
pub fn repair_managed_preset(audio: &AudioConfig) -> Result<Option<PathBuf>, String> {
    ensure_original_backup()?;
    let path = managed_preset_path();
    let backup = repair_managed_preset_at(
        &path,
        &preset_owner_path(),
        &crate::config::data_dir().join("fxsound-preset-backups"),
        audio,
        status_has_preset(MANAGED_PRESET),
    )?;
    if running() {
        restart_for_managed_preset(true)?;
    }
    Ok(backup)
}

/// Select an existing FxSound preset. The caller must ensure processing is on;
/// FxSound ignores preset selection while power is off.
pub fn select_preset(name: &str) -> Result<(), String> {
    run_args(&[format!("--preset={name}"), "--run_minimized".into()])?;
    hide_fxsound_window();
    Ok(())
}

fn select_managed_preset() -> Result<(), String> {
    let current = refresh_status();
    if current.preset.as_deref() == Some(MANAGED_PRESET) {
        return Ok(());
    }
    if !current.running {
        return Err("FxSound is not running; the Crosshair+ preset cannot be selected.".into());
    }
    if !current.power {
        return Err("FxSound is off; turn sound on before selecting the Crosshair+ preset.".into());
    }
    run_args(&[format!("--preset={MANAGED_PRESET}"), "--run_minimized".into()])?;
    hide_fxsound_window();
    let after = refresh_status();
    if after.preset.as_deref() == Some(MANAGED_PRESET) {
        Ok(())
    } else {
        Err(format!(
            "FxSound did not switch to the Crosshair+ preset (reported {}).",
            after.preset.as_deref().unwrap_or("none")
        ))
    }
}

fn status_preset(json: Option<&Value>) -> Option<String> {
    json.and_then(|value| value.get("selected_preset"))
        .and_then(Value::as_str)
        .map(str::to_string)
}

pub fn find_exe() -> Option<PathBuf> {
    if stub_mode() {
        return std::env::current_exe().ok();
    }
    let mut candidates = Vec::new();
    for var in ["ProgramFiles", "ProgramFiles(x86)"] {
        if let Ok(base) = std::env::var(var) {
            candidates.push(PathBuf::from(&base).join("FxSound").join("FxSound.exe"));
            candidates.push(PathBuf::from(&base).join("FxSound LLC").join("FxSound").join("FxSound.exe"));
            candidates.push(PathBuf::from(&base).join("FxSound Inc").join("FxSound.exe"));
        }
    }
    if let Ok(local) = std::env::var("LOCALAPPDATA") {
        candidates.push(PathBuf::from(&local).join("Programs").join("FxSound").join("FxSound.exe"));
        candidates.push(PathBuf::from(&local).join("FxSound").join("FxSound.exe"));
    }
    if let Ok(appdata) = std::env::var("APPDATA") {
        candidates.push(PathBuf::from(&appdata).join("FxSound").join("FxSound.exe"));
    }
    candidates.into_iter().find(|p| p.exists())
}

fn process_name(pid: u32) -> Option<String> {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
    };
    unsafe {
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0).ok()?;
        let mut entry = PROCESSENTRY32W::default();
        entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
        let mut found = None;
        if Process32FirstW(snapshot, &mut entry).is_ok() {
            loop {
                if entry.th32ProcessID == pid {
                    let end = entry.szExeFile.iter().position(|c| *c == 0).unwrap_or(entry.szExeFile.len());
                    found = Some(String::from_utf16_lossy(&entry.szExeFile[..end]));
                    break;
                }
                if Process32NextW(snapshot, &mut entry).is_err() {
                    break;
                }
            }
        }
        let _ = CloseHandle(snapshot);
        found
    }
}

fn stub_pid_path(dir: &Path) -> PathBuf {
    dir.join("stub.pid")
}
pub(crate) fn stub_pid(dir: &Path) -> Option<u32> {
    fs::read_to_string(stub_pid_path(dir))
        .ok()
        .and_then(|text| text.trim().parse::<u32>().ok())
}
pub(crate) fn process_alive(pid: u32) -> bool {
    process_name(pid).is_some()
}
pub(crate) fn stub_pid_alive(dir: &Path) -> bool {
    stub_pid(dir).map(process_alive).unwrap_or(false)
}
pub(crate) fn append_stub_event(dir: &Path, text: &str) {
    if let Ok(mut file) = fs::OpenOptions::new().create(true).append(true).open(dir.join("events.log")) {
        let _ = writeln!(file, "{text}");
    }
}

pub fn running() -> bool {
    if let Some(dir) = stub_dir() {
        return stub_pid_alive(&dir);
    }
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
    };
    unsafe {
        let Ok(snapshot) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else { return false };
        let mut entry = PROCESSENTRY32W::default();
        entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
        let mut found = false;
        if Process32FirstW(snapshot, &mut entry).is_ok() {
            loop {
                let end = entry.szExeFile.iter().position(|c| *c == 0).unwrap_or(entry.szExeFile.len());
                if String::from_utf16_lossy(&entry.szExeFile[..end]).eq_ignore_ascii_case("FxSound.exe") {
                    found = true;
                    break;
                }
                if Process32NextW(snapshot, &mut entry).is_err() {
                    break;
                }
            }
        }
        let _ = CloseHandle(snapshot);
        found
    }
}

struct WindowSearch {
    visible: Option<windows::Win32::Foundation::HWND>,
    any: Option<windows::Win32::Foundation::HWND>,
}

fn fxsound_window() -> Option<windows::Win32::Foundation::HWND> {
    if stub_mode() {
        return None;
    }
    use windows::core::BOOL;
    use windows::Win32::Foundation::LPARAM;
    use windows::Win32::UI::WindowsAndMessaging::{
        EnumWindows, GetWindowTextLengthW, GetWindowTextW, GetWindowThreadProcessId, IsWindowVisible,
    };
    unsafe extern "system" fn callback(hwnd: windows::Win32::Foundation::HWND, lparam: LPARAM) -> BOOL {
        let search = &mut *(lparam.0 as *mut WindowSearch);
        unsafe {
            let len = GetWindowTextLengthW(hwnd);
            if len == 0 {
                return true.into();
            }
            let mut title = vec![0u16; (len + 1) as usize];
            GetWindowTextW(hwnd, &mut title);
            let title = String::from_utf16_lossy(&title).trim_matches('\0').to_string();
            if !title.to_ascii_lowercase().contains("fxsound") {
                return true.into();
            }
            let mut pid = 0u32;
            GetWindowThreadProcessId(hwnd, Some(&mut pid));
            if pid == 0 {
                return true.into();
            }
            let Some(exe) = process_name(pid) else { return true.into() };
            if !exe.eq_ignore_ascii_case("FxSound.exe") {
                return true.into();
            }
            if search.any.is_none() {
                search.any = Some(hwnd);
            }
            if IsWindowVisible(hwnd).as_bool() {
                search.visible = Some(hwnd);
            }
        }
        true.into()
    }
    let mut search = WindowSearch { visible: None, any: None };
    unsafe {
        let _ = EnumWindows(Some(callback), LPARAM(&mut search as *mut _ as isize));
    }
    search.visible.or(search.any)
}

/// FxSound's cold-start path can leave its main window visible even with
/// `--run_minimized` (its hide step only runs when the window is on the desktop).
/// Automated applies always end with the window hidden.
fn hide_fxsound_window() {
    use windows::Win32::UI::WindowsAndMessaging::{IsWindowVisible, ShowWindow, SW_HIDE};
    if let Some(hwnd) = fxsound_window() {
        unsafe {
            if IsWindowVisible(hwnd).as_bool() {
                let _ = ShowWindow(hwnd, SW_HIDE);
            }
        }
    }
}

fn show_fxsound_window() -> bool {
    if stub_mode() {
        return false;
    }
    use windows::Win32::UI::WindowsAndMessaging::{
        IsIconic, SetForegroundWindow, ShowWindow, SW_RESTORE, SW_SHOW,
    };
    let Some(hwnd) = fxsound_window() else { return false };
    unsafe {
        if IsIconic(hwnd).as_bool() {
            let _ = ShowWindow(hwnd, SW_RESTORE);
        } else {
            let _ = ShowWindow(hwnd, SW_SHOW);
        }
        let _ = SetForegroundWindow(hwnd);
    }
    true
}

pub fn installed_version() -> String {
    if let Ok(text) = fs::read_to_string(settings_path()) {
        if let Some(idx) = text.find("name=\"version\"") {
            let rest = &text[idx..];
            if let Some(val_idx) = rest.find("val=\"") {
                let rest = &rest[val_idx + 5..];
                if let Some(end) = rest.find('"') {
                    return rest[..end].to_string();
                }
            }
        }
    }
    if let Ok(value) = read_status_json() {
        if let Some(v) = value.get("version").and_then(|v| v.as_str()) {
            return v.to_string();
        }
    }
    String::new()
}

fn version_at_least(version: &str, major: u32, minor: u32, patch: u32) -> bool {
    let parts: Vec<u32> = version.split('.').filter_map(|p| p.parse::<u32>().ok()).collect();
    let got = (parts.first().copied().unwrap_or(0), parts.get(1).copied().unwrap_or(0), parts.get(2).copied().unwrap_or(0));
    got >= (major, minor, patch)
}

pub fn version_is_newer(latest: &str, installed: &str) -> bool {
    fn parts(value: &str) -> (u32, u32, u32) {
        let p: Vec<u32> = value.split('.').filter_map(|p| p.parse::<u32>().ok()).collect();
        (p.first().copied().unwrap_or(0), p.get(1).copied().unwrap_or(0), p.get(2).copied().unwrap_or(0))
    }
    parts(latest) > parts(installed)
}

fn status_args() -> Vec<String> {
    ["--run_minimized", "--status"].iter().map(|arg| (*arg).to_string()).collect()
}

/// Ask the running FxSound instance to rewrite `status.json`, then return the
/// parsed status. Automated reads are headless and always hide the window, even
/// when the CLI call fails.
pub fn refresh_status() -> FxSoundStatus {
    if version_at_least(&installed_version(), 1, 2, 11) && running() {
        let before = status_mtime();
        if let Err(err) = run_args_timeout(&status_args(), Duration::from_millis(1500)) {
            crate::logging::log(&format!("fxsound: status refresh failed: {err}"));
        }
        if let Some(before) = before {
            if !wait_for_status_change(&status_path(), before, Duration::from_millis(1200)) {
                crate::logging::log("fxsound: status refresh did not rewrite status.json; using the last readable status");
            }
        }
        hide_fxsound_window();
    }
    status()
}

/// A CLI `--status` call is asynchronous from FxSound's side. Waiting for the
/// file to change keeps startup preset checks from acting on the previous
/// session's JSON snapshot.
fn wait_for_status_change(path: &Path, before: SystemTime, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        let changed = fs::metadata(path).and_then(|meta| meta.modified()).map(|after| after > before).unwrap_or(false);
        if changed && read_status_json_at(path).is_ok() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(40));
    }
}

pub fn status() -> FxSoundStatus {
    let path = find_exe();
    let version = installed_version();
    let json = read_status_json().ok();
    let cli_ready = version_at_least(&version, 1, 2, 11);
    let (power, output) = status_values(json.as_ref());
    let preset = status_preset(json.as_ref());
    let num_bands = band_count(json.as_ref());
    let filter_q = status_float(json.as_ref(), "filter_q").unwrap_or(0.0);
    let running = running();
    let message = if path.is_none() {
        "FxSound is not installed.".into()
    } else if !running {
        "FxSound is installed but not running.".into()
    } else if !cli_ready {
        format!("FxSound {version} is below the 1.2.11 automation requirement.")
    } else {
        "FxSound is ready.".into()
    };
    FxSoundStatus {
        installed: path.is_some(),
        running,
        version,
        cli_ready,
        power,
        preset,
        num_bands,
        output,
        message,
        path: path.map(|p| p.to_string_lossy().to_string()),
        filter_q,
        latest_version: None,
        update_available: false,
        busy: false,
        task: None,
    }
}

fn status_values(json: Option<&Value>) -> (bool, Option<String>) {
    let power = json.and_then(|v| v.get("power")).and_then(|v| v.as_bool()).unwrap_or(false);
    let output = json
        .and_then(|v| {
            // FxSound 1.2.16's --status payload uses `selected_output`; older
            // shapes and fixtures used the other two keys.
            v.get("output_device")
                .or_else(|| v.get("outputDevice"))
                .or_else(|| v.get("selected_output"))
                .or_else(|| v.get("device"))
        })
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    (power, output)
}

fn band_count(json: Option<&Value>) -> usize {
    json.and_then(|v| v.get("equalizer"))
        .and_then(|v| v.get("num_bands"))
        .and_then(|v| v.as_u64())
        .map(|n| n as usize)
        .unwrap_or(0)
}

fn status_float(json: Option<&Value>, key: &str) -> Option<f32> {
    json.and_then(|v| v.get("equalizer"))
        .and_then(|v| v.get(key))
        .and_then(|v| v.as_f64())
        .map(|v| v as f32)
}

pub(crate) fn read_status_json() -> Result<Value, String> {
    read_status_json_at(&status_path())
}

fn read_status_json_at(path: &Path) -> Result<Value, String> {
    let text = fs::read_to_string(path).map_err(|e| e.to_string())?;
    serde_json::from_str::<Value>(&text).map_err(|e| e.to_string())
}

#[derive(Clone, Debug, PartialEq)]
pub struct CliCall {
    pub args: Vec<String>,
}

pub fn build_call(audio: &AudioConfig, current_bands: usize, current_filter_q: f32) -> CliCall {
    let mut args = vec![format!("--power={}", if audio.enabled { 1 } else { 0 })];
    // Sound off must not rewrite the selected preset either. Only the power
    // command is sent while disabled; the next enable applies the saved curve to
    // the managed Crosshair+ preset.
    if audio.enabled {
        let (freqs, gains) = dsp_math::fx_bands(audio);
        // FxSound pins each band's legal frequency range between fixed ISO centers,
        // so its 10-band map cannot be moved down to 31 Hz. Re-sending --num_bands
        // resets the user's preset map even when the count is unchanged; only change
        // it when switching between the 10-band and 20-band parametric layouts.
        // Band gains are applied index-to-index to FxSound's own map.
        if current_bands == 0 || current_bands != freqs.len() {
            args.push(format!("--num_bands={}", freqs.len()));
        }
        // Changing Q re-centers FxSound's bands. Send it only when the requested
        // value differs from the running instance, then immediately re-send gains.
        if current_filter_q <= 0.0 || (audio.filter_q - current_filter_q).abs() > 0.01 {
            args.push(format!("--filter_q={:.1}", audio.filter_q.clamp(1.0, 3.0)));
        }
        args.extend([
            format!(
                "--set_band_gain={}",
                gains.iter().enumerate().map(|(i, g)| format!("{i}:{g:.2}")).collect::<Vec<_>>().join(",")
            ),
            format!("--master_gain={:.2}", dsp_math::master_gain(audio)),
            format!("--volume_leveling={:.1}", audio.volume_leveling.clamp(0.0, 4.0)),
            format!("--balance={:.0}", audio.balance.clamp(-20.0, 20.0)),
            format!(
                "--set_effect=bass:{:.1},clarity:{:.1},ambience:{:.1},surround:{:.1},dynamicboost:{:.1}",
                audio.bass.clamp(0.0, 10.0),
                audio.treble.clamp(0.0, 10.0),
                audio.echo.clamp(0.0, 10.0),
                audio.surround.clamp(0.0, 10.0),
                audio.dynamic_boost.clamp(0.0, 10.0)
            ),
        ]);
        // FxSound's running-instance CLI handler calls showMainWindow() (restore +
        // SetForegroundWindow + HWND_TOP) for every call that lacks this flag, so a
        // single Sound-tab change used to yank its window over Crosshair+. Always
        // drive it headless; "Open FxSound" is the explicit way to show the GUI.
        if let Some(device) = &audio.device {
            if !device.trim().is_empty() {
                args.push(format!("--output={device}"));
            }
        }
    }
    args.push("--run_minimized".into());
    CliCall { args }
}

const CLI_TIMEOUT: Duration = Duration::from_millis(2500);

/// FxSound forwards CLI arguments to an already-running instance and how long
/// that forwarder takes is not under our control. A stuck forwarder previously
/// held the audio worker forever, so every call is bounded and the forwarder is
/// killed on timeout. Killing this child never kills the main FxSound process.
/// Every FxSound child goes through here so stub mode can re-enter this same
/// executable instead of launching the real app.
fn exe_command(exe: &Path) -> Command {
    let mut command = Command::new(exe);
    if stub_mode() {
        command.arg("--fxsound-stub");
    }
    command
}

fn run_args_timeout(args: &[String], timeout: Duration) -> Result<String, String> {
    let exe = find_exe().ok_or_else(|| "FxSound.exe was not found.".to_string())?;
    let mut command = exe_command(&exe);
    let mut child = command
        .args(args)
        .creation_flags(CREATE_NO_WINDOW)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("Could not launch FxSound: {e}"))?;
    let status = wait_child_timeout(&mut child, timeout)?;
    let mut stdout = String::new();
    let mut stderr = String::new();
    if let Some(mut pipe) = child.stdout.take() {
        let _ = pipe.read_to_string(&mut stdout);
    }
    if let Some(mut pipe) = child.stderr.take() {
        let _ = pipe.read_to_string(&mut stderr);
    }
    let stdout = stdout.trim().to_string();
    let stderr = stderr.trim().to_string();
    if !status.success() {
        return Err(format!("FxSound returned {}: {}", status, if stderr.is_empty() { stdout } else { stderr }));
    }
    Ok(stdout)
}

/// Poll a child without blocking forever. On timeout the forwarder is killed
/// and reaped; the main FxSound process is never touched.
fn wait_child_timeout(child: &mut Child, timeout: Duration) -> Result<ExitStatus, String> {
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Ok(status),
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(format!("FxSound did not respond within {} ms.", timeout.as_millis()));
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(err) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("Could not wait for FxSound: {err}"));
            }
        }
    }
}

fn run_args(args: &[String]) -> Result<String, String> {
    run_args_timeout(args, CLI_TIMEOUT)
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ApplyPlan {
    pub start: bool,
    pub effective_device: Option<String>,
    pub fallback_device: Option<String>,
}

fn plan_apply(current: &FxSoundStatus, audio: &AudioConfig, device_present: bool) -> ApplyPlan {
    let fallback_device = if audio.enabled {
        audio
            .device
            .as_deref()
            .map(str::trim)
            .filter(|name| !name.is_empty() && !device_present)
            .map(str::to_string)
    } else {
        None
    };
    let effective_device = if fallback_device.is_some() {
        None
    } else {
        audio.device.clone()
    };
    ApplyPlan {
        start: !current.running,
        effective_device,
        fallback_device,
    }
}

#[derive(Clone, Debug)]
pub struct ApplyReport {
    pub status: FxSoundStatus,
    pub fallback_device: Option<String>,
    pub active_output: Option<String>,
    pub mismatches: Vec<String>,
}

pub fn apply_report(audio: &AudioConfig) -> Result<ApplyReport, String> {
    let mut current = status();
    if !current.installed {
        return Err("FxSound is not installed. Use Set up audio.".into());
    }
    if !current.cli_ready {
        return Err(format!("FxSound {} is too old for CLI automation. Install 1.2.11 or newer.", current.version));
    }
    if audio.enabled {
        // Create the file before starting FxSound when possible, so the normal
        // startup path sees it and no restart is needed.
        ensure_crosshair_preset(audio)?;
    }
    // FxSound's first-instance startup path only accepts a subset of the CLI
    // options; band gains and effects are only handled by the running-instance
    // path. Start it first, wait for the process and status channel, then send
    // the complete call. Sound-off starts FxSound with power=0 but deliberately
    // sends no curve or output values, so the selected user preset stays intact.
    let requested_present = audio
        .device
        .as_deref()
        .map(|device| output_device_present(device.trim()))
        .unwrap_or(true);
    let plan = plan_apply(&current, audio, requested_present);
    if plan.start {
        let status_before = status_mtime();
        start_fxsound(audio.enabled)?;
        if !wait_until_running(Duration::from_secs(4)) {
            return Err("FxSound did not start.".into());
        }
        if !wait_until_status_ready(status_before, Duration::from_secs(2)) {
            // The process is running; the status file is only a convenience.
            // Send the complete call with FxSound's default 10-band map rather
            // than dropping every slider value on the floor.
            crate::logging::log("fxsound: status channel not ready after start; applying with 10-band fallback");
        }
        current = refresh_status();
    }
    if audio.enabled {
        // Crosshair+ owns this preset, so no band or effect write can touch a
        // user preset. The first ensure above created the file before FxSound
        // started; refresh once here instead of running the same preset check
        // twice on every launch.
        current = refresh_status();
        if !current.power {
            run_args(&["--power=1".into(), "--run_minimized".into()])?;
            hide_fxsound_window();
        }
        select_managed_preset()?;
        current = refresh_status();
    }
    let current_bands = if current.num_bands == 0 { 10 } else { current.num_bands };
    let mut effective = audio.clone();
    if let Some(device) = &plan.fallback_device {
        crate::logging::log(&format!(
            "fxsound: output device '{device}' is not present; keeping current output"
        ));
    }
    effective.device = plan.effective_device.clone();
    let call = build_call(&effective, current_bands, current.filter_q);
    let applied = run_args(&call.args);
    std::thread::sleep(Duration::from_millis(140));
    // Hide even when the call failed: FxSound may already have raised its
    // window while parsing the arguments, and the next apply would inherit it.
    hide_fxsound_window();
    applied?;
    // The status command is asynchronous, like every other CLI call. Wait for
    // the file to change before comparing, otherwise a stale status would
    // produce a false "FxSound ignored" banner after a perfectly good apply.
    let status = refresh_status();
    let mismatches = read_status_json()
        .map(|actual| verify::mismatch_messages(&effective, &actual))
        .unwrap_or_default();
    if !mismatches.is_empty() {
        crate::logging::log(&format!(
            "fxsound: apply mismatch after status refresh: {}",
            mismatches.join("; ")
        ));
    }
    Ok(ApplyReport {
        fallback_device: plan.fallback_device,
        active_output: status.output.clone(),
        status,
        mismatches,
    })
}

/// Stub mode uses deterministic endpoints so the harness never depends on the
/// machine's real audio hardware; every other mode keeps the live WASAPI list.
fn available_devices() -> Vec<AudioDevice> {
    if stub_mode() {
        return vec![
            AudioDevice { id: "stub-speakers".into(), name: "Stub Speakers".into(), is_default: true },
            AudioDevice { id: "stub-headphones".into(), name: "Stub Headphones".into(), is_default: false },
        ];
    }
    AudioRuntime::devices()
}

fn output_device_present(name: &str) -> bool {
    available_devices()
        .iter()
        .any(|device| device.name.eq_ignore_ascii_case(name))
}

fn wait_until_running(timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if running() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(80));
    }
}

fn wait_until_stopped(timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if !running() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(80));
    }
}

fn is_fxsound_output(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    name.contains("fxsound audio enhancer") || name.contains("fxsound speakers") || name.starts_with("fxsound")
}

/// Pick a real playback endpoint for the unstick restart. A saved name wins
/// only while that endpoint still exists; otherwise the Windows default, then
/// the first physical endpoint, is used. FxSound's own virtual/enhancer output
/// is never a valid target for its restart.
fn choose_output(devices: &[AudioDevice], saved: Option<&str>) -> Option<AudioDevice> {
    let physical: Vec<&AudioDevice> = devices.iter().filter(|device| !is_fxsound_output(&device.name)).collect();
    if let Some(saved) = saved.map(str::trim).filter(|name| !name.is_empty()) {
        if let Some(device) = physical.iter().find(|device| device.name.eq_ignore_ascii_case(saved)) {
            return Some((*device).clone());
        }
    }
    if let Some(device) = physical.iter().find(|device| device.is_default) {
        return Some((*device).clone());
    }
    physical.first().map(|device| (*device).clone())
}

fn start_fxsound_minimized() -> Result<(), String> {
    let exe = find_exe().ok_or_else(|| "FxSound.exe was not found.".to_string())?;
    exe_command(&exe)
        .args(["--run_minimized"])
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .map_err(|e| format!("Could not start FxSound: {e}"))?;
    Ok(())
}

/// Manually recover a wedged FxSound output: stop it, let Windows release the
/// stale endpoint, choose a physical output, start it headless and replay the
/// current EQ/output/power through the normal CLI builder. This is deliberately
/// only reachable from the explicit Unstick button; it is not wake automation.
pub fn unstick(audio: &AudioConfig) -> Result<FxSoundStatus, String> {
    let current = status();
    if !current.installed {
        return Err("FxSound is not installed. Use Set up audio.".into());
    }
    if !current.cli_ready {
        return Err(format!("FxSound {} is too old for CLI automation. Install 1.2.11 or newer.", current.version));
    }
    ensure_original_backup()?;

    crate::logging::log("fxsound: unstick stage=power-off");
    if let Err(err) = run_args(&["--power=0".into(), "--run_minimized".into()]) {
        // The stop below is authoritative; a failed power-off call must not
        // prevent the actual restart.
        crate::logging::log(&format!("fxsound: unstick power-off call failed: {err}"));
    }

    crate::logging::log("fxsound: unstick stage=stop");
    stop_fxsound();
    if !wait_until_stopped(Duration::from_secs(4)) {
        return Err("FxSound did not stop; close it manually and try again.".into());
    }

    crate::logging::log("fxsound: unstick stage=devices");
    let devices = available_devices();
    let chosen = choose_output(&devices, audio.device.as_deref())
        .ok_or_else(|| "No physical playback device is available to reconnect FxSound.".to_string())?;
    crate::logging::log(&format!("fxsound: unstick output='{}'", chosen.name));

    crate::logging::log("fxsound: unstick stage=start");
    start_fxsound_minimized()?;
    if !wait_until_running(Duration::from_secs(5)) {
        return Err("FxSound did not start after the restart.".into());
    }
    let before_status = status_mtime();
    if !wait_until_status_ready(before_status, Duration::from_secs(3)) {
        crate::logging::log("fxsound: unstick status channel not ready; applying with fallback bands");
    }
    let mut after_start = status();
    if audio.enabled {
        ensure_crosshair_preset(audio)?;
        after_start = status();
        if !after_start.power {
            run_args(&["--power=1".into(), "--run_minimized".into()])?;
            hide_fxsound_window();
        }
        select_managed_preset()?;
        after_start = status();
    }
    let bands = if after_start.num_bands == 0 { 10 } else { after_start.num_bands };
    let mut effective = audio.clone();
    effective.device = Some(chosen.name.clone());
    let call = build_call(&effective, bands, after_start.filter_q);

    crate::logging::log("fxsound: unstick stage=apply");
    let applied = run_args(&call.args);
    std::thread::sleep(Duration::from_millis(140));
    hide_fxsound_window();
    applied?;
    let after = refresh_status();

    crate::logging::log("fxsound: unstick stage=verify");
    verify_unstick_result(&after, &chosen.name, audio.enabled)?;
    crate::logging::log(&format!("fxsound: unstick recovered on '{}'", chosen.name));
    Ok(after)
}

/// Verify the manual recovery without treating the deliberate Sound-off path
/// as a failure. Sound off sends only `--power=0 --run_minimized`, so its old
/// output name is expected to remain; Sound on sends the chosen output and must
/// report it back. Power must match in both cases.
fn verify_unstick_result(after: &FxSoundStatus, chosen: &str, enabled: bool) -> Result<(), String> {
    if !after.running {
        return Err("FxSound stopped again while restoring the settings.".into());
    }
    if after.power != enabled {
        return Err(format!(
            "FxSound reported power={} instead of {}.",
            after.power, enabled
        ));
    }
    if enabled {
        let output_matches = after
            .output
            .as_deref()
            .map(|output| output.eq_ignore_ascii_case(chosen))
            .unwrap_or(false);
        if !output_matches {
            return Err(format!(
                "FxSound reported output '{}' instead of '{}'.",
                after.output.as_deref().unwrap_or("none"),
                chosen
            ));
        }
    }
    Ok(())
}

fn status_mtime() -> Option<SystemTime> {
    fs::metadata(status_path()).and_then(|meta| meta.modified()).ok()
}

fn wait_until_status_ready(before: Option<SystemTime>, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        let fresh = status_mtime().map(|after| before.map(|before| after > before).unwrap_or(true)).unwrap_or(false);
        if fresh && read_status_json().is_ok() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        let _ = run_args_timeout(&status_args(), Duration::from_millis(1200));
        std::thread::sleep(Duration::from_millis(120));
    }
}

fn start_fxsound(power: bool) -> Result<(), String> {
    let exe = find_exe().ok_or_else(|| "FxSound.exe was not found.".to_string())?;
    exe_command(&exe)
        .args(["--run_minimized", if power { "--power=1" } else { "--power=0" }])
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .map_err(|e| format!("Could not start FxSound: {e}"))?;
    Ok(())
}

/// Start FxSound if necessary and bring its window forward. This is called on
/// the audio worker, never on the UI thread; all waits are bounded.
pub fn open_fxsound() -> Result<(), String> {
    let exe = find_exe().ok_or_else(|| "FxSound.exe was not found.".to_string())?;
    if !running() {
        // Launch only. Power is the Sound tab's job; starting the GUI must not
        // silently unmute a user who intentionally has FxSound off.
        exe_command(&exe)
            .args(["--run_minimized"])
            .creation_flags(CREATE_NO_WINDOW)
            .spawn()
            .map_err(|e| format!("Could not start FxSound: {e}"))?;
        if !wait_until_running(Duration::from_secs(4)) {
            return Err("FxSound did not start.".into());
        }
    }
    let _ = show_fxsound_window();
    Ok(())
}

/// Run a helper process with a hard deadline. `Command::output` has no
/// timeout, and a stuck winget or PowerShell otherwise parks the audio worker
/// (and the installer modal) forever.
fn command_output_bounded(command: &mut Command, timeout: Duration) -> Result<std::process::Output, String> {
    use std::process::Stdio;
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = command.spawn().map_err(|e| e.to_string())?;
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return child.wait_with_output().map_err(|e| e.to_string()),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(100)),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("timed out after {}s", timeout.as_secs()));
            }
            Err(e) => {
                let _ = child.kill();
                return Err(e.to_string());
            }
        }
    }
}

fn run_winget(args: &[&str]) -> Result<(), String> {
    let mut command = Command::new("winget");
    command.args(args).creation_flags(CREATE_NO_WINDOW);
    let output = command_output_bounded(&mut command, Duration::from_secs(600))
        .map_err(|e| format!("winget is unavailable: {e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let detail = if stderr.trim().is_empty() { stdout.trim() } else { stderr.trim() };
        return Err(format!("winget {} failed: {}", args.first().copied().unwrap_or("command"), detail));
    }
    Ok(())
}

pub fn winget_available() -> bool {
    let mut command = Command::new("winget");
    command.arg("--version").creation_flags(CREATE_NO_WINDOW);
    command_output_bounded(&mut command, Duration::from_secs(15))
        .map(|output| output.status.success())
        .unwrap_or(false)
}

fn winget_upgrade_args() -> [&'static str; 8] {
    [
        "upgrade",
        "--id",
        "FxSound.FxSound",
        "-e",
        "--silent",
        "--accept-source-agreements",
        "--accept-package-agreements",
        "--disable-interactivity",
    ]
}

fn winget_install_args() -> [&'static str; 8] {
    [
        "install",
        "--id",
        "FxSound.FxSound",
        "-e",
        "--silent",
        "--accept-source-agreements",
        "--accept-package-agreements",
        "--disable-interactivity",
    ]
}

fn winget_uninstall_args() -> [&'static str; 7] {
    [
        "uninstall",
        "--id",
        "FxSound.FxSound",
        "-e",
        "--silent",
        "--accept-source-agreements",
        "--disable-interactivity",
    ]
}

fn install_verified() -> bool {
    find_exe().is_some() && version_at_least(&installed_version(), 1, 2, 11)
}

fn install_with_winget() -> Result<(), String> {
    let installed = find_exe().is_some();
    if installed && run_winget(&winget_upgrade_args()).is_ok() && install_verified() {
        return Ok(());
    }
    run_winget(&winget_install_args()).map_err(|install| {
        if installed {
            format!("upgrade did not produce a current install; install failed ({install})")
        } else {
            install
        }
    })?;
    if !install_verified() {
        return Err("winget finished but FxSound 1.2.11+ was not detected".into());
    }
    Ok(())
}

pub fn installer_args() -> [&'static str; 3] {
    ["/exenoui", "/quiet", "/norestart"]
}

#[derive(Clone, Debug, PartialEq)]
struct InstallerInfo {
    url: String,
    digest: Option<String>,
}

fn installer_info_from_release(value: &Value) -> Option<InstallerInfo> {
    let assets = value.get("assets")?.as_array()?;
    let asset = assets.iter().find(|asset| asset.get("name").and_then(Value::as_str) == Some("fxsound_setup.exe"))?;
    let url = asset.get("browser_download_url")?.as_str()?.to_string();
    let digest = asset
        .get("digest")
        .and_then(Value::as_str)
        .and_then(|value| value.strip_prefix("sha256:"))
        .map(|value| value.to_ascii_lowercase());
    Some(InstallerInfo { url, digest })
}

fn version_from_release(value: &Value) -> Option<String> {
    fn numeric_token(value: &str) -> Option<String> {
        for token in value.split(|ch: char| !(ch.is_ascii_digit() || ch == '.')) {
            if token.matches('.').count() >= 2 && token.chars().all(|ch| ch.is_ascii_digit() || ch == '.') {
                return Some(token.trim_matches('.').to_string());
            }
        }
        None
    }
    value
        .get("name")
        .and_then(Value::as_str)
        .and_then(numeric_token)
        .or_else(|| value.get("tag_name").and_then(Value::as_str).and_then(numeric_token))
}

fn http_text(url: &str) -> Result<String, String> {
    let mut curl = Command::new("curl.exe");
    curl.args(["-fsSL", "--max-time", "20", url]).creation_flags(CREATE_NO_WINDOW);
    if let Ok(output) = command_output_bounded(&mut curl, Duration::from_secs(30)) {
        if output.status.success() && !output.stdout.is_empty() {
            return Ok(String::from_utf8_lossy(&output.stdout).into_owned());
        }
    }
    let escaped = url.replace('\'', "''");
    let script = format!("(Invoke-WebRequest -UseBasicParsing -Uri '{escaped}' -TimeoutSec 20).Content");
    let mut command = Command::new("powershell.exe");
    command
        .args(["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-Command", &script])
        .creation_flags(CREATE_NO_WINDOW);
    let output = command_output_bounded(&mut command, Duration::from_secs(30))
        .map_err(|e| format!("HTTP unavailable: {e}"))?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_string());
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn release_json() -> Result<Value, String> {
    serde_json::from_str::<Value>(&http_text(RELEASE_API)?).map_err(|e| e.to_string())
}

pub fn latest_release_version() -> Option<String> {
    release_json().ok().and_then(|value| version_from_release(&value))
}

fn sha256_file(path: &Path) -> Option<String> {
    let mut command = Command::new("certutil");
    command.args(["-hashfile", &path.to_string_lossy(), "SHA256"]).creation_flags(CREATE_NO_WINDOW);
    let output = command_output_bounded(&mut command, Duration::from_secs(30)).ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    text.lines()
        .map(str::trim)
        .find(|line| line.len() == 64 && line.chars().all(|ch| ch.is_ascii_hexdigit()))
        .map(|line| line.to_ascii_lowercase())
}

fn download_file(url: &str, path: &Path) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let path_text = path.to_string_lossy().to_string();
    let mut curl = Command::new("curl.exe");
    curl.args(["-fL", "--retry", "2", "--max-time", "300", "-o", &path_text, url])
        .creation_flags(CREATE_NO_WINDOW);
    if let Ok(output) = command_output_bounded(&mut curl, Duration::from_secs(330)) {
        if output.status.success() && path.exists() {
            return Ok(());
        }
    }
    let escaped_url = url.replace('\'', "''");
    let escaped_path = path_text.replace('\'', "''");
    let script = format!(
        "$ProgressPreference='SilentlyContinue'; Invoke-WebRequest -UseBasicParsing -Uri '{escaped_url}' -OutFile '{escaped_path}' -TimeoutSec 300"
    );
    let mut command = Command::new("powershell.exe");
    command
        .args(["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-Command", &script])
        .creation_flags(CREATE_NO_WINDOW);
    let output = command_output_bounded(&mut command, Duration::from_secs(330))
        .map_err(|e| format!("download failed: {e}"))?;
    if !output.status.success() || !path.exists() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_string());
    }
    Ok(())
}

fn download_installer() -> Result<PathBuf, String> {
    let info = installer_info_from_release(&release_json()?).ok_or("The official installer link was not found.")?;
    let dir = crate::config::data_dir().join("fxsound-installer");
    let path = dir.join("fxsound_setup.exe");
    if path.exists() {
        if let Some(expected) = &info.digest {
            if sha256_file(&path).as_deref() == Some(expected.as_str()) {
                return Ok(path);
            }
        } else if fs::metadata(&path).map(|m| m.len() > 1_000_000).unwrap_or(false) {
            return Ok(path);
        }
    }
    let _ = fs::remove_file(&path);
    download_file(&info.url, &path)?;
    if let Some(expected) = info.digest {
        let actual = sha256_file(&path).ok_or("Could not verify the downloaded installer.")?;
        if actual != expected {
            let _ = fs::remove_file(&path);
            return Err("The downloaded installer failed its signature check.".into());
        }
    }
    Ok(path)
}

fn powershell_quote(value: &str) -> String {
    value.replace('\'', "''")
}

fn run_installer_with(path: &Path, args: &[&str]) -> Result<(), String> {
    let arguments = args.iter().map(|arg| format!("'{}'", powershell_quote(arg))).collect::<Vec<_>>().join(",");
    let script = format!(
        "$p = Start-Process -FilePath '{}' -ArgumentList {} -Verb RunAs -Wait -PassThru; exit $p.ExitCode",
        powershell_quote(&path.to_string_lossy()),
        arguments
    );
    let mut command = Command::new("powershell.exe");
    command
        .args(["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-Command", &script])
        .creation_flags(CREATE_NO_WINDOW);
    let output = command_output_bounded(&mut command, Duration::from_secs(900))
        .map_err(|e| format!("The installer could not be started: {e}"))?;
    if output.status.success() {
        return Ok(());
    }
    // Fallback for systems where the PowerShell elevation shim is unavailable.
    if args == installer_args() {
        let mut installer = Command::new(path);
        installer.args(args).creation_flags(CREATE_NO_WINDOW);
        let status = command_output_bounded(&mut installer, Duration::from_secs(900))
            .map_err(|e| format!("The installer could not be started: {e}"))?
            .status;
        if status.success() {
            return Ok(());
        }
    }
    Err(String::from_utf8_lossy(&output.stderr).trim().to_string())
}

fn install_fallback() -> Result<(), String> {
    let path = download_installer()?;
    run_installer_with(&path, &installer_args())?;
    if !install_verified() {
        return Err("The installer finished but FxSound 1.2.11+ was not detected.".into());
    }
    Ok(())
}

fn install_latest() -> Result<(), String> {
    if winget_available() {
        match install_with_winget() {
            Ok(()) => return Ok(()),
            Err(err) => crate::logging::log(&format!("fxsound: winget install path failed: {err}")),
        }
    }
    install_fallback()
}

fn stop_fxsound() {
    if let Some(dir) = stub_dir() {
        // Only the stamped stub PID is killed; the real FxSound process is
        // never addressed by name in this mode.
        if let Some(pid) = stub_pid(&dir) {
            append_stub_event(&dir, &format!("stop pid={pid}"));
            if let Err(err) = Command::new("taskkill")
                .args(["/PID", &pid.to_string(), "/F"])
                .creation_flags(CREATE_NO_WINDOW)
                .output()
            {
                crate::logging::log(&format!("fxsound stub: could not stop pid {pid}: {err}"));
            }
            let _ = fs::remove_file(stub_pid_path(&dir));
        } else {
            append_stub_event(&dir, "stop missing-pid");
        }
        std::thread::sleep(Duration::from_millis(150));
        return;
    }
    if let Err(err) = Command::new("taskkill").args(["/IM", "FxSound.exe", "/F"]).creation_flags(CREATE_NO_WINDOW).output() {
        crate::logging::log(&format!("fxsound: could not stop the running app: {err}"));
    }
    std::thread::sleep(Duration::from_millis(400));
}

/// Cleanup hook for the sandboxed harness; a no-op outside stub mode.
pub(crate) fn stop_stub() {
    if stub_mode() {
        stop_fxsound();
    }
}

fn uninstall_fxsound() -> Result<(), String> {
    if winget_available() && run_winget(&winget_uninstall_args()).is_ok() {
        return Ok(());
    }
    let path = download_installer()?;
    run_installer_with(&path, &["/x", "//", "/qn"])
}

pub fn setup(proxy: &EventLoopProxy<AppEvent>) -> Result<(), String> {
    backup_settings().map_err(|err| format!("Could not back up FxSound settings; installation aborted: {err}"))?;
    install_latest()?;
    start_fxsound(true).map_err(|err| format!("FxSound was installed but could not be started: {err}"))?;
    let status = status();
    let _ = proxy.send_event(AppEvent::FxSound(status));
    Ok(())
}

/// Uninstall and reinstall FxSound, keeping the user's saved settings. Each
/// step reports a stage so the caller can say what the reinstall is doing;
/// installing covers the winget/download path, which fetches its own package.
pub fn repair_with(mut stage: impl FnMut(&str)) -> Result<PathBuf, String> {
    stage("Stopping FxSound…");
    stop_fxsound();
    stage("Backing up settings…");
    let snapshot = backup_settings_fresh()
        .map_err(|err| format!("Could not back up FxSound settings; reinstall aborted: {err}"))?;
    stage("Uninstalling FxSound…");
    if let Err(err) = uninstall_fxsound() {
        crate::logging::log(&format!("fxsound: uninstall during repair failed, reinstalling over it: {err}"));
    }
    std::thread::sleep(Duration::from_millis(500));
    stage("Installing FxSound…");
    install_latest()?;
    stage("Restoring settings…");
    restore_backup_from(&snapshot);
    stage("Starting FxSound…");
    start_fxsound(true).map_err(|err| format!("FxSound was reinstalled but could not be started: {err}"))?;
    Ok(snapshot)
}

/// Settings keys whose value changed across the reinstall, so a restore that
/// misses something is named in the probe report instead of only flagged.
fn changed_setting_keys(before: Option<&[u8]>, after: Option<&[u8]>) -> Vec<String> {
    fn pairs(bytes: &[u8]) -> Vec<(String, String)> {
        String::from_utf8_lossy(bytes)
            .lines()
            .filter_map(|line| {
                let rest = line.trim().strip_prefix("<VALUE name=\"")?;
                let (key, rest) = rest.split_once('"')?;
                let rest = rest.trim_start().strip_prefix("val=\"")?;
                let (value, _) = rest.split_once('"')?;
                Some((key.to_string(), value.to_string()))
            })
            .collect()
    }
    let (Some(before), Some(after)) = (before, after) else { return Vec::new() };
    let (before, after) = (pairs(before), pairs(after));
    let mut changed: Vec<String> = Vec::new();
    for (key, value) in &before {
        if after.iter().any(|(other, now)| other == key && now == value) || changed.contains(key) {
            continue;
        }
        changed.push(key.clone());
    }
    for (key, _) in &after {
        if !before.iter().any(|(other, _)| other == key) && !changed.contains(key) {
            changed.push(key.clone());
        }
    }
    changed
}

/// `--repair-fxsound`: run the real reinstall once and report what happened.
/// Destructive by design, so it is only reachable through that explicit flag.
pub fn repair_probe() -> serde_json::Value {
    const STAGES: [&str; 6] = [
        "Stopping FxSound…",
        "Backing up settings…",
        "Uninstalling FxSound…",
        "Installing FxSound…",
        "Restoring settings…",
        "Starting FxSound…",
    ];
    let settings = settings_path();
    let before = fs::read(&settings).ok();
    let version_before = installed_version();
    let exe_before = find_exe().map(|path| path.to_string_lossy().to_string());
    let mut stages: Vec<serde_json::Value> = Vec::new();
    let mut last = std::time::Instant::now();
    let original_backup = crate::config::data_dir().join("fxsound-backup").join("FxSound.settings");
    let original_before = fs::read(&original_backup).ok();
    let result = repair_with(|text| {
        let now = std::time::Instant::now();
        let elapsed = now.duration_since(last).as_millis() as u64;
        crate::logging::log(&format!("fxrepair: {text} ({elapsed}ms)"));
        stages.push(serde_json::json!({ "stage": text, "ms": elapsed }));
        last = now;
    });
    let after = fs::read(&settings).ok();
    let snapshot = result.as_ref().ok().cloned();
    let backup_bytes = snapshot
        .as_ref()
        .and_then(|path| fs::read(path.join("FxSound.settings")).ok());
    let original_after = fs::read(&original_backup).ok();
    let exe_after = find_exe().map(|path| path.to_string_lossy().to_string());
    let version_after = installed_version();
    let seen: Vec<String> = stages.iter().filter_map(|entry| entry.get("stage").and_then(|v| v.as_str()).map(str::to_string)).collect();
    let all_stages = STAGES.iter().all(|want| seen.iter().any(|got| got == want));
    serde_json::json!({
        "ok": result.is_ok(),
        "error": result.as_ref().err().cloned(),
        "versionBefore": version_before,
        "versionAfter": version_after,
        "exeBefore": exe_before,
        "exeAfter": exe_after,
        "runningAfter": running(),
        "settingsSameBytes": before.is_some() && before == after,
        "settingsBackupMatches": before.is_some() && before.as_ref() == backup_bytes.as_ref(),
        "reinstallSnapshot": snapshot.as_ref().map(|path| path.to_string_lossy().to_string()),
        "originalBackupPreserved": original_before == original_after,
        "changedSettings": changed_setting_keys(before.as_deref(), after.as_deref()),
        "allStages": all_stages,
        "stages": stages,
    })
}

fn copy_dir_all(src: &Path, dest: &Path) -> Result<(), String> {
    fs::create_dir_all(dest).map_err(|e| e.to_string())?;
    for entry in fs::read_dir(src).map_err(|e| e.to_string())? {
        let entry = entry.map_err(|e| e.to_string())?;
        let target = dest.join(entry.file_name());
        if entry.path().is_dir() {
            copy_dir_all(&entry.path(), &target)?;
        } else {
            fs::copy(entry.path(), &target).map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}

fn backup_settings_to(src: &Path, dest: &Path) -> Result<(), String> {
    if !src.exists() {
        return Ok(());
    }
    if dest.exists() {
        return Ok(());
    }
    copy_dir_all(src, dest)
}

fn backup_settings() -> Result<(), String> {
    let src = settings_path().parent().map(|p| p.to_path_buf()).ok_or("No FxSound data folder.")?;
    let dest = crate::config::data_dir().join("fxsound-backup");
    backup_settings_to(&src, &dest)
}

fn reinstall_backup_name(stamp: u128, suffix: u32) -> String {
    if suffix == 0 {
        format!("fxsound-reinstall-backup-{stamp}")
    } else {
        format!("fxsound-reinstall-backup-{stamp}-{suffix}")
    }
}

/// A reinstall needs a snapshot of the state as it is right now, but it must
/// never replace the one-time original backup. A timestamped sibling keeps
/// both; the numeric suffix only avoids a same-millisecond collision.
fn backup_settings_fresh() -> Result<PathBuf, String> {
    let src = settings_path().parent().map(|p| p.to_path_buf()).ok_or("No FxSound data folder.")?;
    if !src.exists() {
        return Err("FxSound's data folder does not exist.".into());
    }
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let base = crate::config::data_dir();
    let mut dest = base.join(reinstall_backup_name(stamp, 0));
    let mut suffix = 1u32;
    while dest.exists() {
        dest = base.join(reinstall_backup_name(stamp, suffix));
        suffix += 1;
    }
    copy_dir_all(&src, &dest)?;
    crate::logging::log(&format!("fxsound: reinstall snapshot written to {}", dest.display()));
    Ok(dest)
}

fn restore_backup_from(backup: &Path) {
    let Some(src) = settings_path().parent().map(|p| p.to_path_buf()) else { return };
    if !backup.exists() {
        return;
    }
    if let Err(err) = copy_dir_all(backup, &src) {
        crate::logging::log(&format!("fxsound: could not restore the settings backup: {err}"));
    }
}

pub fn open_url(url: &str) {
    use windows::core::{HSTRING, PCWSTR};
    use windows::Win32::UI::Shell::ShellExecuteW;
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    let url = HSTRING::from(url);
    let verb = HSTRING::from("open");
    let result = unsafe {
        ShellExecuteW(None, PCWSTR(verb.as_ptr()), PCWSTR(url.as_ptr()), PCWSTR::null(), PCWSTR::null(), SW_SHOWNORMAL)
    };
    if result.0 as isize <= 32 {
        crate::logging::log(&format!("fxsound: could not open the download page: ShellExecuteW returned {}", result.0 as isize));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_builder_uses_real_fxsound_options() {
        let mut audio = AudioConfig {
            enabled: true,
            device: Some("Speakers".into()),
            output_level: -2.0,
            echo: 7.0,
            bass: 4.0,
            treble: 5.0,
            surround: 3.0,
            dynamic_boost: 2.0,
            volume_leveling: 1.5,
            balance: -4.0,
            filter_q: 2.0,
            volume_guard: true,
            ..Default::default()
        };
        audio.bands[0] = 6.0;
        let call = build_call(&audio, 10, 1.0);
        assert!(call.args.iter().any(|a| a == "--power=1"));
        assert!(!call.args.iter().any(|a| a.starts_with("--num_bands=")));
        assert!(call.args.iter().any(|a| a == "--filter_q=2.0"));
        assert!(call.args.iter().any(|a| a.starts_with("--set_band_gain=0:")));
        assert!(call.args.iter().any(|a| a.starts_with("--master_gain=")));
        assert!(call.args.iter().any(|a| a == "--volume_leveling=1.5"));
        assert!(call.args.iter().any(|a| a == "--balance=-4"));
        assert!(call.args.iter().any(|a| a == "--set_effect=bass:4.0,clarity:5.0,ambience:7.0,surround:3.0,dynamicboost:2.0"));
        assert!(call.args.iter().any(|a| a == "--output=Speakers"));
        assert!(call.args.iter().any(|a| a == "--run_minimized"));
        assert!(!call.args.iter().any(|a| a.starts_with("--set_band_freq=")));
        assert!(build_call(&audio, 20, 2.0).args.iter().any(|a| a == "--num_bands=10"));
        assert!(!build_call(&audio, 10, 2.0).args.iter().any(|a| a.starts_with("--filter_q=")));
        // Every automated call must stay headless, or FxSound raises its window.
        assert!(build_call(&audio, 10, 1.0).args.iter().any(|a| a == "--run_minimized"));
        assert!(build_call(&audio, 20, 2.0).args.iter().any(|a| a == "--run_minimized"));
        audio.enabled = false;
        let disabled = build_call(&audio, 10, 0.0);
        assert_eq!(disabled.args, vec!["--power=0".to_string(), "--run_minimized".to_string()]);
        assert!(!disabled.args.iter().any(|a| a.starts_with("--set_band_gain=")), "sound off must not rewrite a preset");
        assert!(!disabled.args.iter().any(|a| a.starts_with("--set_effect=")), "sound off must not rewrite effects");
    }

    #[test]
    fn compare_payload_is_flat_but_keeps_fxsound_powered_on() {
        let audio = AudioConfig {
            enabled: true,
            device: Some("Headphones".into()),
            bands: [6.0; 10],
            bass: 7.0,
            treble: 8.0,
            output_level: 5.0,
            echo: 6.0,
            surround: 4.0,
            dynamic_boost: 3.0,
            volume_leveling: 2.0,
            balance: -6.0,
            filter_q: 1.5,
            volume_guard: true,
            imported: Some(crate::config::ImportedProfile {
                name: "Imported".into(),
                raw: String::new(),
                bands: Vec::new(),
                parametric: false,
                preamp: 4.0,
            }),
            active_preset: Some("harman".into()),
        };
        let flat = crate::audio::dsp_math::flat_compare(&audio);
        assert!(flat.enabled, "compare keeps the endpoint powered");
        assert_eq!(flat.device.as_deref(), Some("Headphones"));
        assert_eq!(flat.filter_q, 1.5, "keep Q so the bands are not re-centered");
        assert!(flat.imported.is_none(), "the imported curve is part of what compare bypasses");

        let call = build_call(&flat, 10, 1.5);
        assert!(call.args.iter().any(|arg| arg == "--power=1"));
        assert!(!call.args.iter().any(|arg| arg == "--power=0"), "compare must never power the endpoint off");
        assert_eq!(
            call.args.iter().find(|arg| arg.starts_with("--set_band_gain=")).map(String::as_str),
            Some("--set_band_gain=0:0.00,1:0.00,2:0.00,3:0.00,4:0.00,5:0.00,6:0.00,7:0.00,8:0.00,9:0.00")
        );
        assert!(call.args.iter().any(|arg| arg == "--master_gain=0.00"));
        assert!(call.args.iter().any(|arg| arg == "--volume_leveling=0.0"));
        assert!(call.args.iter().any(|arg| arg == "--balance=0"));
        assert!(call
            .args
            .iter()
            .any(|arg| arg == "--set_effect=bass:0.0,clarity:0.0,ambience:0.0,surround:0.0,dynamicboost:0.0"));
        assert!(!call.args.iter().any(|arg| arg.starts_with("--filter_q=")));
        assert!(call.args.iter().any(|arg| arg == "--run_minimized"));
    }

    #[test]
    fn master_gain_clamps_before_reaching_fxsound() {
        let mut audio = AudioConfig { enabled: true, output_level: -12.0, volume_guard: true, ..Default::default() };
        audio.bands = [12.0; 10];
        let call = build_call(&audio, 10, 1.0);
        let gain = call.args.iter().find(|a| a.starts_with("--master_gain=")).unwrap();
        assert_eq!(gain, "--master_gain=-20.00");
    }

    #[test]
    fn status_payload_reads_power_output_and_filter_q() {
        let value = serde_json::json!({
            "power": true,
            "output_device": "Speakers",
            "equalizer": { "filter_q": 2.5, "num_bands": 10 }
        });
        let (power, output) = status_values(Some(&value));
        assert!(power);
        assert_eq!(output.as_deref(), Some("Speakers"));
        assert_eq!(status_float(Some(&value), "filter_q"), Some(2.5));
        assert_eq!(band_count(Some(&value)), 10);
        let (power, output) = status_values(None);
        assert!(!power);
        assert!(output.is_none());
        assert_eq!(status_preset(Some(&value)), None);
        let named = serde_json::json!({ "selected_preset": "Trap" });
        assert_eq!(status_preset(Some(&named)).as_deref(), Some("Trap"));
    }

    #[test]
    fn status_refresh_is_headless() {
        assert_eq!(status_args(), vec!["--run_minimized".to_string(), "--status".to_string()]);
    }

    #[test]
    fn status_wait_observes_a_delayed_rewrite() {
        let root = std::env::temp_dir().join(format!(
            "crosshair-plus-status-wait-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("status.json");
        std::fs::write(&path, br#"{"selected_preset":"Old"}"#).unwrap();
        let before = std::fs::metadata(&path).unwrap().modified().unwrap();
        let writer_path = path.clone();
        let writer = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(90));
            std::fs::write(&writer_path, br#"{"selected_preset":"Crosshair+"}"#).unwrap();
        });
        assert!(wait_for_status_change(&path, before, Duration::from_secs(2)));
        writer.join().unwrap();
        assert_eq!(read_status_json_at(&path).unwrap()["selected_preset"], "Crosshair+");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_running_instance_with_the_preset_already_selected_does_not_restart() {
        let fresh = FxSoundStatus { running: true, ..Default::default() };
        assert!(!preset_restart_needed(&fresh, true));
        assert!(preset_restart_needed(&fresh, false));
        assert!(!preset_restart_needed(&FxSoundStatus { running: false, ..fresh }, false));
    }

    fn endpoint(name: &str, is_default: bool) -> AudioDevice {
        AudioDevice { id: format!("id-{name}"), name: name.into(), is_default }
    }

    #[test]
    fn unstick_fixture_parses_power_output_and_bands() {
        let value: Value = serde_json::from_str(include_str!("../testdata/fxsound-status.json")).unwrap();
        let (power, output) = status_values(Some(&value));
        assert!(power);
        assert_eq!(output.as_deref(), Some("Headphones (Crusher Evo)"));
        assert_eq!(band_count(Some(&value)), 10);
        assert_eq!(status_float(Some(&value), "filter_q"), Some(2.0));
    }

    #[test]
    fn unstick_saved_output_wins_when_present() {
        let devices = [
            endpoint("Headphones (Crusher Evo)", false),
            endpoint("Monitor Speakers (AMD High Definition Audio Device)", true),
        ];
        let chosen = choose_output(&devices, Some("headphones (crusher evo)")).unwrap();
        assert_eq!(chosen.name, "Headphones (Crusher Evo)");
    }

    #[test]
    fn unstick_missing_saved_output_falls_back_to_default_then_first() {
        let devices = [endpoint("Monitor Speakers", false), endpoint("Headphones", true)];
        assert_eq!(choose_output(&devices, Some("Gone")).unwrap().name, "Headphones");
        let no_default = [endpoint("Monitor Speakers", false), endpoint("Headphones", false)];
        assert_eq!(choose_output(&no_default, None).unwrap().name, "Monitor Speakers");
    }

    #[test]
    fn unstick_verification_requires_the_reconnected_output_when_sound_is_on() {
        let recovered = FxSoundStatus {
            running: true,
            power: true,
            output: Some("Headphones (Crusher Evo)".into()),
            ..Default::default()
        };
        assert!(verify_unstick_result(&recovered, "headphones (crusher evo)", true).is_ok());

        let wrong_output = FxSoundStatus { output: Some("Speakers".into()), ..recovered.clone() };
        let error = verify_unstick_result(&wrong_output, "Headphones (Crusher Evo)", true).unwrap_err();
        assert!(error.contains("reported output 'Speakers' instead of 'Headphones (Crusher Evo)'"));

        let stopped = FxSoundStatus { running: false, ..recovered };
        assert!(verify_unstick_result(&stopped, "Headphones (Crusher Evo)", true)
            .unwrap_err()
            .contains("stopped again"));
    }

    #[test]
    fn unstick_verification_keeps_sound_off_and_skips_the_unrequested_output() {
        let still_off = FxSoundStatus {
            running: true,
            power: false,
            output: Some("Old output".into()),
            ..Default::default()
        };
        assert!(verify_unstick_result(&still_off, "Headphones", false).is_ok());

        let came_back_on = FxSoundStatus { power: true, ..still_off };
        let error = verify_unstick_result(&came_back_on, "Headphones", false).unwrap_err();
        assert!(error.contains("reported power=true instead of false"));
    }

    #[test]
    fn unstick_rejects_fxsound_enhancer_output() {
        let devices = [
            endpoint("Speakers (FxSound Audio Enhancer)", true),
            endpoint("Headphones (Crusher Evo)", false),
        ];
        assert!(is_fxsound_output("Speakers (FxSound Audio Enhancer)"));
        assert_eq!(
            choose_output(&devices, Some("Speakers (FxSound Audio Enhancer)")).unwrap().name,
            "Headphones (Crusher Evo)"
        );
        assert!(choose_output(&[endpoint("FxSound Speakers", true)], None).is_none());
    }

    #[test]
    fn status_payload_reads_fxsound_1_2_16_shape() {
        let value = serde_json::json!({
            "version": "1.2.16.0",
            "power": false,
            "selected_preset": "Trap",
            "selected_output": "Monitor Speakers (AMD High Definition Audio Device)",
            "equalizer": { "num_bands": 10, "master_gain": 0.0, "volume_leveling": 0.0, "filter_q": 1.5 },
            "effects": { "ambience": 7.0 }
        });
        let (power, output) = status_values(Some(&value));
        assert!(!power);
        assert_eq!(output.as_deref(), Some("Monitor Speakers (AMD High Definition Audio Device)"));
        assert_eq!(band_count(Some(&value)), 10);
        assert_eq!(status_float(Some(&value), "filter_q"), Some(1.5));
        assert_eq!(band_count(None), 0);
    }

    #[test]
    fn release_metadata_parses_version_and_installer() {
        let value = serde_json::json!({
            "name": "Version 1.2.16.0",
            "tag_name": "latest",
            "assets": [{
                "name": "fxsound_setup.exe",
                "browser_download_url": "https://example.invalid/fxsound_setup.exe",
                "digest": "sha256:ABC123"
            }]
        });
        assert_eq!(version_from_release(&value).as_deref(), Some("1.2.16.0"));
        let info = installer_info_from_release(&value).unwrap();
        assert_eq!(info.url, "https://example.invalid/fxsound_setup.exe");
        assert_eq!(info.digest.as_deref(), Some("abc123"));
        assert!(version_is_newer("1.2.17.0", "1.2.16.0"));
        assert!(!version_is_newer("1.2.16.0", "1.2.16.0"));
        assert!(!version_is_newer("1.1.16.0", "1.2.16.0"));
    }

    #[test]
    fn winget_setup_prefers_an_in_place_upgrade_before_install() {
        let upgrade = winget_upgrade_args();
        let install = winget_install_args();
        assert_eq!(upgrade[0], "upgrade");
        assert_eq!(install[0], "install");
        assert_eq!(upgrade[2], "FxSound.FxSound");
        assert_eq!(install[2], "FxSound.FxSound");
        assert_eq!(installer_args(), ["/exenoui", "/quiet", "/norestart"]);
    }

    #[test]
    fn version_compare() {
        assert!(version_at_least("1.2.11.0", 1, 2, 11));
        assert!(!version_at_least("1.1.16.0", 1, 2, 11));
        assert!(version_at_least("1.2.16.0", 1, 2, 11));
    }

    #[test]
    fn cli_timeout_kills_a_nonresponding_process() {
        let mut child = Command::new(std::env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".into()))
            .args(["/C", "ping -n 5 127.0.0.1 >nul"])
            .creation_flags(CREATE_NO_WINDOW)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn cmd");
        let started = Instant::now();
        let error = wait_child_timeout(&mut child, Duration::from_millis(120)).expect_err("ping should exceed the timeout");
        assert!(error.contains("did not respond"), "{error}");
        assert!(started.elapsed() < Duration::from_secs(2));
        assert!(child.try_wait().unwrap().is_some(), "the timed-out child must be reaped");
    }

    #[test]
    fn backup_copies_the_real_fxsound_appdata_tree_once() {
        let Some(src) = settings_path().parent().map(Path::to_path_buf) else { return };
        if !src.exists() {
            return;
        }
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dest = std::env::temp_dir().join(format!("crosshair-plus-fxsound-backup-{}-{stamp}", std::process::id()));
        backup_settings_to(&src, &dest).unwrap();
        assert!(dest.join("FxSound.settings").is_file(), "settings file was not backed up");
        let marker = dest.join("second-call-marker");
        std::fs::write(&marker, b"x").unwrap();
        backup_settings_to(&src, &dest).unwrap();
        assert!(marker.exists(), "a second backup must not overwrite the first snapshot");
        let _ = std::fs::remove_dir_all(&dest);
    }

    #[test]
    fn reinstall_backup_names_are_timestamped_and_collision_safe() {
        assert_eq!(reinstall_backup_name(1_700_000_000_000, 0), "fxsound-reinstall-backup-1700000000000");
        assert_eq!(reinstall_backup_name(1_700_000_000_000, 3), "fxsound-reinstall-backup-1700000000000-3");
    }

    #[test]
    fn managed_preset_text_is_a_real_fac_with_the_current_curve() {
        let mut audio = AudioConfig { enabled: true, ..Default::default() };
        audio.bands = [1.0, 2.2, 3.45, 2.75, 1.0, -2.0, -3.78, -2.45, -0.2, 1.0];
        let text = managed_preset_text(&audio);
        assert!(text.starts_with("CLASS1 : Effect Type\r\n9: Version\r\nCrosshair+\r\n"));
        assert!(text.contains("10: Number of EQ Bands"));
        assert!(text.contains("Band 7\r\n   2200: CF\r\n   -3.78: Boost/Cut"));
        assert!(text.contains("Band 8\r\n   4550: CF\r\n   -2.45: Boost/Cut"));
        assert!(text.contains("Band 1\r\n   62.5: CF\r\n   1: Boost/Cut"));
        assert_eq!(text.matches("Band ").count(), 10);
    }

    #[test]
    fn preset_fingerprint_detects_edits_and_ownership_never_accepts_a_foreign_file() {
        let root = std::env::temp_dir().join(format!(
            "crosshair-plus-preset-owner-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join(MANAGED_PRESET_FILE);
        let bytes = b"Crosshair+ preset";
        std::fs::write(&path, bytes).unwrap();
        let owner_path = root.join("owner.json");
        assert!(!preset_is_owned_at(&path, bytes, &owner_path), "no ownership marker means the file is foreign");
        let owner = PresetOwnership {
            name: MANAGED_PRESET.into(),
            file: path.to_string_lossy().to_string(),
            fingerprint: fingerprint(bytes),
        };
        std::fs::write(&owner_path, serde_json::to_string(&owner).unwrap()).unwrap();
        assert!(preset_is_owned_at(&path, bytes, &owner_path), "the marker must accept the file it owns");
        assert!(!preset_is_owned_at(&path, b"edited", &owner_path), "an edited file must stop being owned");
        assert!(same_file(&owner.file, &path));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn apply_plans_a_start_when_fxsound_is_not_running() {
        let current = FxSoundStatus {
            installed: true,
            running: false,
            cli_ready: true,
            ..Default::default()
        };
        let audio = AudioConfig { enabled: true, ..Default::default() };
        let plan = plan_apply(&current, &audio, true);
        assert!(plan.start);
        assert!(plan.fallback_device.is_none());
    }

    #[test]
    fn apply_keeps_current_output_when_saved_device_is_missing() {
        let current = FxSoundStatus {
            installed: true,
            running: true,
            cli_ready: true,
            ..Default::default()
        };
        let audio = AudioConfig {
            enabled: true,
            device: Some("Gone Headset".into()),
            ..Default::default()
        };
        let plan = plan_apply(&current, &audio, false);
        assert!(!plan.start);
        assert_eq!(plan.fallback_device.as_deref(), Some("Gone Headset"));
        assert_eq!(plan.effective_device, None);
    }

    #[test]
    fn user_owned_preset_is_repaired_without_data_loss() {
        let root = std::env::temp_dir().join(format!(
            "crosshair-plus-preset-repair-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let path = root.join("Presets").join(MANAGED_PRESET_FILE);
        let owner = root.join("fxsound-preset.json");
        let backup_dir = root.join("backups");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let user_bytes = b"user-owned Crosshair+ preset";
        std::fs::write(&path, user_bytes).unwrap();
        assert_eq!(managed_preset_conflict_at(&path, &owner, false), Some(PresetConflict::UserFile(path.clone())));
        let audio = AudioConfig { enabled: true, ..Default::default() };
        let repair = repair_managed_preset_at(&path, &owner, &backup_dir, &audio, false).unwrap();
        let backup = repair.expect("the user preset must be backed up");
        assert_eq!(std::fs::read(&backup).unwrap(), user_bytes);
        let managed = std::fs::read(&path).unwrap();
        assert_ne!(managed, user_bytes);
        assert!(preset_is_owned_at(&path, &managed, &owner));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn repaired_preset_refuses_to_touch_the_original_when_backup_fails() {
        let root = std::env::temp_dir().join(format!(
            "crosshair-plus-preset-repair-fail-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let path = root.join("Presets").join(MANAGED_PRESET_FILE);
        let owner = root.join("fxsound-preset.json");
        let backup_dir = root.join("backups");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&backup_dir, b"not a directory").unwrap();
        let user_bytes = b"do not lose this preset";
        std::fs::write(&path, user_bytes).unwrap();
        let audio = AudioConfig { enabled: true, ..Default::default() };
        assert!(repair_managed_preset_at(&path, &owner, &backup_dir, &audio, false).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), user_bytes, "the user file must survive a failed backup");
        assert!(!owner.exists(), "no ownership marker may be written when repair did not complete");
        let _ = std::fs::remove_dir_all(&root);
    }
}
