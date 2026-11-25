use std::collections::HashMap;
use std::ffi::c_void;
use std::os::windows::ffi::OsStrExt;
use std::path::Path;

use serde::Serialize;
use windows::core::{BOOL, PWSTR};
use windows::Win32::Foundation::{CloseHandle, LPARAM};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
};
use windows::Win32::Graphics::Gdi::{
    CreateCompatibleDC, DeleteDC, DeleteObject, GetDIBits, GetObjectW, BITMAP, BITMAPINFO, BI_RGB, DIB_RGB_COLORS,
    HGDIOBJ,
};
use windows::Win32::System::Threading::{
    GetCurrentProcessId, OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::UI::Shell::{SHGetFileInfoW, SHFILEINFOW, SHGFI_ICON, SHGFI_LARGEICON};
use windows::Win32::UI::WindowsAndMessaging::{
    DestroyIcon, EnumWindows, GetForegroundWindow, GetIconInfo, GetWindowTextLengthW, GetWindowTextW,
    GetWindowThreadProcessId, IsWindowVisible, ICONINFO,
};

use crate::config::{Config, Slot, SlotSnapshot, MAX_SLOTS, MAX_SLOT_NAME};

#[derive(Serialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RunningApp {
    pub exe: String,
    pub title: String,
    pub pid: u32,
    /// Cached 32px PNG data URL, or None when extraction failed.
    pub icon: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HotkeyOwner<'a> {
    Slot(&'a str),
    Module(&'a str),
}

fn key_label(vk: u32) -> String {
    if (0x70..=0x87).contains(&vk) {
        return format!("F{}", vk - 0x70 + 1);
    }
    if (0x30..=0x39).contains(&vk) || (0x41..=0x5A).contains(&vk) {
        return char::from_u32(vk).map(|c| c.to_string()).unwrap_or_default();
    }
    match vk {
        0x1B => "Esc",
        0x09 => "Tab",
        0x20 => "Space",
        0x0D => "Enter",
        0x24 => "Home",
        0x23 => "End",
        0x21 => "PageUp",
        0x22 => "PageDown",
        0x2D => "Insert",
        0x2E => "Delete",
        0x25 => "Left",
        0x26 => "Up",
        0x27 => "Right",
        0x28 => "Down",
        0x08 => "Backspace",
        _ => "",
    }
    .to_string()
}

pub fn canonical_hotkey(text: &str) -> Option<String> {
    let (mut mods, vk) = parse_hotkey(text)?;
    let mut parts = Vec::new();
    if mods & 0x0002 != 0 { parts.push("Ctrl".to_string()); mods &= !0x0002; }
    if mods & 0x0004 != 0 { parts.push("Shift".to_string()); mods &= !0x0004; }
    if mods & 0x0001 != 0 { parts.push("Alt".to_string()); mods &= !0x0001; }
    if mods & 0x0008 != 0 { parts.push("Win".to_string()); }
    let key = key_label(vk);
    if key.is_empty() { return None; }
    parts.push(key);
    Some(parts.join("+"))
}

fn reserved_hotkey(mods: u32, vk: u32) -> bool {
    matches!(
        (mods, vk),
        (0x0001, 0x09)   // Alt+Tab
            | (0x0001, 0x1B) // Alt+Esc
            | (0x0001, 0x73) // Alt+F4
            | (0x0001, 0x20) // Alt+Space
            | (0x0002, 0x1B) // Ctrl+Esc
            | (0x0006, 0x1B) // Ctrl+Shift+Esc
            | (0x0008, 0x4C) // Win+L
            | (0x0008, 0x44) // Win+D
            | (0x0008, 0x09) // Win+Tab
            | (0x0008, 0x45) // Win+E
            | (0x0008, 0x52) // Win+R
            | (0x0003, 0x2E) // Ctrl+Alt+Delete
    )
}

/// Validate a key against every other global binding. This is the single policy
/// used by slots and by the independent Aim/Sound/Display toggles.
pub fn validate_hotkey(config: &Config, text: &str, owner: HotkeyOwner<'_>) -> Result<(), String> {
    let Some((mods, vk)) = parse_hotkey(text) else {
        return Err("That key could not be registered.".into());
    };
    let canonical = canonical_hotkey(text).ok_or_else(|| "That key could not be registered.".to_string())?;
    let function_key = (0x70..=0x87).contains(&vk);
    if mods == 0 && !function_key {
        return Err("Add Ctrl, Alt or Shift, or use an F-key.".into());
    }
    if reserved_hotkey(mods, vk) {
        return Err("That shortcut is reserved by Windows.".into());
    }
    if mods == 0 && vk == 0x2C {
        return Err("Print Screen is reserved by Windows.".into());
    }
    let reset = canonical_hotkey("Ctrl+Shift+End").unwrap_or_default();
    if canonical == reset {
        return Err("That key is already assigned to Reset.".into());
    }
    for slot in &config.slots.list {
        if matches!(owner, HotkeyOwner::Slot(id) if id == slot.id) {
            continue;
        }
        let Some(existing) = slot.hotkey.as_deref().and_then(canonical_hotkey) else { continue };
        if existing == canonical {
            return Err(format!("That key is already assigned to {}.", slot.name));
        }
    }
    for (module, label, value) in [
        ("aim", "Aim", config.settings.aim_hotkey.as_str()),
        ("display", "Display", config.settings.display_hotkey.as_str()),
        ("audio", "Sound", config.settings.audio_hotkey.as_str()),
        ("restore", "Restore display", config.settings.restore_hotkey.as_str()),
        ("cycle", "Cycle slots", config.settings.cycle_hotkey.as_str()),
    ] {
        if value.is_empty() || matches!(owner, HotkeyOwner::Module(name) if name == module) {
            continue;
        }
        if canonical_hotkey(value).as_deref() == Some(canonical.as_str()) {
            return Err(format!("That key is already assigned to {label}."));
        }
    }
    Ok(())
}

pub fn validate_slot(config: &Config, slot: &Slot, is_new: bool) -> Result<(), String> {
    let name = slot.name.trim();
    if name.is_empty() {
        return Err("Enter a slot name.".into());
    }
    if name.chars().count() > MAX_SLOT_NAME {
        return Err(format!("Slot names are limited to {MAX_SLOT_NAME} characters."));
    }
    if slot.emoji.trim().is_empty() {
        return Err("Choose an emoji.".into());
    }
    if !is_new && !config.slots.list.iter().any(|s| s.id == slot.id) {
        return Err("That slot no longer exists.".into());
    }
    if is_new && config.slots.list.len() >= MAX_SLOTS {
        return Err(format!("Crosshair+ supports up to {MAX_SLOTS} slots."));
    }
    if config.slots.list.iter().any(|s| s.id != slot.id && s.emoji == slot.emoji) {
        return Err("That emoji is already used by another slot.".into());
    }
    if let Some(key) = &slot.hotkey {
        if !key.trim().is_empty() {
            validate_hotkey(config, key, HotkeyOwner::Slot(&slot.id))?;
        }
    }
    Ok(())
}

pub fn save_slot(config: &mut Config, mut slot: Slot) -> Result<(), String> {
    slot.name = slot.name.trim().chars().take(MAX_SLOT_NAME).collect();
    let is_new = !config.slots.list.iter().any(|s| s.id == slot.id);
    validate_slot(config, &slot, is_new)?;
    if is_new {
        config.slots.list.push(slot);
    } else if let Some(existing) = config.slots.list.iter_mut().find(|s| s.id == slot.id) {
        *existing = slot;
    }
    Ok(())
}

/// Save the currently edited slot (when provided) and then switch activation in
/// one operation. The save is validated before any active state is touched, so
/// a rejected edit cannot leave the app half-switched.
pub fn switch_slot(config: &mut Config, id: Option<&str>, save: Option<Slot>) -> Result<bool, String> {
    if let Some(slot) = save {
        save_slot(config, slot)?;
    }
    match id {
        Some(id) => {
            activate_slot(config, id)?;
            Ok(true)
        }
        None => Ok(deactivate_slot(config)),
    }
}

/// A persisted active slot means the previous process did not shut down
/// cleanly. Restore the pre-slot snapshot and clear the session marker.
pub fn restore_session_baseline(config: &mut Config) -> bool {
    if config.slots.active.is_none() && config.slots.baseline.is_none() {
        return false;
    }
    let _ = deactivate_slot(config);
    config.slots.active = None;
    config.slots.baseline = None;
    true
}

pub fn activate_slot(config: &mut Config, id: &str) -> Result<(), String> {
    let slot = config.slots.list.iter().find(|s| s.id == id).cloned().ok_or("Slot not found.")?;
    if config.slots.active.as_deref() == Some(id) {
        return Ok(());
    }
    if config.slots.baseline.is_none() {
        config.slots.baseline = Some(SlotSnapshot {
            crosshair: config.crosshair.clone(),
            display: config.display.clone(),
            audio: config.audio.clone(),
            crosshair_was_on: config.crosshair.enabled,
            display_was_on: config.display.enabled,
            audio_was_on: config.audio.enabled,
        });
    }
    if slot.crosshair.on {
        if let Some(preset) = slot.crosshair.preset.as_deref().and_then(|p| config.find_crosshair_preset(p)) {
            preset.apply_to(&mut config.crosshair);
            config.crosshair.active_preset = Some(preset.id);
        }
        config.crosshair.enabled = true;
        config.crosshair.normalize();
    }
    if slot.display.on {
        if let Some(preset) = slot.display.preset.as_deref().and_then(|p| config.find_display_preset(p)) {
            preset.apply_to(&mut config.display.global);
            if config.display.customize_each {
                for values in config.display.monitors.values_mut() {
                    preset.apply_to(values);
                }
            }
            config.display.active_preset = Some(preset.id);
        }
        config.display.enabled = true;
        config.display.normalize();
    }
    if slot.audio.on {
        if let Some(preset) = slot.audio.preset.as_deref().and_then(|p| config.find_audio_preset(p)) {
            config.audio.bands = preset.bands;
            config.audio.bass = preset.bass;
            config.audio.treble = preset.treble;
            config.audio.output_level = preset.output_level;
            config.audio.echo = preset.echo;
            config.audio.surround = preset.surround;
            config.audio.dynamic_boost = preset.dynamic_boost;
            config.audio.volume_leveling = preset.volume_leveling;
            config.audio.balance = preset.balance;
            config.audio.filter_q = preset.filter_q;
            config.audio.volume_guard = preset.volume_guard;
            config.audio.active_preset = Some(preset.id);
        }
        config.audio.enabled = true;
        config.audio.normalize();
    }
    config.slots.active = Some(id.to_string());
    Ok(())
}

/// Returns true when the active slot was actually reverted.
pub fn deactivate_slot(config: &mut Config) -> bool {
    let Some(active) = config.slots.active.clone() else { return false };
    let Some(slot) = config.slots.list.iter().find(|s| s.id == active).cloned() else {
        config.slots.active = None;
        config.slots.baseline = None;
        return false;
    };
    let Some(baseline) = config.slots.baseline.clone() else {
        config.slots.active = None;
        return false;
    };
    // Crash recovery applies a snapshot straight from a hand-editable config, so
    // the restored values are held to the same ranges as any other input.
    if slot.crosshair.on {
        config.crosshair = baseline.crosshair;
        config.crosshair.normalize();
    }
    if slot.display.on {
        config.display = baseline.display;
        config.display.normalize();
    }
    if slot.audio.on {
        config.audio = baseline.audio;
    }
    config.slots.active = None;
    config.slots.baseline = None;
    true
}

pub fn exe_matches(target: &str, running_exe: &str) -> bool {
    if target.eq_ignore_ascii_case(running_exe) {
        return true;
    }
    Path::new(target)
        .file_name()
        .and_then(|n| n.to_str())
        .map(|n| n.eq_ignore_ascii_case(running_exe))
        .unwrap_or(false)
}

pub fn slot_for_exe<'a>(config: &'a Config, exe: &str) -> Option<&'a Slot> {
    config.slots.list.iter().find(|s| s.target_exe.as_deref().map(|t| exe_matches(t, exe)).unwrap_or(false))
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CycleTarget {
    Slot(String),
    Off,
}

/// One step through the slot list, in display order. An unknown/stale active
/// id starts over at the first slot; the last slot steps to Off.
pub fn cycle_target(config: &Config) -> Option<CycleTarget> {
    let first = config.slots.list.first()?.id.clone();
    let index = config
        .slots
        .active
        .as_deref()
        .and_then(|id| config.slots.list.iter().position(|slot| slot.id == id));
    match index {
        None => Some(CycleTarget::Slot(first)),
        Some(index) if index + 1 < config.slots.list.len() => {
            Some(CycleTarget::Slot(config.slots.list[index + 1].id.clone()))
        }
        Some(_) => Some(CycleTarget::Off),
    }
}

/// The foreground watcher only activates a target slot when no slot is active.
pub fn auto_activate_for(config: &Config, foreground_exe: Option<&str>) -> Option<String> {
    if config.slots.active.is_some() {
        return None;
    }
    let exe = foreground_exe?;
    slot_for_exe(config, exe)
        .filter(|slot| slot.target_exe.is_some())
        .map(|slot| slot.id.clone())
}

/// True when the active game slot has a target process that is no longer running.
pub fn active_target_stopped(config: &Config) -> bool {
    let Some(id) = config.slots.active.as_deref() else { return false };
    let Some(target) = config.slots.list.iter().find(|slot| slot.id == id).and_then(|slot| slot.target_exe.as_deref()) else {
        return false;
    };
    !process_running(target)
}

pub fn parse_hotkey(text: &str) -> Option<(u32, u32)> {
    let mut mods = 0u32;
    let mut key = None;
    let mut keys = 0usize;
    for part in text.split('+').map(|p| p.trim()).filter(|p| !p.is_empty()) {
        match part.to_ascii_lowercase().as_str() {
            "ctrl" | "control" => mods |= 0x0002,
            "shift" => mods |= 0x0004,
            "alt" => mods |= 0x0001,
            "win" | "super" => mods |= 0x0008,
            other => {
                key = Some(virtual_key(other)?);
                keys += 1;
            }
        }
    }
    if keys != 1 { return None; }
    key.map(|k| (mods, k))
}

fn virtual_key(text: &str) -> Option<u32> {
    let upper = text.to_ascii_uppercase();
    if upper.len() == 1 {
        let c = upper.chars().next()?;
        if c.is_ascii_uppercase() || c.is_ascii_digit() {
            return Some(c as u32);
        }
    }
    if let Some(rest) = upper.strip_prefix('F') {
        if let Ok(n) = rest.parse::<u32>() {
            if (1..=24).contains(&n) {
                return Some(0x70 + n - 1);
            }
        }
    }
    Some(match upper.as_str() {
        "ESC" | "ESCAPE" => 0x1B,
        "TAB" => 0x09,
        "SPACE" => 0x20,
        "ENTER" | "RETURN" => 0x0D,
        "HOME" => 0x24,
        "END" => 0x23,
        "PAGEUP" | "PGUP" => 0x21,
        "PAGEDOWN" | "PGDN" => 0x22,
        "INSERT" | "INS" => 0x2D,
        "DELETE" | "DEL" => 0x2E,
        "LEFT" => 0x25,
        "UP" => 0x26,
        "RIGHT" => 0x27,
        "DOWN" => 0x28,
        "BACKSPACE" | "BACK" => 0x08,
        _ => return None,
    })
}

fn process_name(pid: u32) -> Option<String> {
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

pub fn process_running(target: &str) -> bool {
    unsafe {
        let Ok(snapshot) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else { return false };
        let mut entry = PROCESSENTRY32W::default();
        entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
        let mut found = false;
        if Process32FirstW(snapshot, &mut entry).is_ok() {
            loop {
                let end = entry.szExeFile.iter().position(|c| *c == 0).unwrap_or(entry.szExeFile.len());
                let exe = String::from_utf16_lossy(&entry.szExeFile[..end]);
                if exe_matches(target, &exe) {
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

pub fn current_foreground_app() -> Option<RunningApp> {
    unsafe {
        let hwnd = GetForegroundWindow();
        if hwnd.0.is_null() {
            return None;
        }
        let mut pid = 0u32;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        if pid == 0 || pid == GetCurrentProcessId() {
            return None;
        }
        let exe = process_name(pid)?;
        let mut title = vec![0u16; 512];
        let len = GetWindowTextW(hwnd, &mut title);
        let title = String::from_utf16_lossy(&title[..len as usize]);
        Some(RunningApp { exe, title, pid, icon: None })
    }
}

fn process_path(pid: u32) -> Option<std::path::PathBuf> {
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut buffer = [0u16; 1024];
        let mut len = buffer.len() as u32;
        let result = QueryFullProcessImageNameW(handle, PROCESS_NAME_WIN32, PWSTR(buffer.as_mut_ptr()), &mut len);
        let _ = CloseHandle(handle);
        result.ok()?;
        Some(std::path::PathBuf::from(String::from_utf16_lossy(&buffer[..len as usize])))
    }
}

fn base64(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity((bytes.len() + 2) / 3 * 4);
    for chunk in bytes.chunks(3) {
        let b1 = chunk.get(1).copied().unwrap_or(0);
        let b2 = chunk.get(2).copied().unwrap_or(0);
        out.push(TABLE[(chunk[0] >> 2) as usize] as char);
        out.push(TABLE[(((chunk[0] & 0x03) << 4) | (b1 >> 4)) as usize] as char);
        out.push(if chunk.len() > 1 { TABLE[(((b1 & 0x0f) << 2) | (b2 >> 6)) as usize] as char } else { '=' });
        out.push(if chunk.len() > 2 { TABLE[(b2 & 0x3f) as usize] as char } else { '=' });
    }
    out
}

pub(crate) fn icon_data_url(exe_path: &Path) -> Option<String> {
    unsafe {
        let wide: Vec<u16> = exe_path.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
        let mut info = SHFILEINFOW::default();
        let result = SHGetFileInfoW(
            windows::core::PCWSTR(wide.as_ptr()),
            windows::Win32::Storage::FileSystem::FILE_ATTRIBUTE_NORMAL,
            Some(&mut info),
            std::mem::size_of::<SHFILEINFOW>() as u32,
            SHGFI_ICON | SHGFI_LARGEICON,
        );
        if result == 0 || info.hIcon.is_invalid() {
            return None;
        }
        let mut icon_info = ICONINFO::default();
        let mut pixels = Vec::new();
        let mut width = 32u32;
        let mut height = 32u32;
        if GetIconInfo(info.hIcon, &mut icon_info).is_ok() && !icon_info.hbmColor.is_invalid() {
            let mut bitmap = BITMAP::default();
            if GetObjectW(icon_info.hbmColor.into(), std::mem::size_of::<BITMAP>() as i32, Some(&mut bitmap as *mut _ as *mut c_void)) > 0
                && bitmap.bmWidth > 0
                && bitmap.bmHeight > 0
            {
                width = bitmap.bmWidth as u32;
                height = bitmap.bmHeight as u32;
                let mut bmi = BITMAPINFO::default();
                bmi.bmiHeader.biSize = std::mem::size_of_val(&bmi.bmiHeader) as u32;
                bmi.bmiHeader.biWidth = width as i32;
                bmi.bmiHeader.biHeight = -(height as i32);
                bmi.bmiHeader.biPlanes = 1;
                bmi.bmiHeader.biBitCount = 32;
                bmi.bmiHeader.biCompression = BI_RGB.0;
                pixels = vec![0u8; (width * height * 4) as usize];
                let dc = CreateCompatibleDC(None);
                let copied = GetDIBits(
                    dc,
                    icon_info.hbmColor,
                    0,
                    height,
                    Some(pixels.as_mut_ptr() as *mut c_void),
                    &mut bmi,
                    DIB_RGB_COLORS,
                );
                let _ = DeleteDC(dc);
                if copied == 0 {
                    pixels.clear();
                }
            }
            let _ = DeleteObject(HGDIOBJ(icon_info.hbmColor.0));
        }
        if !icon_info.hbmMask.is_invalid() {
            let _ = DeleteObject(HGDIOBJ(icon_info.hbmMask.0));
        }
        let _ = DestroyIcon(info.hIcon);
        if pixels.is_empty() {
            return None;
        }
        if pixels.chunks_exact(4).all(|p| p[3] == 0) {
            for pixel in pixels.chunks_exact_mut(4) {
                pixel[3] = 255;
            }
        }
        let png = crate::screenshot::encode_bgra_png(width, height, &pixels).ok()?;
        Some(format!("data:image/png;base64,{}", base64(&png)))
    }
}

pub fn visible_apps() -> Vec<RunningApp> {
    let mut apps: Vec<RunningApp> = Vec::new();
    unsafe extern "system" fn callback(hwnd: windows::Win32::Foundation::HWND, lparam: LPARAM) -> BOOL {
        let apps = &mut *(lparam.0 as *mut Vec<RunningApp>);
        unsafe {
            if !IsWindowVisible(hwnd).as_bool() {
                return true.into();
            }
            let len = GetWindowTextLengthW(hwnd);
            if len == 0 {
                return true.into();
            }
            let mut title = vec![0u16; (len + 1) as usize];
            GetWindowTextW(hwnd, &mut title);
            let mut pid = 0u32;
            GetWindowThreadProcessId(hwnd, Some(&mut pid));
            if pid == 0 || pid == GetCurrentProcessId() {
                return true.into();
            }
            let Some(exe) = process_name(pid) else { return true.into() };
            let lower = exe.to_ascii_lowercase();
            if lower == "crosshairplus.exe"
                || lower == "explorer.exe"
                || lower == "searchhost.exe"
                || lower == "startmenuexperiencehost.exe"
                || lower == "shellexperiencehost.exe"
                || lower == "textinputhost.exe"
            {
                return true.into();
            }
            if let Some(existing) = apps.iter_mut().find(|a| a.exe.eq_ignore_ascii_case(&exe)) {
                if existing.title.is_empty() {
                    existing.title = String::from_utf16_lossy(&title).trim_matches('\0').to_string();
                }
            } else {
                apps.push(RunningApp {
                    exe,
                    title: String::from_utf16_lossy(&title).trim_matches('\0').to_string(),
                    pid,
                    icon: None,
                });
            }
        }
        true.into()
    }
    unsafe {
        let _ = EnumWindows(Some(callback), LPARAM(&mut apps as *mut _ as isize));
    }
    apps.sort_by(|a, b| a.exe.to_ascii_lowercase().cmp(&b.exe.to_ascii_lowercase()));
    let mut icon_cache: HashMap<String, Option<String>> = HashMap::new();
    for app in &mut apps {
        app.icon = icon_cache
            .entry(app.exe.to_ascii_lowercase())
            .or_insert_with(|| process_path(app.pid).as_deref().and_then(icon_data_url))
            .clone();
    }
    apps
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, SlotModule};

    fn slot(id: &str, emoji: &str) -> Slot {
        Slot {
            id: id.into(),
            name: "Temporary".into(),
            emoji: emoji.into(),
            hotkey: Some("F12".into()),
            target_exe: None,
            crosshair: SlotModule { on: true, preset: Some("style_dot".into()) },
            display: SlotModule::default(),
            audio: SlotModule::default(),
        }
    }

    #[test]
    fn app_icon_is_extracted_and_png_encoded() {
        let exe = std::env::current_exe().expect("test executable path");
        let icon = icon_data_url(&exe).expect("the test executable has an extractable icon");
        assert!(icon.starts_with("data:image/png;base64,"), "unexpected icon URL prefix");
        assert!(icon.len() > 200, "icon payload looks empty");
    }

    #[test]
    fn slot_activation_restores_partial_baseline() {
        let mut config = Config::default();
        config.crosshair.enabled = false;
        config.crosshair.style = "plus".into();
        config.audio.enabled = false;
        config.slots.baseline = None;

        assert!(activate_slot(&mut config, "slot-fps").is_ok());
        assert_eq!(config.slots.active.as_deref(), Some("slot-fps"));
        assert!(config.crosshair.enabled);
        assert_eq!(config.crosshair.style, "tsight");
        assert!(config.audio.enabled);

        assert!(deactivate_slot(&mut config));
        assert!(config.slots.active.is_none());
        assert!(config.slots.baseline.is_none());
        assert!(!config.crosshair.enabled);
        assert_eq!(config.crosshair.style, "plus");
        assert!(!config.audio.enabled);
    }

    #[test]
    fn crash_recovery_clamps_a_hand_edited_baseline_snapshot() {
        let mut config = Config::default();
        let mut game = slot("slot-game", "");
        game.display = SlotModule { on: true, preset: None };
        config.slots.list = vec![game];
        let mut baseline = crate::config::SlotSnapshot {
            crosshair: config.crosshair.clone(),
            display: config.display.clone(),
            audio: config.audio.clone(),
            crosshair_was_on: false,
            display_was_on: false,
            audio_was_on: false,
        };
        baseline.crosshair.size = 900.0;
        baseline.crosshair.style = "bogus".into();
        baseline.display.global.brightness = 900.0;
        baseline.display.mode = "bogus".into();
        config.slots.active = Some("slot-game".into());
        config.slots.baseline = Some(baseline);

        assert!(restore_session_baseline(&mut config), "crash recovery must run");
        assert_eq!(config.crosshair.size, 48.0, "the snapshot's crosshair size is clamped");
        assert_eq!(config.crosshair.style, "plus", "an unknown style falls back");
        assert_eq!(config.display.global.brightness, 40.0, "the snapshot's brightness is clamped");
        assert_eq!(config.display.mode, "all", "an unknown display mode falls back");
        assert!(config.slots.active.is_none() && config.slots.baseline.is_none());
    }

    #[test]
    fn slot_activation_preserves_the_monitor_panel_and_crosshair_image() {
        let mut config = Config::default();
        config.display.global.panel = "oled".into();
        config.crosshair.image = Some("keep.png".into());
        config.crosshair.monitor = Some("monitor-1".into());

        assert!(activate_slot(&mut config, "slot-fps").is_ok());
        assert_eq!(config.display.global.panel, "oled");
        assert_eq!(config.crosshair.image.as_deref(), Some("keep.png"));
        assert_eq!(config.crosshair.monitor.as_deref(), Some("monitor-1"));
        assert_eq!(config.crosshair.size, 16.0, "the slot applies the tuned T-Sight preset");
    }

    #[test]
    fn switch_slot_saves_then_activates_and_recovery_is_idempotent() {
        let mut config = Config::default();
        config.crosshair.style = "plus".into();
        let mut edited = config.slots.list[0].clone();
        edited.name = "Edited Competitive".into();
        assert!(switch_slot(&mut config, Some("slot-fps"), Some(edited)).unwrap());
        assert_eq!(config.slots.list[0].name, "Edited Competitive");
        assert_eq!(config.slots.active.as_deref(), Some("slot-fps"));
        assert!(config.slots.baseline.is_some());

        assert!(restore_session_baseline(&mut config));
        assert!(config.slots.active.is_none());
        assert!(config.slots.baseline.is_none());
        assert_eq!(config.crosshair.style, "plus");
        assert!(!restore_session_baseline(&mut config), "second recovery must be a no-op");
    }

    #[test]
    fn switch_rejects_invalid_edit_before_activation() {
        let mut config = Config::default();
        let mut edited = config.slots.list[0].clone();
        edited.name.clear();
        assert!(switch_slot(&mut config, Some("slot-fps"), Some(edited)).is_err());
        assert!(config.slots.active.is_none());
        assert!(config.slots.baseline.is_none());
    }

    #[test]
    fn validation_rejects_long_names_and_duplicate_emoji() {
        let mut config = Config::default();
        let mut long = slot("slot-new", "🦄");
        long.name = "x".repeat(MAX_SLOT_NAME + 1);
        assert!(validate_slot(&config, &long, true).is_err());

        let mut valid = slot("slot-new", "🦄");
        valid.name = "x".repeat(MAX_SLOT_NAME);
        assert!(validate_slot(&config, &valid, true).is_ok());
        save_slot(&mut config, valid).unwrap();

        let duplicate = slot("slot-copy", "🦄");
        assert!(validate_slot(&config, &duplicate, true).is_err());
    }

    #[test]
    fn foreground_watcher_selects_only_targeted_slots() {
        let mut config = Config::default();
        config.slots.list[0].target_exe = Some("C:\\Games\\Aim.exe".into());
        assert_eq!(auto_activate_for(&config, Some("notepad.exe")), None);
        assert_eq!(auto_activate_for(&config, Some("Aim.exe")).as_deref(), Some("slot-fps"));
        config.slots.active = Some("slot-fps".into());
        assert_eq!(auto_activate_for(&config, Some("Aim.exe")), None);
    }

    #[test]
    fn active_target_stopped_requires_an_active_target() {
        let mut config = Config::default();
        assert!(!active_target_stopped(&config));
        config.slots.active = Some("slot-fps".into());
        assert!(!active_target_stopped(&config));
        config.slots.list[0].target_exe = Some("definitely-not-running-crosshairplus-test.exe".into());
        assert!(active_target_stopped(&config));
    }

    #[test]
    fn hotkey_policy_checks_every_owner_and_reserved_key() {
        let config = Config::default();
        assert_eq!(canonical_hotkey("shift+ctrl+1").as_deref(), Some("Ctrl+Shift+1"));
        assert!(validate_hotkey(&config, "Shift+Ctrl+1", HotkeyOwner::Module("display")).is_ok());
        assert!(validate_hotkey(&config, "F8", HotkeyOwner::Module("audio")).is_err(), "Aim owns F8");
        assert!(validate_hotkey(&config, "F9", HotkeyOwner::Module("display")).is_err(), "a slot owns F9");
        assert!(validate_hotkey(&config, "Ctrl+Shift+End", HotkeyOwner::Module("display")).is_err(), "Reset is reserved");
        assert!(validate_hotkey(&config, "Alt+Tab", HotkeyOwner::Module("display")).is_err(), "Windows owns Alt+Tab");
        assert!(validate_hotkey(&config, "Ctrl+Alt+Delete", HotkeyOwner::Module("display")).is_err(), "Windows owns Ctrl+Alt+Delete");
        assert!(validate_hotkey(&config, "A", HotkeyOwner::Module("display")).is_err(), "bare letters need a modifier");
        assert!(validate_hotkey(&config, "Ctrl+", HotkeyOwner::Module("display")).is_err(), "malformed keys are rejected");
        assert!(validate_hotkey(&config, "F8", HotkeyOwner::Module("aim")).is_ok(), "an owner may keep its own key");
    }

    #[test]
    fn cycle_walks_every_slot_then_turns_off() {
        let mut config = Config::default();
        assert_eq!(cycle_target(&config), Some(CycleTarget::Slot("slot-fps".into())));
        config.slots.active = Some("slot-fps".into());
        assert_eq!(cycle_target(&config), Some(CycleTarget::Slot("slot-night".into())));
        config.slots.active = Some("slot-night".into());
        assert_eq!(cycle_target(&config), Some(CycleTarget::Slot("slot-cinema".into())));
        config.slots.active = Some("slot-cinema".into());
        assert_eq!(cycle_target(&config), Some(CycleTarget::Off));
        assert_eq!(cycle_target(&Config::default()), Some(CycleTarget::Slot("slot-fps".into())));
    }

    #[test]
    fn cycle_restarts_at_the_first_slot_when_active_is_stale_or_empty() {
        let mut config = Config::default();
        config.slots.active = Some("removed-slot".into());
        assert_eq!(cycle_target(&config), Some(CycleTarget::Slot("slot-fps".into())));
        config.slots.list.clear();
        assert_eq!(cycle_target(&config), None);
    }

    #[test]
    fn cycle_hotkey_uses_the_same_conflict_policy() {
        let config = Config::default();
        assert!(validate_hotkey(&config, "F11", HotkeyOwner::Module("cycle")).is_err(), "a slot owns F11");
        assert!(validate_hotkey(&config, "F8", HotkeyOwner::Module("cycle")).is_err(), "Aim owns F8");
        assert!(validate_hotkey(&config, "Ctrl+Shift+End", HotkeyOwner::Module("cycle")).is_err(), "Reset is reserved");
        assert!(validate_hotkey(&config, "Ctrl+Shift+F11", HotkeyOwner::Module("cycle")).is_ok());
    }

    #[test]
    fn game_watcher_detects_a_live_process_and_reverts_a_stopped_target() {
        let current = std::env::current_exe().unwrap().to_string_lossy().into_owned();
        assert!(process_running(&current), "the test process was not visible to the watcher");

        let mut config = Config::default();
        config.slots.list[0].target_exe = Some(current.clone());
        assert_eq!(auto_activate_for(&config, Some(&current)).as_deref(), Some("slot-fps"));
        assert!(activate_slot(&mut config, "slot-fps").is_ok());
        assert!(!active_target_stopped(&config), "a live target was treated as stopped");
        assert!(deactivate_slot(&mut config));
        assert!(config.slots.active.is_none());
        assert!(config.slots.baseline.is_none());

        config.slots.list[0].target_exe = Some("definitely-not-running-crosshairplus-test.exe".into());
        assert!(activate_slot(&mut config, "slot-fps").is_ok());
        assert!(active_target_stopped(&config), "a stopped target was not detected");
        assert!(deactivate_slot(&mut config));
        assert!(config.slots.active.is_none());
        assert!(config.slots.baseline.is_none());
    }
}
