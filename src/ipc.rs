use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::audio::{AudioDevice, FxSoundStatus};
use crate::config::{AudioConfig, AudioPreset, Config, CrosshairConfig, CrosshairPreset, DisplayConfig, DisplayPreset, Settings, Slot, StoredImage};
use crate::monitors::MonitorInfo;
use crate::slots::RunningApp;

#[derive(Serialize)]
struct EventEnvelope<T: Serialize> {
    v: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<String>,
    #[serde(flatten)]
    event: T,
}

#[derive(Serialize)]
#[serde(tag = "type", content = "payload", rename_all = "PascalCase")]
pub enum Event {
    FullState(Box<FullStatePayload>),
    PreviewTick { v: u64 },
    DisplayLut { r: Vec<u8>, g: Vec<u8>, b: Vec<u8>, applied: bool, warning: Option<DisplayWarning> },
    SoundMeter { peak: f32, #[serde(rename = "headroomDb")] headroom_db: f32 },
    MonitorsUpdated { monitors: Vec<MonitorInfo> },
    InstalledGames {
        games: Vec<crate::games::InstalledGame>,
        #[serde(rename = "timedOut")]
        timed_out: bool,
    },
    #[serde(rename = "FxSoundStatus")]
    FxSoundStatusEvent { #[serde(flatten)] status: FxSoundStatus },
    BackupPreview { preview: crate::backup::BackupPreview },
    BackupResult { ok: bool, message: String },
    StoredImageResult { name: String, ok: bool },
    RecentImages { images: Vec<StoredImage> },
    HotkeyConflict { #[serde(rename = "slotId")] slot_id: Option<String>, key: String, owner: String },
    HotkeyResolved { #[serde(rename = "slotId")] slot_id: Option<String>, key: String },
    DiagnosticBanner { code: String, message: String, recoverable: bool },
    AudioRecovery { code: String, message: String, actions: Vec<DiagnosticAction> },
}

#[derive(Serialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct DiagnosticAction {
    pub id: String,
    pub label: String,
}

#[derive(Serialize, Clone, Debug, Default)]
#[serde(rename_all = "camelCase")]
pub struct DisplayWarning {
    pub code: String,
    pub message: String,
}

#[derive(Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct FullStatePayload {
    pub config: Config,
    pub monitors: Vec<MonitorInfo>,
    pub fx_sound: FxSoundStatus,
    pub audio_devices: Vec<AudioDevice>,
    pub crosshair_presets: Vec<CrosshairPreset>,
    pub display_presets: Vec<DisplayPreset>,
    pub audio_presets: Vec<AudioPreset>,
    pub app_version: String,
    pub selected_slot: Option<String>,
    pub data_dir: String,
    pub running_apps: Vec<RunningApp>,
    pub installed_games: Vec<crate::games::InstalledGame>,
    pub audio_curve: Vec<(f32, f32)>,
    pub master_gain: f32,
    pub launch_at_login: bool,
    /// A restore left a safety copy that the UI can offer to undo.
    pub can_undo_restore: bool,
    /// Images already stored in the data folder, newest first. The Recent menu
    /// reads this instead of opening a picker again.
    pub crosshair_images: Vec<StoredImage>,
}

pub fn to_script(event: Event) -> String {
    let envelope = EventEnvelope { v: 1, id: None, event };
    match serde_json::to_string(&envelope) {
        Ok(json) => format!("window.__crosshair_onMessage({json});"),
        Err(err) => {
            crate::logging::log(&format!("ipc: event serialization failed: {err}"));
            String::new()
        }
    }
}

#[derive(Deserialize)]
struct RawEnvelope {
    #[serde(default)]
    v: u32,
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    payload: Value,
}

fn p<T: for<'de> Deserialize<'de>>(value: Value) -> Option<T> {
    let value = if value.is_null() { Value::Object(Default::default()) } else { value };
    match serde_json::from_value(value) {
        Ok(parsed) => Some(parsed),
        Err(err) => {
            crate::logging::log(&format!("ipc: invalid payload: {err}"));
            None
        }
    }
}

#[derive(Debug, Clone)]
pub enum Command {
    Ready,
    SetTab { tab: String },
    ToggleMaster { module: String },
    UpdateCrosshair { crosshair: CrosshairConfig },
    UpdateDisplay { display: DisplayConfig },
    CompareDisplay { active: bool },
    CompareSound { active: bool },
    UpdateSound { audio: AudioConfig },
    SavePreset { module: String, name: String, image_mode: bool },
    DeletePreset { module: String, id: String },
    AssignHotkey { slot_id: String, hotkey: String },
    ClearHotkey { slot_id: String },
    AssignModuleHotkey { module: String, hotkey: String },
    ClearModuleHotkey { module: String },
    ImportImage { path: Option<String> },
    RefreshRecentImages,
    UseStoredImage { name: String },
    DeleteStoredImage { name: String },
    RemoveImage,
    ImportEq { path: Option<String>, text: Option<String>, name: Option<String> },
    FlattenEqImport,
    ClearEqImport,
    BackupModalOpen { open: bool },
    CreateBackup,
    BrowseBackup,
    RestoreBackup,
    UndoRestore,
    SelectSlot { id: Option<String> },
    SaveSlot { slot: Slot },
    ActivateSlot { id: Option<String>, slot: Option<Slot> },
    DeleteSlot { id: String },
    BrowseSlotExe { slot_id: String },
    ScanGames,
    SettingsChanged { settings: Settings },
    SetupAudio,
    RepairAudio,
    RepairManagedPreset,
    UnstickAudio,
    OpenFxSound,
    OpenFxSoundPresets,
    OpenFxSoundDownload,
    OpenWindowsDisplaySettings,
    OpenDataFolder,
    ResetDisplay,
    FactoryReset,
    OpenLog,
    OpenMain,
    OpenSettings,
    ExitApp,
    HideTray,
}

impl Command {
    /// Stable name reported by the UI watchdog, so a stalled command handler
    /// names the command instead of an anonymous stage.
    pub fn name(&self) -> &'static str {
        match self {
            Command::Ready => "Ready",
            Command::SetTab { .. } => "SetTab",
            Command::ToggleMaster { .. } => "ToggleMaster",
            Command::UpdateCrosshair { .. } => "UpdateCrosshair",
            Command::UpdateDisplay { .. } => "UpdateDisplay",
            Command::CompareDisplay { .. } => "CompareDisplay",
            Command::CompareSound { .. } => "CompareSound",
            Command::UpdateSound { .. } => "UpdateSound",
            Command::SavePreset { .. } => "SavePreset",
            Command::DeletePreset { .. } => "DeletePreset",
            Command::AssignHotkey { .. } => "AssignHotkey",
            Command::ClearHotkey { .. } => "ClearHotkey",
            Command::AssignModuleHotkey { .. } => "AssignModuleHotkey",
            Command::ClearModuleHotkey { .. } => "ClearModuleHotkey",
            Command::ImportImage { .. } => "ImportImage",
            Command::RefreshRecentImages => "RefreshRecentImages",
            Command::UseStoredImage { .. } => "UseStoredImage",
            Command::DeleteStoredImage { .. } => "DeleteStoredImage",
            Command::RemoveImage => "RemoveImage",
            Command::ImportEq { .. } => "ImportEq",
            Command::FlattenEqImport => "FlattenEqImport",
            Command::ClearEqImport => "ClearEqImport",
            Command::BackupModalOpen { .. } => "BackupModalOpen",
            Command::CreateBackup => "CreateBackup",
            Command::BrowseBackup => "BrowseBackup",
            Command::RestoreBackup => "RestoreBackup",
            Command::UndoRestore => "UndoRestore",
            Command::SelectSlot { .. } => "SelectSlot",
            Command::SaveSlot { .. } => "SaveSlot",
            Command::ActivateSlot { .. } => "ActivateSlot",
            Command::DeleteSlot { .. } => "DeleteSlot",
            Command::BrowseSlotExe { .. } => "BrowseSlotExe",
            Command::ScanGames => "ScanGames",
            Command::SettingsChanged { .. } => "SettingsChanged",
            Command::SetupAudio => "SetupAudio",
            Command::RepairAudio => "RepairAudio",
            Command::RepairManagedPreset => "RepairManagedPreset",
            Command::UnstickAudio => "UnstickAudio",
            Command::OpenFxSound => "OpenFxSound",
            Command::OpenFxSoundPresets => "OpenFxSoundPresets",
            Command::OpenFxSoundDownload => "OpenFxSoundDownload",
            Command::OpenWindowsDisplaySettings => "OpenWindowsDisplaySettings",
            Command::OpenDataFolder => "OpenDataFolder",
            Command::ResetDisplay => "ResetDisplay",
            Command::FactoryReset => "FactoryReset",
            Command::OpenLog => "OpenLog",
            Command::OpenMain => "OpenMain",
            Command::OpenSettings => "OpenSettings",
            Command::ExitApp => "ExitApp",
            Command::HideTray => "HideTray",
        }
    }
}

/// Opt-in verification reports are accepted only by the sandboxed harness;
/// they are never part of the normal command vocabulary.
pub fn is_verify_report(text: &str) -> bool {
    let Ok(value) = serde_json::from_str::<Value>(text) else { return false };
    value.get("v").and_then(Value::as_u64) == Some(1)
        && value.get("type").and_then(Value::as_str) == Some("VerifyReport")
}

/// The verify page's per-path overlay check. Like the other reports it is only
/// accepted in the sandboxed verifier and is never a command.
pub fn is_verify_overlay(text: &str) -> bool {
    let Ok(value) = serde_json::from_str::<Value>(text) else { return false };
    value.get("v").and_then(Value::as_u64) == Some(1)
        && value.get("type").and_then(Value::as_str) == Some("VerifyOverlay")
}

/// The `--stress` harness posts its verdict through the same bridge it just
/// tortured; like the other verify reports it is not a command.
pub fn is_stress_report(text: &str) -> bool {
    let Ok(value) = serde_json::from_str::<Value>(text) else { return false };
    value.get("v").and_then(Value::as_u64) == Some(1)
        && value.get("type").and_then(Value::as_str) == Some("StressReport")
}

pub fn is_fx_verify_report(text: &str) -> bool {
    let Ok(value) = serde_json::from_str::<Value>(text) else { return false };
    value.get("v").and_then(Value::as_u64) == Some(1)
        && value.get("type").and_then(Value::as_str) == Some("FxVerifyReport")
}

pub fn parse(text: &str) -> Option<Command> {
    let raw: RawEnvelope = match serde_json::from_str(text) {
        Ok(v) => v,
        Err(err) => {
            crate::logging::log(&format!("ipc: malformed envelope ({err})"));
            return None;
        }
    };
    if raw.v != 1 {
        crate::logging::log(&format!("ipc: unsupported protocol version {}", raw.v));
        return None;
    }
    let command = match raw.kind.as_str() {
        "Ready" => Command::Ready,
        "SetTab" => Command::SetTab { tab: p::<TabPayload>(raw.payload)?.tab },
        "ToggleMaster" => Command::ToggleMaster { module: p::<ModulePayload>(raw.payload)?.module },
        "UpdateCrosshair" => Command::UpdateCrosshair { crosshair: p::<CrosshairPayload>(raw.payload)?.crosshair },
        "UpdateDisplay" => Command::UpdateDisplay { display: p::<DisplayPayload>(raw.payload)?.display },
        "CompareDisplay" => Command::CompareDisplay { active: p::<ComparePayload>(raw.payload)?.active },
        "CompareSound" => Command::CompareSound { active: p::<ComparePayload>(raw.payload)?.active },
        "UpdateSound" => Command::UpdateSound { audio: p::<AudioPayload>(raw.payload)?.audio },
        "SavePreset" => {
            let v = p::<SavePresetPayload>(raw.payload)?;
            Command::SavePreset { module: v.module, name: v.name, image_mode: v.image_mode }
        }
        "DeletePreset" => {
            let v = p::<DeletePresetPayload>(raw.payload)?;
            Command::DeletePreset { module: v.module, id: v.id }
        }
        "AssignHotkey" => {
            let v = p::<AssignHotkeyPayload>(raw.payload)?;
            Command::AssignHotkey { slot_id: v.slot_id, hotkey: v.hotkey }
        }
        "ClearHotkey" => Command::ClearHotkey { slot_id: p::<SlotIdPayload>(raw.payload)?.slot_id },
        "AssignModuleHotkey" => {
            let v = p::<AssignModuleHotkeyPayload>(raw.payload)?;
            Command::AssignModuleHotkey { module: v.module, hotkey: v.hotkey }
        }
        "ClearModuleHotkey" => Command::ClearModuleHotkey { module: p::<ModulePayload>(raw.payload)?.module },
        "ImportImage" => Command::ImportImage { path: p::<PathPayload>(raw.payload)?.path },
        "RefreshRecentImages" => Command::RefreshRecentImages,
        "UseStoredImage" => Command::UseStoredImage { name: p::<ImageNamePayload>(raw.payload)?.name },
        "DeleteStoredImage" => Command::DeleteStoredImage { name: p::<ImageNamePayload>(raw.payload)?.name },
        "RemoveImage" => Command::RemoveImage,
        "ImportEq" => {
            let v = p::<ImportEqPayload>(raw.payload)?;
            Command::ImportEq { path: v.path, text: v.text, name: v.name }
        }
        "FlattenEqImport" => Command::FlattenEqImport,
        "ClearEqImport" => Command::ClearEqImport,
        "BackupModalOpen" => Command::BackupModalOpen { open: p::<BackupModalPayload>(raw.payload)?.open },
        "CreateBackup" => Command::CreateBackup,
        "BrowseBackup" => Command::BrowseBackup,
        "RestoreBackup" => Command::RestoreBackup,
        "UndoRestore" => Command::UndoRestore,
        "SelectSlot" => Command::SelectSlot { id: p::<OptionalIdPayload>(raw.payload)?.id },
        "SaveSlot" => Command::SaveSlot { slot: p::<SlotPayload>(raw.payload)?.slot },
        "ActivateSlot" => {
            let v = p::<ActivateSlotPayload>(raw.payload)?;
            Command::ActivateSlot { id: v.id, slot: v.slot }
        }
        "DeleteSlot" => Command::DeleteSlot { id: p::<SlotIdPayload>(raw.payload)?.slot_id },
        "BrowseSlotExe" => Command::BrowseSlotExe { slot_id: p::<SlotIdPayload>(raw.payload)?.slot_id },
        "ScanGames" => Command::ScanGames,
        "SettingsChanged" => Command::SettingsChanged { settings: p::<SettingsPayload>(raw.payload)?.settings },
        "SetupAudio" => Command::SetupAudio,
        "RepairAudio" => Command::RepairAudio,
        "RepairManagedPreset" => Command::RepairManagedPreset,
        "UnstickAudio" => Command::UnstickAudio,
        "OpenFxSound" => Command::OpenFxSound,
        "OpenFxSoundPresets" => Command::OpenFxSoundPresets,
        "OpenFxSoundDownload" => Command::OpenFxSoundDownload,
        "OpenWindowsDisplaySettings" => Command::OpenWindowsDisplaySettings,
        "OpenDataFolder" => Command::OpenDataFolder,
        "ResetDisplay" => Command::ResetDisplay,
        "FactoryReset" => Command::FactoryReset,
        "OpenLog" => Command::OpenLog,
        "OpenMain" => Command::OpenMain,
        "OpenSettings" => Command::OpenSettings,
        "ExitApp" => Command::ExitApp,
        "HideTray" => Command::HideTray,
        other => {
            crate::logging::log(&format!("ipc: unknown command '{other}' ignored"));
            return None;
        }
    };
    Some(command)
}

#[derive(Deserialize)]
struct TabPayload { tab: String }
#[derive(Deserialize)]
struct ModulePayload { module: String }
#[derive(Deserialize)]
struct CrosshairPayload { crosshair: CrosshairConfig }
#[derive(Deserialize)]
struct DisplayPayload { display: DisplayConfig }
#[derive(Deserialize)]
struct ComparePayload { active: bool }
#[derive(Deserialize)]
struct AudioPayload { audio: AudioConfig }
#[derive(Deserialize)]
struct SavePresetPayload {
    module: String,
    name: String,
    #[serde(default, rename = "imageMode")]
    image_mode: bool,
}
#[derive(Deserialize)]
struct ImageNamePayload { name: String }
#[derive(Deserialize)]
struct DeletePresetPayload { module: String, id: String }
#[derive(Deserialize)]
struct AssignHotkeyPayload { #[serde(rename = "slotId")] slot_id: String, hotkey: String }
#[derive(Deserialize)]
struct AssignModuleHotkeyPayload { module: String, hotkey: String }
#[derive(Deserialize)]
struct SlotIdPayload { #[serde(rename = "slotId")] slot_id: String }
#[derive(Deserialize)]
struct PathPayload { #[serde(default)] path: Option<String> }
#[derive(Deserialize)]
struct ImportEqPayload { #[serde(default)] path: Option<String>, #[serde(default)] text: Option<String>, #[serde(default)] name: Option<String> }
#[derive(Deserialize)]
struct BackupModalPayload { open: bool }
#[derive(Deserialize)]
struct OptionalIdPayload { #[serde(default)] id: Option<String> }
#[derive(Deserialize)]
struct ActivateSlotPayload { #[serde(default)] id: Option<String>, #[serde(default)] slot: Option<Slot> }
#[derive(Deserialize)]
struct SlotPayload { slot: Slot }
#[derive(Deserialize)]
struct SettingsPayload { settings: Settings }

pub const IPC_BOOTSTRAP: &str = r#"
window.__crosshair_onMessage = window.__crosshair_onMessage || function () {};
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stored_image_result_serializes_for_the_ui() {
        let script = to_script(Event::StoredImageResult { name: "1700-gone.png".into(), ok: false });
        assert!(script.contains("\"type\":\"StoredImageResult\""), "{script}");
        assert!(script.contains("\"name\":\"1700-gone.png\""), "{script}");
        assert!(script.contains("\"ok\":false"), "{script}");
    }

    #[test]
    fn recent_images_serialize_for_the_ui() {
        let script = to_script(Event::RecentImages {
            images: vec![StoredImage { label: "aim.png".into(), name: "1700-aim.png".into() }],
        });
        assert!(script.contains("\"type\":\"RecentImages\""), "{script}");
        assert!(script.contains("\"label\":\"aim.png\""), "{script}");
        assert!(script.contains("\"name\":\"1700-aim.png\""), "{script}");
    }

    #[test]
    fn parses_every_plan_command() {
        let commands = [
            (r#"{"v":1,"type":"Ready"}"#, "Ready"),
            (r#"{"v":1,"type":"SetTab","payload":{"tab":"audio"}}"#, "SetTab"),
            (r#"{"v":1,"type":"ToggleMaster","payload":{"module":"display"}}"#, "ToggleMaster"),
            (r#"{"v":1,"type":"UpdateCrosshair","payload":{"crosshair":{}}}"#, "UpdateCrosshair"),
            (r#"{"v":1,"type":"UpdateDisplay","payload":{"display":{}}}"#, "UpdateDisplay"),
            (r#"{"v":1,"type":"CompareDisplay","payload":{"active":true}}"#, "CompareDisplay"),
            (r#"{"v":1,"type":"CompareSound","payload":{"active":true}}"#, "CompareSound"),
            (r#"{"v":1,"type":"UpdateSound","payload":{"audio":{}}}"#, "UpdateSound"),
            (r#"{"v":1,"type":"SavePreset","payload":{"module":"audio","name":"Mine"}}"#, "SavePreset"),
            (r#"{"v":1,"type":"SavePreset","payload":{"module":"crosshair","name":"Image","imageMode":true}}"#, "SavePresetImageMode"),
            (r#"{"v":1,"type":"DeletePreset","payload":{"module":"audio","id":"x"}}"#, "DeletePreset"),
            (r#"{"v":1,"type":"AssignHotkey","payload":{"slotId":"slot-fps","hotkey":"F12"}}"#, "AssignHotkey"),
            (r#"{"v":1,"type":"ClearHotkey","payload":{"slotId":"slot-fps"}}"#, "ClearHotkey"),
            (r#"{"v":1,"type":"AssignModuleHotkey","payload":{"module":"display","hotkey":"Ctrl+Shift+1"}}"#, "AssignModuleHotkey"),
            (r#"{"v":1,"type":"ClearModuleHotkey","payload":{"module":"display"}}"#, "ClearModuleHotkey"),
            (r#"{"v":1,"type":"ImportImage","payload":{"path":null}}"#, "ImportImage"),
            (r#"{"v":1,"type":"RefreshRecentImages"}"#, "RefreshRecentImages"),
            (r#"{"v":1,"type":"UseStoredImage","payload":{"name":"1700-aim.png"}}"#, "UseStoredImage"),
            (r#"{"v":1,"type":"DeleteStoredImage","payload":{"name":"1700-aim.png"}}"#, "DeleteStoredImage"),
            (r#"{"v":1,"type":"RemoveImage"}"#, "RemoveImage"),
            (r#"{"v":1,"type":"ImportEq","payload":{"path":null,"text":null}}"#, "ImportEq"),
            (r#"{"v":1,"type":"FlattenEqImport"}"#, "FlattenEqImport"),
            (r#"{"v":1,"type":"ClearEqImport"}"#, "ClearEqImport"),
            (r#"{"v":1,"type":"BackupModalOpen","payload":{"open":true}}"#, "BackupModalOpen"),
            (r#"{"v":1,"type":"CreateBackup"}"#, "CreateBackup"),
            (r#"{"v":1,"type":"BrowseBackup"}"#, "BrowseBackup"),
            (r#"{"v":1,"type":"RestoreBackup"}"#, "RestoreBackup"),
            (r#"{"v":1,"type":"UndoRestore"}"#, "UndoRestore"),
            (r#"{"v":1,"type":"SelectSlot","payload":{"id":"slot-fps"}}"#, "SelectSlot"),
            (r#"{"v":1,"type":"SaveSlot","payload":{"slot":{}}}"#, "SaveSlot"),
            (r#"{"v":1,"type":"ActivateSlot","payload":{"id":"slot-fps"}}"#, "ActivateSlot"),
            (r#"{"v":1,"type":"ActivateSlot","payload":{"id":"slot-fps","slot":{"id":"slot-fps","name":"FPS"}}}"#, "ActivateSlotWithSave"),
            (r#"{"v":1,"type":"DeleteSlot","payload":{"slotId":"slot-fps"}}"#, "DeleteSlot"),
            (r#"{"v":1,"type":"BrowseSlotExe","payload":{"slotId":"slot-fps"}}"#, "BrowseSlotExe"),
            (r#"{"v":1,"type":"SettingsChanged","payload":{"settings":{}}}"#, "SettingsChanged"),
            (r#"{"v":1,"type":"SetupAudio"}"#, "SetupAudio"),
            (r#"{"v":1,"type":"RepairAudio"}"#, "RepairAudio"),
            (r#"{"v":1,"type":"RepairManagedPreset"}"#, "RepairManagedPreset"),
            (r#"{"v":1,"type":"UnstickAudio"}"#, "UnstickAudio"),
            (r#"{"v":1,"type":"OpenFxSound"}"#, "OpenFxSound"),
            (r#"{"v":1,"type":"OpenFxSoundPresets"}"#, "OpenFxSoundPresets"),
            (r#"{"v":1,"type":"OpenFxSoundDownload"}"#, "OpenFxSoundDownload"),
            (r#"{"v":1,"type":"OpenWindowsDisplaySettings"}"#, "OpenWindowsDisplaySettings"),
            (r#"{"v":1,"type":"OpenDataFolder"}"#, "OpenDataFolder"),
            (r#"{"v":1,"type":"ResetDisplay"}"#, "ResetDisplay"),
            (r#"{"v":1,"type":"FactoryReset"}"#, "FactoryReset"),
            (r#"{"v":1,"type":"OpenLog"}"#, "OpenLog"),
        ];
        for (json, name) in commands {
            assert!(parse(json).is_some(), "command did not parse: {name}");
        }
    }

    #[test]
    fn rejects_wrong_versions_and_unknown_commands() {
        assert!(parse(r#"{"v":2,"type":"Ready"}"#).is_none());
        assert!(parse(r#"{"v":1,"type":"NotACommand"}"#).is_none());
        assert!(parse("not json").is_none());
        assert!(is_verify_report(r#"{"v":1,"type":"VerifyReport","payload":{"ok":true}}"#));
        assert!(is_stress_report(r#"{"v":1,"type":"StressReport","payload":{"ok":true}}"#));
        assert!(!is_stress_report(r#"{"v":1,"type":"VerifyReport","payload":{"ok":true}}"#));
        assert!(is_fx_verify_report(r#"{"v":1,"type":"FxVerifyReport","payload":{"ok":true}}"#));
        assert!(!is_fx_verify_report(r#"{"v":1,"type":"VerifyReport","payload":{"ok":true}}"#));
        assert!(!is_verify_report(r#"{"v":2,"type":"VerifyReport"}"#));
        assert!(!is_verify_report(r#"{"v":1,"type":"Ready"}"#));
        assert!(is_verify_overlay(r#"{"v":1,"type":"VerifyOverlay","payload":{"path":"use"}}"#));
        assert!(!is_verify_overlay(r#"{"v":1,"type":"VerifyReport"}"#));
        assert!(!is_verify_overlay(r#"{"v":2,"type":"VerifyOverlay"}"#));
    }

    #[test]
    fn envelope_filtering_rejects_missing_version_and_bad_payloads() {
        assert!(parse(r#"{"type":"Ready"}"#).is_none(), "missing version was accepted");
        assert!(parse(r#"{"v":1.0,"type":"Ready"}"#).is_none(), "float version was accepted");
        assert_eq!(Command::SetTab { tab: "slots".into() }.name(), "SetTab");
        assert_eq!(Command::ActivateSlot { id: None, slot: None }.name(), "ActivateSlot");
        assert!(parse(r#"{"v":1,"type":"SetTab","payload":{}}"#).is_none(), "missing payload field was accepted");
        assert!(parse(r#"{"v":1,"type":"SetTab","payload":{"tab":9}}"#).is_none(), "wrong payload type was accepted");
        assert!(parse(r#"{"v":1,"id":"cmd-7","type":"Ready"}"#).is_some(), "optional id was rejected");
        assert!(parse(r#"{"v":1,"type":"VerifyReport","payload":{"ok":true}}"#).is_none(), "verification report leaked into commands");
        assert!(is_verify_report(r#"{"v":1,"id":"verify-1","type":"VerifyReport","payload":{"ok":true}}"#));
        assert!(!is_verify_report(r#"{"type":"VerifyReport"}"#), "missing version was treated as a report");
        assert!(!is_verify_report(r#"{"v":1,"type":"Ready"}"#));
    }

    #[test]
    fn preview_tick_is_the_only_event_payload() {
        let script = to_script(Event::PreviewTick { v: 7 });
        assert!(script.contains(r#""type":"PreviewTick","payload":{"v":7}"#));
        assert!(!script.contains(r#""id""#));
    }

    #[test]
    fn ui_has_no_deferred_phase_placeholders() {
        let html = include_str!("../ui/index.html");
        let js = include_str!("../ui/app.js");
        assert!(!html.contains("Initializing in later phase"));
        assert!(!html.contains("later phase"));
        assert!(!html.contains("base64"));
        assert!(!js.contains("base64"));
        assert_eq!(html.matches("<button class=\"tab").count(), 4);
        assert!(html.contains("id=\"settingsModal\""));
        assert!(js.contains("SelectSlot"));
        assert!(js.contains("ToggleMaster"));
    }
}
