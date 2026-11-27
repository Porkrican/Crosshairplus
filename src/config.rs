use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

pub const APP_VERSION: &str = "1.0.0";
pub const CONFIG_VERSION: u32 = 2;
/// Crosshair preset IDs from older versions that `normalize` rewrites to the
/// current built-in. A restored preset must avoid them too, or its slot would
/// be silently repointed at the built-in after the restore.
pub(crate) const RETIRED_PRESET_IDS: [&str; 2] = ["style_casual", "style_hairline"];
pub const MAX_SLOT_NAME: usize = 24;
pub const MAX_SLOTS: usize = 6;
/// Imported crosshair images are copied into the data folder, so a stray huge
/// file must be refused before it reaches the disk instead of after.
pub const MAX_IMPORT_BYTES: u64 = 20 * 1024 * 1024;
pub const BAND_FREQS: [f32; 10] = [31.0, 62.0, 125.0, 250.0, 500.0, 1000.0, 2000.0, 4000.0, 8000.0, 16000.0];
/// Extensions accepted by both the importer and the Recent image picker.
pub const IMAGE_EXTENSIONS: [&str; 8] = ["png", "jpg", "jpeg", "webp", "gif", "svg", "bmp", "ico"];

/// Optional isolation root used only by the opt-in preset-delete verification
/// run. Normal launches never set this variable.
pub fn sandbox_root() -> Option<PathBuf> {
    std::env::var_os("CROSSHAIRPLUS_SANDBOX_DIR").filter(|value| !value.is_empty()).map(PathBuf::from)
}
pub fn data_dir() -> PathBuf {
    if let Some(root) = sandbox_root() {
        return root.join("data");
    }
    live_data_dir()
}
/// The real user data folder, even inside a sandboxed verification run. The
/// read-only FxSound verifier compares the live audio config with
/// `status.json`, so it must not read the sandbox copy or its defaults.
pub fn live_data_dir() -> PathBuf {
    let appdata = std::env::var("APPDATA").unwrap_or_else(|_| ".".into());
    PathBuf::from(appdata).join("CrosshairPlus")
}
pub fn live_config_path() -> PathBuf {
    live_data_dir().join("config.json")
}
pub fn config_path() -> PathBuf {
    data_dir().join("config.json")
}
/// Derived cache of the last installed-games scan. Never part of a backup or
/// the config; deleting it only costs one scan.
pub fn games_cache_path() -> PathBuf {
    data_dir().join("games.json")
}
pub fn crosshairs_dir() -> PathBuf {
    data_dir().join("crosshairs")
}
/// One image already stored in the data folder. `name` is the on-disk file
/// name; `label` is the original import name with the timestamp prefix gone.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct StoredImage {
    pub name: String,
    pub label: String,
}

fn stored_image_extension(name: &str) -> Option<String> {
    Path::new(name).extension().and_then(|value| value.to_str()).map(str::to_ascii_lowercase)
}

/// A stored name must be a single, plain file name with a supported image
/// extension. Anything that could escape the crosshairs folder is refused.
fn stored_image_name_safe(name: &str) -> bool {
    let mut parts = Path::new(name).components();
    matches!(parts.next(), Some(std::path::Component::Normal(_)))
        && parts.next().is_none()
        && Path::new(name).file_name().and_then(|value| value.to_str()) == Some(name)
        && !name.contains([':', '\0'])
        && stored_image_extension(name).is_some_and(|ext| IMAGE_EXTENSIONS.contains(&ext.as_str()))
}

/// The original import name, without the millisecond stamp that keeps two
/// imports of the same file from overwriting each other.
pub fn stored_image_label(name: &str) -> String {
    let stripped = name
        .split_once('-')
        .filter(|(stamp, rest)| {
            stamp.len() >= 10 && stamp.chars().all(|ch| ch.is_ascii_digit()) && !rest.is_empty()
        })
        .map(|(_, rest)| rest)
        .unwrap_or(name);
    stripped.to_string()
}

pub fn stored_image_path_in(dir: &Path, name: &str) -> Option<PathBuf> {
    if !stored_image_name_safe(name) {
        return None;
    }
    let path = dir.join(name);
    match fs::metadata(&path) {
        Ok(meta) if meta.is_file() => Some(path),
        _ => None,
    }
}

pub fn recent_images_in(dir: &Path) -> Vec<StoredImage> {
    let Ok(entries) = fs::read_dir(dir) else { return Vec::new() };
    let mut images: Vec<(u64, StoredImage)> = Vec::new();
    for entry in entries.flatten() {
        let Ok(name) = entry.file_name().into_string() else { continue };
        if !stored_image_name_safe(&name) {
            continue;
        }
        let Ok(meta) = entry.metadata() else { continue };
        if !meta.is_file() {
            continue;
        }
        let modified = meta
            .modified()
            .ok()
            .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|duration| duration.as_millis().min(u64::MAX as u128) as u64)
            .unwrap_or(0);
        images.push((modified, StoredImage { label: stored_image_label(&name), name }));
    }
    images.sort_by(|left, right| right.0.cmp(&left.0).then_with(|| left.1.name.cmp(&right.1.name)));
    images.into_iter().map(|(_, image)| image).collect()
}

pub fn recent_crosshair_images() -> Vec<StoredImage> {
    recent_images_in(&crosshairs_dir())
}

/// Point the live crosshair at a stored file only when it still exists, so a
/// deletion outside the app can never leave a dead path in the config.
pub fn apply_stored_image_in(dir: &Path, crosshair: &mut CrosshairConfig, name: &str) -> Result<(), String> {
    let path = stored_image_path_in(dir, name).ok_or_else(|| {
        "That image is no longer in the crosshairs folder. Import it again or pick another one.".to_string()
    })?;
    crosshair.image = Some(path.to_string_lossy().to_string());
    crosshair.image_name = Some(stored_image_label(name));
    Ok(())
}

pub fn apply_stored_image(crosshair: &mut CrosshairConfig, name: &str) -> Result<(), String> {
    apply_stored_image_in(&crosshairs_dir(), crosshair, name)
}

fn image_path_matches(current: &str, path: &Path) -> bool {
    Path::new(current) == path || current.eq_ignore_ascii_case(&path.to_string_lossy())
}

/// Remove a stored file and every reference to it: the live crosshair image and
/// any preset saved from that image. A preset loses only its image block, so it
/// still applies as a shape preset instead of restoring a path that no longer
/// exists, and the active marker is dropped when it named that preset. Returns
/// true when the file was the live image, so the caller repaints and can leave
/// the vector fallback clean.
pub fn delete_stored_image_in(dir: &Path, config: &mut Config, name: &str) -> Result<bool, String> {
    let path = stored_image_path_in(dir, name).ok_or_else(|| {
        "That image is no longer in the crosshairs folder.".to_string()
    })?;
    fs::remove_file(&path).map_err(|err| format!("Could not delete that image: {err}"))?;
    let active_preset = config.crosshair.active_preset.clone();
    let mut cleared_active_preset = false;
    for preset in &mut config.custom.crosshair {
        let matches = preset
            .config
            .image
            .as_deref()
            .is_some_and(|current| image_path_matches(current, &path));
        if !matches {
            continue;
        }
        if active_preset.as_deref() == Some(preset.id.as_str()) {
            cleared_active_preset = true;
        }
        preset.config.image = None;
        preset.config.image_name = None;
    }
    if cleared_active_preset {
        config.crosshair.active_preset = None;
    }
    let active = config
        .crosshair
        .image
        .as_deref()
        .is_some_and(|current| image_path_matches(current, &path));
    if active {
        config.crosshair.image = None;
        config.crosshair.image_name = None;
    }
    Ok(active)
}

pub fn delete_stored_image(config: &mut Config, name: &str) -> Result<bool, String> {
    delete_stored_image_in(&crosshairs_dir(), config, name)
}

/// Drop the live image when it points at the stored file named here and that
/// file is gone, so a failed pick or delete cannot leave a dead path behind.
/// A file that still exists is left alone (a failed delete may be a lock, not a
/// missing image). Returns true when the crosshair changed.
pub fn clear_vanished_live_image_in(dir: &Path, crosshair: &mut CrosshairConfig, name: &str) -> bool {
    if !stored_image_name_safe(name) {
        return false;
    }
    let Some(current) = crosshair.image.as_deref() else {
        return false;
    };
    let path = dir.join(name);
    // A still-present file must survive even when the delete itself failed
    // (a lock or denied permission is not a vanished image).
    if path.is_file() || !image_path_matches(current, &path) {
        return false;
    }
    crosshair.image = None;
    crosshair.image_name = None;
    true
}

pub fn clear_vanished_live_image(crosshair: &mut CrosshairConfig, name: &str) -> bool {
    clear_vanished_live_image_in(&crosshairs_dir(), crosshair, name)
}

/// Drop a live image whose file no longer exists. Restoring a slot snapshot or
/// skipping a preset image must not keep a dead path alive when the current
/// picture was the deleted file too. Returns the display name it removed.
pub fn clear_missing_live_image(crosshair: &mut CrosshairConfig) -> Option<String> {
    let path = crosshair.image.as_deref()?.trim();
    if path.is_empty() || Path::new(path).is_file() {
        return None;
    }
    let name = crosshair
        .image_name
        .as_deref()
        .filter(|name| !name.trim().is_empty())
        .map(str::to_string)
        .or_else(|| Path::new(path).file_name().and_then(|name| name.to_str()).map(str::to_string))
        .unwrap_or_else(|| "The image".to_string());
    crosshair.image = None;
    crosshair.image_name = None;
    Some(name)
}
pub fn webview_dir() -> PathBuf {
    if let Some(root) = sandbox_root() {
        return root.join("webview");
    }
    let local = std::env::var("LOCALAPPDATA").unwrap_or_else(|_| data_dir().to_string_lossy().into());
    PathBuf::from(local).join("CrosshairPlus").join("WebView2")
}
pub fn log_path() -> PathBuf {
    data_dir().join("crosshair.log")
}
pub fn ensure_dirs() {
    for (label, path) in [("data", data_dir()), ("crosshairs", crosshairs_dir()), ("webview", webview_dir())] {
        if let Err(err) = fs::create_dir_all(&path) {
            crate::logging::log(&format!("config: cannot create {label} directory {}: {err}", path.display()));
        }
    }
}

fn valid_hex_color(value: &str) -> bool {
    value.len() == 7 && value.starts_with('#') && value[1..].chars().all(|ch| ch.is_ascii_hexdigit())
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct Settings {
    pub start_with_windows: bool,
    pub start_minimized: bool,
    pub minimize_to_tray: bool,
    pub restore_display_on_exit: bool,
    pub hotkeys_enabled: bool,
    pub auto_revert_slots: bool,
    pub preview_day: bool,
    /// Independent module toggles. Empty means unassigned. Aim keeps the
    /// historical F8 default; display and sound start unassigned so they never
    /// steal a game key without the user choosing one.
    #[serde(default = "default_aim_hotkey")]
    pub aim_hotkey: String,
    #[serde(default)]
    pub display_hotkey: String,
    #[serde(default)]
    pub audio_hotkey: String,
    /// Escape hatch for a calibration that hides the screen: restores the OS
    /// ramps and switches the Display module off without opening the window.
    #[serde(default)]
    pub restore_hotkey: String,
    /// One key that walks the slot list and finally switches every module off.
    #[serde(default)]
    pub cycle_hotkey: String,
    /// Show the in-game notification pill for hotkey and slot changes.
    pub osd_toast: bool,
}
fn default_aim_hotkey() -> String {
    "F8".into()
}
impl Default for Settings {
    fn default() -> Self {
        Self {
            start_with_windows: false,
            start_minimized: false,
            minimize_to_tray: true,
            restore_display_on_exit: true,
            hotkeys_enabled: true,
            auto_revert_slots: true,
            preview_day: true,
            aim_hotkey: default_aim_hotkey(),
            display_hotkey: String::new(),
            audio_hotkey: String::new(),
            restore_hotkey: String::new(),
            cycle_hotkey: String::new(),
            osd_toast: true,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct CrosshairConfig {
    pub enabled: bool,
    pub style: String,
    pub size: f32,
    pub thickness: f32,
    pub gap: f32,
    pub rotation: f32,
    pub dot: bool,
    pub dot_size: f32,
    pub outline: bool,
    pub outline_width: f32,
    pub opacity: f32,
    pub color: String,
    pub image: Option<String>,
    pub image_name: Option<String>,
    pub image_scale: f32,
    pub image_rotation: f32,
    pub image_invert: bool,
    pub image_solid: bool,
    pub image_tint: bool,
    pub image_solid_color: String,
    pub gif_speed: f32,
    pub gif_playing: bool,
    pub monitor: Option<String>,
    pub offset_x: i32,
    pub offset_y: i32,
    pub hide_in_menus: bool,
    pub hide_on_rmb: bool,
    /// `always`, `hide` (while aiming) or `only` (only while aiming).
    /// `None` means a pre-3-way config; normalize derives it from hide_on_rmb.
    #[serde(default)]
    pub aim_visibility: Option<String>,
    /// Mouse button held for `hide`/`only`: right (default), left, middle,
    /// mouse4 or mouse5.
    #[serde(default)]
    pub aim_button: Option<String>,
    pub active_preset: Option<String>,
}
impl Default for CrosshairConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            style: "plus".into(),
            size: 18.0,
            thickness: 2.0,
            gap: 4.0,
            rotation: 0.0,
            dot: false,
            dot_size: 2.0,
            outline: true,
            outline_width: 1.0,
            opacity: 100.0,
            color: "#4EA8FF".into(),
            image: None,
            image_name: None,
            image_scale: 100.0,
            image_rotation: 0.0,
            image_invert: false,
            image_solid: false,
            image_tint: false,
            image_solid_color: "#FFFFFF".into(),
            gif_speed: 1.0,
            gif_playing: true,
            monitor: None,
            offset_x: 0,
            offset_y: 0,
            hide_in_menus: false,
            hide_on_rmb: false,
            // Field-level serde default is None for legacy configs; a fresh
            // config is already in the explicit modern shape.
            aim_visibility: Some("always".into()),
            aim_button: Some("right".into()),
            active_preset: None,
        }
    }
}
impl CrosshairConfig {
    pub fn normalize(&mut self) {
        // Older configs only had hide_on_rmb. Preserve that behavior once, then
        // keep writing the legacy bool so a downgrade still reads the setting.
        let visibility = match self.aim_visibility.as_deref() {
            Some("always") => "always",
            Some("hide") => "hide",
            Some("only") => "only",
            _ => {
                if self.hide_on_rmb {
                    "hide"
                } else {
                    "always"
                }
            }
        };
        self.aim_visibility = Some(visibility.into());
        self.hide_on_rmb = visibility == "hide";
        let aim_button = match self.aim_button.as_deref() {
            Some("left") => "left",
            Some("middle") => "middle",
            Some("mouse4") => "mouse4",
            Some("mouse5") => "mouse5",
            _ => "right",
        };
        self.aim_button = Some(aim_button.into());
        let valid = ["plus", "dot", "ring", "tsight", "triangle", "peak"];
        if !valid.contains(&self.style.as_str()) {
            self.style = "plus".into();
        }
        self.size = self.size.clamp(2.0, 48.0);
        self.thickness = self.thickness.clamp(1.0, 10.0);
        self.gap = self.gap.clamp(0.0, 24.0);
        self.rotation = self.rotation.clamp(0.0, 360.0);
        self.dot_size = self.dot_size.clamp(1.0, 16.0);
        self.outline_width = self.outline_width.clamp(0.0, 6.0);
        self.opacity = self.opacity.clamp(10.0, 100.0);
        self.image_scale = self.image_scale.clamp(10.0, 300.0);
        self.image_rotation = self.image_rotation.clamp(0.0, 360.0);
        self.gif_speed = self.gif_speed.clamp(0.25, 2.0);
        self.offset_x = self.offset_x.clamp(-300, 300);
        self.offset_y = self.offset_y.clamp(-300, 300);
        if !valid_hex_color(&self.color) {
            self.color = "#4EA8FF".into();
        }
        if !valid_hex_color(&self.image_solid_color) {
            self.image_solid_color = "#FFFFFF".into();
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct DisplayValues {
    pub brightness: f32,
    pub contrast: f32,
    pub gamma: f32,
    pub warmth: f32,
    pub r: f32,
    pub g: f32,
    pub b: f32,
    pub panel: String,
}
impl Default for DisplayValues {
    fn default() -> Self {
        Self { brightness: 0.0, contrast: 0.0, gamma: 1.0, warmth: 0.0, r: 100.0, g: 100.0, b: 100.0, panel: "auto".into() }
    }
}
impl DisplayValues {
    pub fn normalize(&mut self) {
        self.brightness = self.brightness.clamp(-40.0, 40.0);
        self.contrast = self.contrast.clamp(-30.0, 40.0);
        self.gamma = self.gamma.clamp(0.50, 2.50);
        self.warmth = self.warmth.clamp(0.0, 80.0);
        self.r = self.r.clamp(50.0, 120.0);
        self.g = self.g.clamp(50.0, 120.0);
        self.b = self.b.clamp(50.0, 120.0);
        let panels = ["auto", "led", "oled", "ips", "va", "miniled", "tn"];
        if !panels.contains(&self.panel.as_str()) {
            self.panel = "auto".into();
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct DisplayConfig {
    pub enabled: bool,
    pub mode: String,
    pub customize_each: bool,
    pub selected_monitor: Option<String>,
    pub global: DisplayValues,
    pub monitors: HashMap<String, DisplayValues>,
    pub backlight: HashMap<String, u32>,
    pub restore_on_exit: bool,
    pub active_preset: Option<String>,
}
impl Default for DisplayConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            mode: "all".into(),
            customize_each: false,
            selected_monitor: None,
            global: DisplayValues::default(),
            monitors: HashMap::new(),
            backlight: HashMap::new(),
            restore_on_exit: true,
            active_preset: None,
        }
    }
}
impl DisplayConfig {
    pub fn normalize(&mut self) {
        if self.mode == "main_only" {
            self.mode = "this".into();
        }
        if self.mode != "this" && self.mode != "all" {
            self.mode = "all".into();
        }
        self.global.normalize();
        for v in self.monitors.values_mut() {
            v.normalize();
        }
    }
    pub fn values_for(&self, monitor_id: &str) -> DisplayValues {
        if self.customize_each {
            self.monitors.get(monitor_id).cloned().unwrap_or_else(|| self.global.clone())
        } else {
            self.global.clone()
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct ImportedBand {
    pub freq: f32,
    pub gain: f32,
}
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct ImportedProfile {
    pub name: String,
    pub raw: String,
    pub bands: Vec<ImportedBand>,
    pub parametric: bool,
    pub preamp: f32,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct AudioConfig {
    pub enabled: bool,
    pub device: Option<String>,
    pub bands: [f32; 10],
    /// FxSound Bass Boost effect, 0..10.
    pub bass: f32,
    /// FxSound Clarity (Fidelity) effect, 0..10.
    pub treble: f32,
    /// FxSound master gain in dB, -20..20.
    pub output_level: f32,
    /// FxSound Ambience effect, 0..10. The historical field name is kept so
    /// existing configs migrate without a schema break.
    pub echo: f32,
    pub surround: f32,
    pub dynamic_boost: f32,
    pub volume_leveling: f32,
    pub balance: f32,
    pub filter_q: f32,
    /// Automatic anti-clip headroom for boosted EQ bands.
    pub volume_guard: bool,
    pub imported: Option<ImportedProfile>,
    pub active_preset: Option<String>,
}
impl Default for AudioConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            device: None,
            bands: [0.0; 10],
            bass: 0.0,
            treble: 0.0,
            output_level: 0.0,
            echo: 0.0,
            surround: 0.0,
            dynamic_boost: 0.0,
            volume_leveling: 0.0,
            balance: 0.0,
            filter_q: 1.0,
            volume_guard: true,
            imported: None,
            active_preset: None,
        }
    }
}
impl AudioConfig {
    pub fn normalize(&mut self) -> Vec<String> {
        // The config file is hand-editable, so an imported profile read from it
        // gets the same validation a restored one gets: no non-finite or
        // out-of-range value may reach the curve. The returned messages say what
        // had to be cleaned up, so a load can explain a vanished curve.
        let mut warnings = Vec::new();
        self.imported = crate::audio::apo_import::sanitize_imported(self.imported.take(), &mut warnings);
        for b in &mut self.bands {
            *b = b.clamp(-12.0, 12.0);
        }
        self.bass = self.bass.clamp(0.0, 10.0);
        self.treble = self.treble.clamp(0.0, 10.0);
        self.output_level = self.output_level.clamp(-20.0, 20.0);
        self.echo = self.echo.clamp(0.0, 10.0);
        self.surround = self.surround.clamp(0.0, 10.0);
        self.dynamic_boost = self.dynamic_boost.clamp(0.0, 10.0);
        self.volume_leveling = self.volume_leveling.clamp(0.0, 4.0);
        self.balance = self.balance.clamp(-20.0, 20.0);
        self.filter_q = self.filter_q.clamp(1.0, 3.0);
        warnings
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct SlotModule {
    pub on: bool,
    pub preset: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct Slot {
    pub id: String,
    pub name: String,
    pub emoji: String,
    pub hotkey: Option<String>,
    pub target_exe: Option<String>,
    pub crosshair: SlotModule,
    pub display: SlotModule,
    pub audio: SlotModule,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct SlotSnapshot {
    pub crosshair: CrosshairConfig,
    pub display: DisplayConfig,
    pub audio: AudioConfig,
    pub crosshair_was_on: bool,
    pub display_was_on: bool,
    pub audio_was_on: bool,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct SlotsConfig {
    pub list: Vec<Slot>,
    pub active: Option<String>,
    pub baseline: Option<SlotSnapshot>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct CrosshairPreset {
    pub id: String,
    pub name: String,
    pub builtin: bool,
    pub config: CrosshairConfig,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct DisplayPreset {
    pub id: String,
    pub name: String,
    pub builtin: bool,
    pub values: DisplayValues,
}

impl DisplayPreset {
    /// Copy only the calibration fields. The monitor's panel choice is never
    /// part of a preset, so applying one must not reset it to a default.
    pub fn apply_to(&self, values: &mut DisplayValues) {
        values.brightness = self.values.brightness;
        values.contrast = self.values.contrast;
        values.gamma = self.values.gamma;
        values.warmth = self.values.warmth;
        values.r = self.values.r;
        values.g = self.values.g;
        values.b = self.values.b;
    }
}

impl CrosshairPreset {
    /// A preset saved from the image panel owns its image block. Built-in and
    /// vector presets own only the shape, so applying one keeps the user's
    /// current image exactly as before.
    pub fn image_mode(&self) -> bool {
        self.config.image.as_deref().is_some_and(|path| !path.trim().is_empty())
    }

    /// Apply the preset's shape and, for an image-mode custom preset, its image
    /// block. Monitor, visibility and enabled state always stay with the user.
    pub fn apply_to(&self, target: &mut CrosshairConfig) {
        let enabled = target.enabled;
        let monitor = target.monitor.clone();
        let hide_in_menus = target.hide_in_menus;
        // Keep the user's 3-way visibility choice, including a legacy config
        // that only has the old bool. Derive the compatibility bool back from
        // the choice so old builds keep reading the same behavior.
        let aim_visibility = target
            .aim_visibility
            .clone()
            .unwrap_or_else(|| if target.hide_on_rmb { "hide".into() } else { "always".into() });
        let aim_button = target.aim_button.clone().unwrap_or_else(|| "right".into());
        let keep_image = !self.image_mode();
        let image = target.image.clone();
        let image_name = target.image_name.clone();
        let image_scale = target.image_scale;
        let image_rotation = target.image_rotation;
        let image_invert = target.image_invert;
        let image_solid = target.image_solid;
        let image_tint = target.image_tint;
        let image_solid_color = target.image_solid_color.clone();
        let gif_speed = target.gif_speed;
        let gif_playing = target.gif_playing;
        *target = self.config.clone();
        target.enabled = enabled;
        target.monitor = monitor;
        target.hide_in_menus = hide_in_menus;
        target.aim_visibility = Some(aim_visibility.clone());
        target.hide_on_rmb = aim_visibility == "hide";
        target.aim_button = Some(aim_button);
        if keep_image {
            target.image = image;
            target.image_name = image_name;
            target.image_scale = image_scale;
            target.image_rotation = image_rotation;
            target.image_invert = image_invert;
            target.image_solid = image_solid;
            target.image_tint = image_tint;
            target.image_solid_color = image_solid_color;
            target.gif_speed = gif_speed;
            target.gif_playing = gif_playing;
        }
    }

    /// The preset's image display name when its file no longer exists, so a
    /// caller can keep the live image and explain why.
    pub fn missing_image(&self) -> Option<String> {
        let path = self.config.image.as_deref()?.trim();
        if path.is_empty() || Path::new(path).is_file() {
            return None;
        }
        Some(
            self.config
                .image_name
                .as_deref()
                .filter(|name| !name.trim().is_empty())
                .unwrap_or_else(|| {
                    Path::new(path)
                        .file_name()
                        .and_then(|name| name.to_str())
                        .unwrap_or("The preset image")
                })
                .to_string(),
        )
    }

    /// Like `apply_to`, but an image-mode preset whose file is gone applies its
    /// shape only and keeps the target's current image block. Returns the
    /// missing image's display name.
    pub fn apply_to_checked(&self, target: &mut CrosshairConfig) -> Option<String> {
        let missing = self.missing_image();
        if missing.is_some() {
            let mut shape_only = self.clone();
            shape_only.config.image = None;
            shape_only.apply_to(target);
            // Keeping a live image only helps while its file exists; a dead
            // live path would otherwise survive the skipped preset image.
            clear_missing_live_image(target);
        } else {
            self.apply_to(target);
        }
        missing
    }
}

/// Copy every image-related field, so a preset that cannot use its own image
/// leaves the user's current picture exactly as it was.
fn keep_image_block(from: &CrosshairConfig, target: &mut CrosshairConfig) {
    target.image = from.image.clone();
    target.image_name = from.image_name.clone();
    target.image_scale = from.image_scale;
    target.image_rotation = from.image_rotation;
    target.image_invert = from.image_invert;
    target.image_solid = from.image_solid;
    target.image_tint = from.image_tint;
    target.image_solid_color = from.image_solid_color.clone();
    target.gif_speed = from.gif_speed;
    target.gif_playing = from.gif_playing;
}

/// The page applies a preset by sending the whole crosshair config. When that
/// config names a preset whose own image file is gone, keep the live image
/// block instead of accepting the dead path. Returns the missing image's label.
pub fn guard_preset_image(
    current: &CrosshairConfig,
    incoming: &mut CrosshairConfig,
    preset: Option<&CrosshairPreset>,
) -> Option<String> {
    let preset = preset?;
    if preset.config.image.as_deref() != incoming.image.as_deref() {
        return None;
    }
    let missing = preset.missing_image()?;
    keep_image_block(current, incoming);
    clear_missing_live_image(incoming);
    Some(missing)
}

pub fn missing_preset_image_message(name: &str) -> String {
    format!(
        "{name} is no longer available, so the preset applied its shape without that image. Re-import it or pick a new one."
    )
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct AudioPreset {
    pub id: String,
    pub name: String,
    pub builtin: bool,
    pub bands: [f32; 10],
    pub bass: f32,
    pub treble: f32,
    pub output_level: f32,
    pub echo: f32,
    pub surround: f32,
    pub dynamic_boost: f32,
    pub volume_leveling: f32,
    pub balance: f32,
    pub filter_q: f32,
    pub volume_guard: bool,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct CustomPresets {
    pub crosshair: Vec<CrosshairPreset>,
    pub display: Vec<DisplayPreset>,
    pub audio: Vec<AudioPreset>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct Config {
    pub version: u32,
    pub settings: Settings,
    pub crosshair: CrosshairConfig,
    pub display: DisplayConfig,
    pub audio: AudioConfig,
    pub slots: SlotsConfig,
    pub custom: CustomPresets,
}
impl Default for Config {
    fn default() -> Self {
        Self {
            version: CONFIG_VERSION,
            settings: Settings::default(),
            crosshair: CrosshairConfig::default(),
            display: DisplayConfig::default(),
            audio: AudioConfig::default(),
            slots: default_slots(),
            custom: CustomPresets::default(),
        }
    }
}
impl Config {
    /// Bring older configs forward. Built-in presets live in code, so this only
    /// rewrites values that still exactly match a superseded built-in while that
    /// preset is active. Tweaked values, custom presets and slots are untouched.
    fn migrate(&mut self) {
        if self.version >= CONFIG_VERSION {
            return;
        }
        if let Some(id) = self.crosshair.active_preset.clone() {
            if let Some(old) = legacy_crosshair_presets().into_iter().find(|preset| preset.id == id) {
                if same_crosshair_values(&self.crosshair, &old.config) {
                    if let Some(new) =
                        builtin_crosshair_presets().into_iter().find(|preset| preset.id == id)
                    {
                        new.apply_to(&mut self.crosshair);
                        self.crosshair.active_preset = Some(id);
                    }
                }
            }
        }
        if let Some(id) = self.display.active_preset.clone() {
            if let Some(old) = legacy_display_presets().into_iter().find(|preset| preset.id == id) {
                if same_display_values(&self.display.global, &old.values) {
                    if let Some(new) =
                        builtin_display_presets().into_iter().find(|preset| preset.id == id)
                    {
                        new.apply_to(&mut self.display.global);
                    }
                }
                for values in self.display.monitors.values_mut() {
                    if same_display_values(values, &old.values) {
                        if let Some(new) =
                            builtin_display_presets().into_iter().find(|preset| preset.id == id)
                        {
                            new.apply_to(values);
                        }
                    }
                }
            }
        }
        if let Some(id) = self.audio.active_preset.clone() {
            if let Some(old) = legacy_audio_presets().into_iter().find(|preset| preset.id == id) {
                if same_audio_values(&self.audio, &old) {
                    if let Some(new) =
                        builtin_audio_presets().into_iter().find(|preset| preset.id == id)
                    {
                        self.audio.bands = new.bands;
                        self.audio.bass = new.bass;
                        self.audio.treble = new.treble;
                        self.audio.output_level = new.output_level;
                        self.audio.echo = new.echo;
                        self.audio.surround = new.surround;
                        self.audio.dynamic_boost = new.dynamic_boost;
                        self.audio.volume_leveling = new.volume_leveling;
                        self.audio.balance = new.balance;
                        self.audio.filter_q = new.filter_q;
                        self.audio.volume_guard = new.volume_guard;
                        self.audio.active_preset = Some(id);
                    }
                }
            }
        }
        self.version = CONFIG_VERSION;
    }

    /// Clamps every stored value into its supported range and returns what had
    /// to be cleaned up, so a load can explain a setting or curve disappearing.
    pub fn normalize(&mut self) -> Vec<String> {
        self.crosshair.normalize();
        self.display.normalize();
        let mut warnings = self.audio.normalize();
        // Crash recovery copies this snapshot into the live config *after* load
        // normalization, so its profile would otherwise reach the curve
        // unchecked. It is the only snapshot field the curve reads.
        if let Some(baseline) = &mut self.slots.baseline {
            warnings.extend(baseline.audio.normalize());
        }
        for warning in &warnings {
            crate::logging::log(&format!("config: {warning}"));
        }
        for preset in &mut self.custom.crosshair {
            preset.config.normalize();
        }
        let legacy_active = self
            .crosshair
            .active_preset
            .as_deref()
            .is_some_and(|id| RETIRED_PRESET_IDS.iter().any(|retired| *retired == id));
        if legacy_active {
            self.crosshair.active_preset = Some("style_plus".into());
        }
        self.slots.list.truncate(MAX_SLOTS);
        for slot in &mut self.slots.list {
            if slot
                .crosshair
                .preset
                .as_deref()
                .is_some_and(|id| RETIRED_PRESET_IDS.iter().any(|retired| *retired == id))
            {
                slot.crosshair.preset = Some("style_plus".into());
            }
            slot.name = slot.name.chars().take(MAX_SLOT_NAME).collect();
            if slot.name.trim().is_empty() {
                slot.name = "Slot".into();
            }
            if slot.emoji.is_empty() {
                slot.emoji = "⚡".into();
            }
        }
        if let Some(active) = &self.slots.active {
            if !self.slots.list.iter().any(|s| &s.id == active) {
                self.slots.active = None;
                self.slots.baseline = None;
            }
        }
        warnings
    }
    /// Loads the live config and returns the problems its content had to be
    /// cleaned of, so startup can explain them instead of only logging them.
    pub fn load_reporting() -> (Self, Vec<String>) {
        ensure_dirs();
        Self::load_from_reporting(&config_path())
    }
    pub fn load_from(path: &Path) -> Self {
        Self::load_from_reporting(path).0
    }
    pub fn load_from_reporting(path: &Path) -> (Self, Vec<String>) {
        let (config, warnings) = match fs::read_to_string(path) {
            Ok(text) => match Self::parse_normalized(&text) {
                Ok(parsed) => parsed,
                Err(err) => {
                    // Only the live config path belongs in the session log; a
                    // custom path (a test or an import) must not append to it.
                    let announce = path == config_path();
                    if announce {
                        crate::logging::log(&format!("config: parse failed ({err}); attempting recovery"));
                    }
                    let corrupt = corrupt_path(path);
                    let quarantine_kept = match fs::copy(path, &corrupt) {
                        Ok(_) => true,
                        Err(copy_err) => {
                            if announce {
                                crate::logging::log(&format!("config: could not preserve unreadable config: {copy_err}"));
                            }
                            false
                        }
                    };
                    let corrupt_name = corrupt
                        .file_name()
                        .map(|name| name.to_string_lossy().into_owned())
                        .unwrap_or_else(|| corrupt.display().to_string());
                    let backup = backup_path(path);
                    let backup_name = backup
                        .file_name()
                        .map(|name| name.to_string_lossy().into_owned())
                        .unwrap_or_else(|| backup.display().to_string());
                    let damage_note = if quarantine_kept {
                        format!(" The damaged file was kept as {corrupt_name}.")
                    } else {
                        " The damaged file could not be preserved.".to_string()
                    };
                    match fs::read_to_string(&backup) {
                        Ok(backup_text) => match Self::parse_normalized(&backup_text) {
                            Ok((recovered, mut recovered_warnings)) => {
                                if announce {
                                    crate::logging::log(&format!("config: recovered settings from {backup_name}"));
                                }
                                recovered_warnings.insert(
                                    0,
                                    format!("Settings were damaged; recovered the last saved copy from {backup_name}.{damage_note}"),
                                );
                                (recovered, recovered_warnings)
                            }
                            Err(backup_err) => {
                                if announce {
                                    crate::logging::log(&format!("config: backup {backup_name} could not be read ({backup_err}); starting defaults"));
                                }
                                (Config::default(), vec![format!(
                                    "Settings and their backup could not be read; started with default settings.{damage_note}"
                                )])
                            }
                        },
                        Err(backup_err) => {
                            if announce {
                                crate::logging::log(&format!("config: backup {backup_name} could not be read ({backup_err}); starting defaults"));
                            }
                            (Config::default(), vec![format!(
                                "Settings and their backup could not be read; started with default settings.{damage_note}"
                            )])
                        }
                    }
                }
            },
            Err(err) => {
                if err.kind() != std::io::ErrorKind::NotFound {
                    crate::logging::log(&format!("config: cannot read {}: {err}; starting defaults", path.display()));
                }
                (Config::default(), Vec::new())
            }
        };
        (config, warnings)
    }
    /// Parse a config file without falling back to defaults, so callers that
    /// must not silently lose content (restore undo) can report the problem.
    pub(crate) fn load_strict(path: &Path) -> Result<(Self, Vec<String>), String> {
        let text = fs::read_to_string(path).map_err(|err| format!("could not read {}: {err}", path.display()))?;
        Self::parse_normalized(&text).map_err(|err| format!("{} is not a valid config: {err}", path.display()))
    }
    /// The one parse → migrate → normalize path, shared by the lenient load and
    /// the strict undo read so they cannot drift apart.
    fn parse_normalized(text: &str) -> Result<(Self, Vec<String>), String> {
        let mut config = parse_config(text).map_err(|err| err.to_string())?;
        config.migrate();
        let warnings = config.normalize();
        Ok((config, warnings))
    }
    pub fn save(&self) -> std::io::Result<()> {
        ensure_dirs();
        self.save_to(&config_path())
    }
    pub fn save_to(&self, path: &Path) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let text = serde_json::to_string_pretty(self).map_err(std::io::Error::other)?;
        let backup = backup_path(path);
        // Refresh the rolling backup from the previous file only when it still
        // parses. A save made after recovery must not replace a good .bak with
        // the corrupt file that triggered the recovery.
        if path.exists() {
            match fs::read_to_string(path) {
                Ok(previous) => match Self::parse_normalized(&previous) {
                    Ok(_) => {
                        if let Err(err) = fs::copy(path, &backup) {
                            crate::logging::log(&format!("config: could not refresh backup {}: {err}", backup.display()));
                        }
                    }
                    Err(err) => {
                        crate::logging::log(&format!("config: leaving backup untouched; {} is not valid: {err}", path.display()));
                    }
                },
                Err(err) => {
                    crate::logging::log(&format!("config: leaving backup untouched; {} is unreadable: {err}", path.display()));
                }
            }
        }
        let tmp = path.with_extension("tmp");
        fs::write(&tmp, text)?;
        // A same-volume rename replaces the previous file atomically on Windows
        // (MoveFileExW + MOVEFILE_REPLACE_EXISTING), so a crash never leaves the
        // application without a valid config.
        fs::rename(tmp, path)?;
        Ok(())
    }
    /// Remove a custom preset and clear its active selection. Built-ins are
    /// never stored in `custom`, so callers validate custom identity first.
    pub fn remove_custom_preset(&mut self, module: &str, id: &str) -> bool {
        match module {
            "crosshair" => {
                let before = self.custom.crosshair.len();
                self.custom.crosshair.retain(|preset| preset.id != id);
                let removed = self.custom.crosshair.len() != before;
                if removed && self.crosshair.active_preset.as_deref() == Some(id) {
                    self.crosshair.active_preset = None;
                }
                removed
            }
            "display" => {
                let before = self.custom.display.len();
                self.custom.display.retain(|preset| preset.id != id);
                let removed = self.custom.display.len() != before;
                if removed && self.display.active_preset.as_deref() == Some(id) {
                    self.display.active_preset = None;
                }
                removed
            }
            "audio" => {
                let before = self.custom.audio.len();
                self.custom.audio.retain(|preset| preset.id != id);
                let removed = self.custom.audio.len() != before;
                if removed && self.audio.active_preset.as_deref() == Some(id) {
                    self.audio.active_preset = None;
                }
                removed
            }
            _ => false,
        }
    }
    pub fn all_crosshair_presets(&self) -> Vec<CrosshairPreset> {
        let mut v = builtin_crosshair_presets();
        v.extend(self.custom.crosshair.clone());
        v
    }
    pub fn all_display_presets(&self) -> Vec<DisplayPreset> {
        let mut v = builtin_display_presets();
        v.extend(self.custom.display.clone());
        v
    }
    pub fn all_audio_presets(&self) -> Vec<AudioPreset> {
        let mut v = builtin_audio_presets();
        v.extend(self.custom.audio.clone());
        v
    }
    pub fn find_crosshair_preset(&self, id: &str) -> Option<CrosshairPreset> {
        self.all_crosshair_presets().into_iter().find(|p| p.id == id)
    }
    pub fn find_display_preset(&self, id: &str) -> Option<DisplayPreset> {
        self.all_display_presets().into_iter().find(|p| p.id == id)
    }
    pub fn find_audio_preset(&self, id: &str) -> Option<AudioPreset> {
        self.all_audio_presets().into_iter().find(|p| p.id == id)
    }
}

fn camel_key(key: &str) -> String {
    let mut parts = key.split('_');
    let Some(first) = parts.next() else { return key.to_string() };
    let mut out = String::from(first);
    for part in parts {
        let mut chars = part.chars();
        if let Some(first_char) = chars.next() {
            out.extend(first_char.to_uppercase());
            out.push_str(chars.as_str());
        }
    }
    out
}

fn migrate_legacy_keys(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(map) => {
            let old = std::mem::take(map);
            for (key, mut child) in old {
                migrate_legacy_keys(&mut child);
                map.insert(camel_key(&key), child);
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                migrate_legacy_keys(item);
            }
        }
        _ => {}
    }
}

fn parse_config(text: &str) -> Result<Config, serde_json::Error> {
    // Windows editors often add a UTF-8 BOM. It is not part of JSON and used to
    // make an otherwise valid config look unreadable.
    let text = text.trim_start_matches('\u{feff}');
    let mut value: serde_json::Value = serde_json::from_str(text)?;
    migrate_legacy_keys(&mut value);
    serde_json::from_value(value)
}

pub(crate) fn backup_path(path: &Path) -> PathBuf {
    if path == config_path() {
        data_dir().join("config.json.bak")
    } else {
        match path.file_name() {
            Some(name) => {
                let mut backup = name.to_os_string();
                backup.push(".bak");
                path.with_file_name(backup)
            }
            None => path.with_extension("bak"),
        }
    }
}

/// The damaged copy kept beside the config when a parse fails. The live file is
/// left in place and is replaced by the next successful save.
pub(crate) fn corrupt_path(path: &Path) -> PathBuf {
    path.with_extension("corrupt.json")
}

pub fn default_slots() -> SlotsConfig {
    let slot = |id: &str, name: &str, emoji: &str, hotkey: &str, aim: bool, screen: bool, sound: bool, aim_preset: &str, screen_preset: &str, sound_preset: &str| Slot {
        id: id.into(),
        name: name.into(),
        emoji: emoji.into(),
        hotkey: Some(hotkey.into()),
        target_exe: None,
        crosshair: SlotModule { on: aim, preset: Some(aim_preset.into()) },
        display: SlotModule { on: screen, preset: Some(screen_preset.into()) },
        audio: SlotModule { on: sound, preset: Some(sound_preset.into()) },
    };
    SlotsConfig {
        list: vec![
            slot("slot-fps", "Competitive", "🎯", "F9", true, false, true, "style_tsight", "reference_neutral", "fps_footsteps"),
            slot("slot-night", "Late Night", "🌙", "F10", true, true, true, "style_dot", "late_night", "cinema"),
            slot("slot-cinema", "Cinema", "🎬", "F11", false, true, true, "style_plus", "cinema", "cinema"),
        ],
        active: None,
        baseline: None,
    }
}

pub fn builtin_crosshair_presets() -> Vec<CrosshairPreset> {
    let mk = |id: &str,
              name: &str,
              style: &str,
              color: &str,
              size: f32,
              gap: f32,
              dot: bool,
              dot_size: f32,
              outline: bool,
              opacity: f32| {
        let mut config = CrosshairConfig::default();
        config.style = style.into();
        config.color = color.into();
        config.size = size;
        config.thickness = 2.0;
        config.gap = gap;
        config.dot = dot;
        config.dot_size = dot_size;
        config.outline = outline;
        config.outline_width = 1.0;
        config.opacity = opacity;
        CrosshairPreset { id: id.into(), name: name.into(), builtin: true, config }
    };
    vec![
        mk("style_plus", "Pro Plus", "plus", "#00FF66", 17.0, 3.0, false, 2.0, true, 100.0),
        mk("style_dot", "Precision Dot", "dot", "#00E5FF", 4.0, 0.0, false, 2.0, true, 100.0),
        mk("style_ring", "Tracking Ring", "ring", "#FFFFFF", 18.0, 0.0, false, 2.0, true, 90.0),
        mk("style_circle_dot", "Ring + Dot", "ring", "#38FF9C", 16.0, 0.0, true, 3.0, true, 95.0),
        mk("style_triangle", "Triangle", "triangle", "#FF5C8D", 14.0, 0.0, false, 2.0, true, 90.0),
        mk("style_peak", "Chevron", "peak", "#FFD600", 12.0, 0.0, false, 2.0, true, 100.0),
        mk("style_tsight", "Open T-Cross", "tsight", "#FFE600", 16.0, 8.0, true, 2.0, true, 100.0),
    ]
}

pub fn builtin_display_presets() -> Vec<DisplayPreset> {
    let mk = |id: &str, name: &str, brightness: f32, contrast: f32, gamma: f32, warmth: f32, r: f32, g: f32, b: f32| DisplayPreset {
        id: id.into(),
        name: name.into(),
        builtin: true,
        values: DisplayValues { brightness, contrast, gamma, warmth, r, g, b, panel: "auto".into() },
    };
    vec![
        mk("reference_neutral", "⚖️ Standard", 0.0, 0.0, 1.00, 0.0, 100.0, 100.0, 100.0),
        mk("dark_visibility", "🔦 Shadow Boost", 4.0, 6.0, 1.18, 0.0, 100.0, 100.0, 100.0),
        mk("cinema", "🍿 Movie Night", -2.0, 8.0, 0.93, 10.0, 100.0, 100.0, 100.0),
        mk("late_night", "🌙 Night Comfort", -8.0, 0.0, 0.98, 40.0, 100.0, 100.0, 100.0),
        mk("vivid", "✨ Vivid Games", 3.0, 10.0, 0.96, 0.0, 100.0, 100.0, 100.0),
    ]
}

pub fn builtin_audio_presets() -> Vec<AudioPreset> {
    let mk = |id: &str, name: &str, bands: [f32; 10], bass: f32, treble: f32, output_level: f32, echo: f32, surround: f32, dynamic_boost: f32| AudioPreset {
        id: id.into(),
        name: name.into(),
        builtin: true,
        bands,
        bass,
        treble,
        output_level,
        echo,
        surround,
        dynamic_boost,
        volume_leveling: 0.0,
        balance: 0.0,
        filter_q: 1.5,
        volume_guard: true,
    };
    vec![
        mk("studio_flat", "🎚️ Flat", [0.0; 10], 0.0, 0.0, 0.0, 0.0, 0.0, 0.0),
        mk("fps_footsteps", "👣 Footsteps", [-4.0, -3.5, -2.5, -1.0, 0.0, 1.5, 3.5, 4.5, 3.0, 1.0], 0.0, 3.0, 0.0, 0.0, 0.0, 0.0),
        mk("cinema", "🎬 Movies", [3.0, 3.5, 2.0, 0.0, -1.0, 1.5, 2.0, 1.5, 1.0, 1.5], 3.0, 3.0, 0.0, 2.0, 2.0, 1.0),
        mk("harman", "🎵 Music", [2.5, 3.0, 1.5, 0.0, -1.0, 0.0, 1.5, 2.0, 2.5, 1.5], 2.0, 2.0, 0.0, 0.0, 0.0, 0.0),
        mk("clean_bass", "🔊 Bass Boost", [4.0, 4.5, 2.5, 0.5, -1.0, 0.0, 1.0, 1.5, 1.5, 1.0], 4.0, 2.0, 0.0, 0.0, 0.0, 0.0),
        mk("comms", "🎙️ Voice Chat", [-5.0, -4.0, -2.0, -0.5, 1.0, 2.5, 3.0, 1.5, 0.0, -1.0], 0.0, 3.0, 0.0, 0.0, 0.0, 0.0),
        mk("rock", "🎸 Rock", [2.5, 2.0, 1.0, -0.5, -1.0, 1.0, 2.5, 2.0, 1.0, 0.0], 2.0, 2.0, 0.0, 0.0, 1.0, 0.0),
        mk("horror", "👻 Horror", [4.5, 4.0, 1.0, -2.0, -3.0, -1.5, 0.5, 1.0, 2.0, 2.5], 3.0, 1.0, 0.0, 3.0, 2.0, 1.0),
        mk("lofi", "🎧 Lo-Fi / Chill", [1.5, 2.0, 1.5, 1.0, 0.5, 0.0, -1.0, -2.0, -2.5, -3.0], 1.0, 0.0, 0.0, 1.0, 0.0, 0.0),
    ]
}

fn legacy_crosshair_presets() -> Vec<CrosshairPreset> {
    let mk = |id: &str, name: &str, style: &str, color: &str, size: f32, gap: f32, dot: bool, dot_size: f32, outline: bool| {
        let mut config = CrosshairConfig::default();
        config.style = style.into();
        config.color = color.into();
        config.size = size;
        config.thickness = 2.0;
        config.gap = gap;
        config.dot = dot;
        config.dot_size = dot_size;
        config.outline = outline;
        CrosshairPreset { id: id.into(), name: name.into(), builtin: true, config }
    };
    vec![
        mk("style_plus", "", "plus", "#4EA8FF", 14.0, 4.0, false, 2.0, true),
        mk("style_dot", "", "dot", "#00E5FF", 4.0, 0.0, false, 2.0, false),
        mk("style_ring", "", "ring", "#FFFFFF", 18.0, 0.0, false, 2.0, true),
        mk("style_circle_dot", "", "ring", "#5CE693", 16.0, 0.0, true, 3.0, true),
        mk("style_triangle", "", "triangle", "#FF6B6B", 22.0, 3.0, false, 2.0, true),
        mk("style_peak", "", "peak", "#FFE600", 21.0, 4.0, false, 2.0, true),
        mk("style_tsight", "", "tsight", "#FFE600", 22.0, 4.0, true, 2.0, true),
    ]
}

fn legacy_display_presets() -> Vec<DisplayPreset> {
    let mk = |id: &str, name: &str, brightness: f32, contrast: f32, gamma: f32, warmth: f32, r: f32, g: f32, b: f32| DisplayPreset {
        id: id.into(),
        name: name.into(),
        builtin: true,
        values: DisplayValues { brightness, contrast, gamma, warmth, r, g, b, panel: "led".into() },
    };
    vec![
        mk("reference_neutral", "", 0.0, 0.0, 1.0, 0.0, 100.0, 100.0, 100.0),
        mk("dark_visibility", "", 5.0, 7.0, 1.16, 6.0, 100.0, 101.0, 103.0),
        mk("cinema", "", -3.0, 9.0, 1.10, 12.0, 103.0, 100.0, 96.0),
        mk("late_night", "", -8.0, 1.0, 1.08, 34.0, 100.0, 94.0, 86.0),
        mk("vivid", "", 2.0, 11.0, 0.97, 5.0, 104.0, 101.0, 100.0),
    ]
}

fn legacy_audio_presets() -> Vec<AudioPreset> {
    let mk = |id: &str, name: &str, bands: [f32; 10], bass: f32, treble: f32, output_level: f32, echo: f32, surround: f32, dynamic_boost: f32| AudioPreset {
        id: id.into(),
        name: name.into(),
        builtin: true,
        bands,
        bass,
        treble,
        output_level,
        echo,
        surround,
        dynamic_boost,
        volume_leveling: 0.0,
        balance: 0.0,
        filter_q: 1.0,
        volume_guard: true,
    };
    vec![
        mk("studio_flat", "", [0.0; 10], 0.0, 0.0, 0.0, 0.0, 0.0, 0.0),
        mk("fps_footsteps", "", [2.0, 3.0, 4.0, 3.0, 1.0, -2.0, -4.0, -3.0, -1.0, 0.0], 2.0, 5.0, 0.0, 0.0, 0.0, 0.0),
        mk("cinema", "", [3.0, 4.0, 3.0, 2.0, 0.0, -1.0, -2.0, -1.0, 0.0, 2.0], 5.0, 4.0, 0.0, 6.0, 4.0, 2.0),
        mk("harman", "", [4.0, 3.0, 2.0, 1.0, 0.0, 0.0, 1.0, 2.0, 3.0, 4.0], 4.0, 4.0, 0.0, 0.0, 0.0, 0.0),
        mk("clean_bass", "", [6.0, 5.0, 2.0, 1.0, 0.0, -1.0, -2.0, -2.0, -1.0, 0.0], 8.0, 2.0, 0.0, 0.0, 0.0, 0.0),
        mk("comms", "", [-4.0, -3.0, -1.0, 1.0, 3.0, 4.0, 5.0, 4.0, 2.0, 1.0], 0.0, 8.0, 0.0, 0.0, 0.0, 0.0),
    ]
}

fn close(a: f32, b: f32) -> bool {
    (a - b).abs() < 0.0001
}

fn same_crosshair_values(a: &CrosshairConfig, b: &CrosshairConfig) -> bool {
    a.style == b.style
        && close(a.size, b.size)
        && close(a.thickness, b.thickness)
        && close(a.gap, b.gap)
        && close(a.rotation, b.rotation)
        && a.dot == b.dot
        && close(a.dot_size, b.dot_size)
        && a.outline == b.outline
        && close(a.outline_width, b.outline_width)
        && close(a.opacity, b.opacity)
        && a.color.eq_ignore_ascii_case(&b.color)
}

fn same_display_values(a: &DisplayValues, b: &DisplayValues) -> bool {
    close(a.brightness, b.brightness)
        && close(a.contrast, b.contrast)
        && close(a.gamma, b.gamma)
        && close(a.warmth, b.warmth)
        && close(a.r, b.r)
        && close(a.g, b.g)
        && close(a.b, b.b)
}

fn same_audio_values(a: &AudioConfig, b: &AudioPreset) -> bool {
    a.bands.iter().zip(b.bands.iter()).all(|(left, right)| close(*left, *right))
        && close(a.bass, b.bass)
        && close(a.treble, b.treble)
        && close(a.output_level, b.output_level)
        && close(a.echo, b.echo)
        && close(a.surround, b.surround)
        && close(a.dynamic_boost, b.dynamic_boost)
        && close(a.volume_leveling, b.volume_leveling)
        && close(a.balance, b.balance)
        && close(a.filter_q, b.filter_q)
        && a.volume_guard == b.volume_guard
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crosshair_catalog_has_seven_distinct_designs() {
        let presets = builtin_crosshair_presets();
        assert_eq!(presets.len(), 7);
        let names: Vec<&str> = presets.iter().map(|preset| preset.name.as_str()).collect();
        assert_eq!(
            names,
            ["Pro Plus", "Precision Dot", "Tracking Ring", "Ring + Dot", "Triangle", "Chevron", "Open T-Cross"]
        );
        assert!(presets.iter().any(|preset| preset.id == "style_ring" && preset.config.style == "ring"));
        assert!(presets.iter().any(|preset| preset.id == "style_circle_dot" && preset.config.dot));
        assert!(presets.iter().any(|preset| preset.id == "style_tsight" && preset.config.dot));
        let styles: std::collections::HashSet<&str> = presets.iter().map(|preset| preset.config.style.as_str()).collect();
        assert!(styles.contains("triangle") && styles.contains("peak"));
        assert!(!names.iter().any(|name| *name == "Casual" || *name == "Hairline"));
    }

    #[test]
    fn builtin_presets_carry_the_tuned_esports_and_media_values() {
        let aim = builtin_crosshair_presets();
        let plus = aim.iter().find(|preset| preset.id == "style_plus").unwrap();
        assert_eq!(plus.config.size, 17.0);
        assert_eq!(plus.config.gap, 3.0);
        assert_eq!(plus.config.color, "#00FF66");
        assert!(!plus.config.dot);
        let tsight = aim.iter().find(|preset| preset.id == "style_tsight").unwrap();
        assert_eq!(tsight.config.gap, 8.0);
        assert!(
            tsight.config.dot_size * 0.5 + tsight.config.outline_width * 2.0
                < tsight.config.gap * 0.5
        );

        let display = builtin_display_presets();
        let names: Vec<&str> = display.iter().map(|preset| preset.name.as_str()).collect();
        assert_eq!(
            names,
            ["⚖️ Standard", "🔦 Shadow Boost", "🍿 Movie Night", "🌙 Night Comfort", "✨ Vivid Games"]
        );
        let movie = display.iter().find(|preset| preset.id == "cinema").unwrap();
        assert!(movie.values.gamma < 1.0, "Movie Night must deepen midtones");
        let shadow = display.iter().find(|preset| preset.id == "dark_visibility").unwrap();
        assert!(shadow.values.gamma > 1.0, "Shadow Boost must lift shadows");

        let audio = builtin_audio_presets();
        let names: Vec<&str> = audio.iter().map(|preset| preset.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "🎚️ Flat",
                "👣 Footsteps",
                "🎬 Movies",
                "🎵 Music",
                "🔊 Bass Boost",
                "🎙️ Voice Chat",
                "🎸 Rock",
                "👻 Horror",
                "🎧 Lo-Fi / Chill",
            ]
        );
        let footsteps = audio.iter().find(|preset| preset.id == "fps_footsteps").unwrap();
        assert!(footsteps.bands[7] > footsteps.bands[0], "footsteps must favour 4 kHz over rumble");
        assert!(audio.iter().all(|preset| preset.filter_q == 1.5 && preset.volume_guard));
    }

    #[test]
    fn every_builtin_preset_matches_the_approved_catalog_exactly() {
        let aim = builtin_crosshair_presets();
        let expected_aim: [(&str, &str, &str, &str, f32, f32, bool, f32, bool, f32, f32); 7] = [
            ("style_plus", "Pro Plus", "plus", "#00FF66", 17.0, 3.0, false, 2.0, true, 1.0, 100.0),
            ("style_dot", "Precision Dot", "dot", "#00E5FF", 4.0, 0.0, false, 2.0, true, 1.0, 100.0),
            ("style_ring", "Tracking Ring", "ring", "#FFFFFF", 18.0, 0.0, false, 2.0, true, 1.0, 90.0),
            ("style_circle_dot", "Ring + Dot", "ring", "#38FF9C", 16.0, 0.0, true, 3.0, true, 1.0, 95.0),
            ("style_triangle", "Triangle", "triangle", "#FF5C8D", 14.0, 0.0, false, 2.0, true, 1.0, 90.0),
            ("style_peak", "Chevron", "peak", "#FFD600", 12.0, 0.0, false, 2.0, true, 1.0, 100.0),
            ("style_tsight", "Open T-Cross", "tsight", "#FFE600", 16.0, 8.0, true, 2.0, true, 1.0, 100.0),
        ];
        assert_eq!(aim.len(), expected_aim.len());
        for (preset, expected) in aim.iter().zip(expected_aim.iter()) {
            assert_eq!(preset.id, expected.0);
            assert_eq!(preset.name, expected.1);
            assert_eq!(preset.config.style, expected.2);
            assert_eq!(preset.config.color, expected.3);
            assert_eq!(preset.config.size, expected.4);
            assert_eq!(preset.config.gap, expected.5);
            assert_eq!(preset.config.dot, expected.6);
            assert_eq!(preset.config.dot_size, expected.7);
            assert_eq!(preset.config.outline, expected.8);
            assert_eq!(preset.config.outline_width, expected.9);
            assert_eq!(preset.config.opacity, expected.10);
            assert_eq!(preset.config.thickness, 2.0);
            assert!(!preset.config.enabled, "built-in aim presets stay disabled");
        }

        let display = builtin_display_presets();
        let expected_display: [(&str, &str, f32, f32, f32, f32, f32, f32, f32); 5] = [
            ("reference_neutral", "⚖️ Standard", 0.0, 0.0, 1.00, 0.0, 100.0, 100.0, 100.0),
            ("dark_visibility", "🔦 Shadow Boost", 4.0, 6.0, 1.18, 0.0, 100.0, 100.0, 100.0),
            ("cinema", "🍿 Movie Night", -2.0, 8.0, 0.93, 10.0, 100.0, 100.0, 100.0),
            ("late_night", "🌙 Night Comfort", -8.0, 0.0, 0.98, 40.0, 100.0, 100.0, 100.0),
            ("vivid", "✨ Vivid Games", 3.0, 10.0, 0.96, 0.0, 100.0, 100.0, 100.0),
        ];
        assert_eq!(display.len(), expected_display.len());
        for (preset, expected) in display.iter().zip(expected_display.iter()) {
            assert_eq!(preset.id, expected.0);
            assert_eq!(preset.name, expected.1);
            assert_eq!(preset.values.brightness, expected.2);
            assert_eq!(preset.values.contrast, expected.3);
            assert_eq!(preset.values.gamma, expected.4);
            assert_eq!(preset.values.warmth, expected.5);
            assert_eq!(preset.values.r, expected.6);
            assert_eq!(preset.values.g, expected.7);
            assert_eq!(preset.values.b, expected.8);
        }

        let audio = builtin_audio_presets();
        let expected_audio: [(&str, &str, [f32; 10], f32, f32, f32, f32, f32); 9] = [
            ("studio_flat", "🎚️ Flat", [0.0; 10], 0.0, 0.0, 0.0, 0.0, 0.0),
            ("fps_footsteps", "👣 Footsteps", [-4.0, -3.5, -2.5, -1.0, 0.0, 1.5, 3.5, 4.5, 3.0, 1.0], 0.0, 3.0, 0.0, 0.0, 0.0),
            ("cinema", "🎬 Movies", [3.0, 3.5, 2.0, 0.0, -1.0, 1.5, 2.0, 1.5, 1.0, 1.5], 3.0, 3.0, 2.0, 2.0, 1.0),
            ("harman", "🎵 Music", [2.5, 3.0, 1.5, 0.0, -1.0, 0.0, 1.5, 2.0, 2.5, 1.5], 2.0, 2.0, 0.0, 0.0, 0.0),
            ("clean_bass", "🔊 Bass Boost", [4.0, 4.5, 2.5, 0.5, -1.0, 0.0, 1.0, 1.5, 1.5, 1.0], 4.0, 2.0, 0.0, 0.0, 0.0),
            ("comms", "🎙️ Voice Chat", [-5.0, -4.0, -2.0, -0.5, 1.0, 2.5, 3.0, 1.5, 0.0, -1.0], 0.0, 3.0, 0.0, 0.0, 0.0),
            ("rock", "🎸 Rock", [2.5, 2.0, 1.0, -0.5, -1.0, 1.0, 2.5, 2.0, 1.0, 0.0], 2.0, 2.0, 0.0, 1.0, 0.0),
            ("horror", "👻 Horror", [4.5, 4.0, 1.0, -2.0, -3.0, -1.5, 0.5, 1.0, 2.0, 2.5], 3.0, 1.0, 3.0, 2.0, 1.0),
            ("lofi", "🎧 Lo-Fi / Chill", [1.5, 2.0, 1.5, 1.0, 0.5, 0.0, -1.0, -2.0, -2.5, -3.0], 1.0, 0.0, 1.0, 0.0, 0.0),
        ];
        assert_eq!(audio.len(), expected_audio.len());
        for (preset, expected) in audio.iter().zip(expected_audio.iter()) {
            assert_eq!(preset.id, expected.0);
            assert_eq!(preset.name, expected.1);
            assert_eq!(preset.bands, expected.2);
            assert_eq!(preset.bass, expected.3);
            assert_eq!(preset.treble, expected.4);
            assert_eq!(preset.output_level, 0.0);
            assert_eq!(preset.echo, expected.5);
            assert_eq!(preset.surround, expected.6);
            assert_eq!(preset.dynamic_boost, expected.7);
            assert_eq!(preset.filter_q, 1.5);
            assert!(preset.volume_guard);
            assert_eq!(preset.balance, 0.0);
            assert_eq!(preset.volume_leveling, 0.0);
        }
    }

    #[test]
    fn image_mode_presets_round_trip_and_restore_their_image_block() {
        let mut saved = CrosshairConfig {
            image: Some("crosshairs/1700000000000-aim.png".into()),
            image_name: Some("aim.png".into()),
            image_scale: 180.0,
            image_rotation: 45.0,
            image_invert: true,
            image_solid: true,
            image_tint: true,
            image_solid_color: "#FF2200".into(),
            gif_speed: 1.75,
            gif_playing: false,
            ..Default::default()
        };
        saved.size = 22.0;
        saved.style = "ring".into();
        let preset = CrosshairPreset {
            id: "crosshair_image".into(),
            name: "Image Aim".into(),
            builtin: false,
            config: saved.clone(),
        };
        assert!(preset.image_mode());
        let parsed: CrosshairPreset =
            serde_json::from_str(&serde_json::to_string(&preset).unwrap()).unwrap();
        assert_eq!(parsed, preset, "an image-mode preset must survive config.json");

        let mut target = CrosshairConfig {
            enabled: true,
            image: Some("crosshairs/old.png".into()),
            image_name: Some("old.png".into()),
            image_scale: 25.0,
            image_rotation: 10.0,
            image_invert: false,
            image_solid: false,
            image_tint: false,
            image_solid_color: "#00FF00".into(),
            gif_speed: 0.5,
            gif_playing: true,
            monitor: Some("monitor-2".into()),
            ..Default::default()
        };
        parsed.apply_to(&mut target);
        assert!(target.enabled, "applying a preset must not switch the user's overlay off");
        assert_eq!(target.monitor.as_deref(), Some("monitor-2"));
        assert_eq!(target.style, "ring");
        assert_eq!(target.size, 22.0);
        assert_eq!(target.image.as_deref(), Some("crosshairs/1700000000000-aim.png"));
        assert_eq!(target.image_name.as_deref(), Some("aim.png"));
        assert_eq!(target.image_scale, 180.0);
        assert_eq!(target.image_rotation, 45.0);
        assert!(target.image_invert);
        assert!(target.image_solid);
        assert!(target.image_tint);
        assert_eq!(target.image_solid_color, "#FF2200");
        assert_eq!(target.gif_speed, 1.75);
        assert!(!target.gif_playing);

        let builtin = builtin_crosshair_presets().into_iter().find(|preset| preset.id == "style_plus").unwrap();
        assert!(!builtin.image_mode());
        builtin.apply_to(&mut target);
        assert_eq!(target.image.as_deref(), Some("crosshairs/1700000000000-aim.png"));
        assert_eq!(target.image_name.as_deref(), Some("aim.png"));
        assert_eq!(target.image_scale, 180.0);
        assert!(target.image_invert);
        assert!(target.image_solid);
        assert!(target.image_tint);
        assert_eq!(target.image_solid_color, "#FF2200");
        assert_eq!(target.gif_speed, 1.75);
        assert!(!target.gif_playing);
    }

    #[test]
    fn recent_images_skip_missing_files_and_reject_unsafe_names() {
        let dir = std::env::temp_dir().join(format!(
            "crosshairplus-recent-{}-{}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0)
        ));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("1700000000000-aim.png"), b"png").unwrap();
        fs::write(dir.join("1700000000001-alt.GIF"), b"gif").unwrap();
        fs::write(dir.join("notes.txt"), b"txt").unwrap();

        let images = recent_images_in(&dir);
        assert_eq!(images.len(), 2, "{images:?}");
        assert!(images.iter().any(|image| image.name == "1700000000000-aim.png" && image.label == "aim.png"));
        assert!(images.iter().any(|image| image.name == "1700000000001-alt.GIF" && image.label == "alt.GIF"));
        assert!(!images.iter().any(|image| image.name == "notes.txt"));

        let mut crosshair = CrosshairConfig::default();
        apply_stored_image_in(&dir, &mut crosshair, "1700000000000-aim.png").unwrap();
        assert_eq!(crosshair.image_name.as_deref(), Some("aim.png"));
        assert_eq!(
            Path::new(crosshair.image.as_deref().unwrap()).file_name().and_then(|name| name.to_str()),
            Some("1700000000000-aim.png")
        );

        fs::remove_file(dir.join("1700000000000-aim.png")).unwrap();
        let before = crosshair.clone();
        let error = apply_stored_image_in(&dir, &mut crosshair, "1700000000000-aim.png").unwrap_err();
        assert!(error.contains("no longer in the crosshairs folder"), "{error}");
        assert_eq!(crosshair, before, "a vanished recent file must not become a dead path");
        assert!(!recent_images_in(&dir).iter().any(|image| image.name == "1700000000000-aim.png"));

        for unsafe_name in ["../aim.png", "sub/aim.png", "C:\\aim.png", "notes.txt", "", "aim.png:stream"] {
            assert!(stored_image_path_in(&dir, unsafe_name).is_none(), "{unsafe_name}");
        }
        assert!(stored_image_path_in(&dir, "1700000000001-alt.GIF").is_some());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn deleting_the_active_stored_image_clears_the_crosshair_image() {
        let dir = std::env::temp_dir().join(format!(
            "crosshairplus-delete-image-{}-{}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0)
        ));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("1700000000000-aim.png"), b"png").unwrap();
        let mut config = Config::default();
        apply_stored_image_in(&dir, &mut config.crosshair, "1700000000000-aim.png").unwrap();
        assert!(delete_stored_image_in(&dir, &mut config, "1700000000000-aim.png").unwrap());
        assert!(config.crosshair.image.is_none());
        assert!(config.crosshair.image_name.is_none());
        assert!(!dir.join("1700000000000-aim.png").exists());
        let before = config.crosshair.clone();
        let error = delete_stored_image_in(&dir, &mut config, "1700000000000-aim.png").unwrap_err();
        assert!(error.contains("no longer in the crosshairs folder"), "{error}");
        assert_eq!(config.crosshair, before, "a vanished file must not change the crosshair");
        assert!(recent_images_in(&dir).is_empty(), "the deleted image must not be listed");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn deleting_a_stored_image_clears_image_mode_presets_that_used_it() {
        let dir = std::env::temp_dir().join(format!(
            "crosshairplus-delete-preset-image-{}-{}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0)
        ));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("1700000000000-live.png"), b"png").unwrap();
        fs::write(dir.join("1700000000001-shared.png"), b"png").unwrap();
        let live = dir.join("1700000000000-live.png").to_string_lossy().to_string();
        let shared = dir.join("1700000000001-shared.png").to_string_lossy().to_string();
        let mut config = Config::default();
        apply_stored_image_in(&dir, &mut config.crosshair, "1700000000000-live.png").unwrap();
        config.custom.crosshair = vec![
            CrosshairPreset {
                id: "crosshair_live".into(),
                name: "Live image".into(),
                builtin: false,
                config: CrosshairConfig {
                    image: Some(live.clone()),
                    image_name: Some("live.png".into()),
                    image_scale: 180.0,
                    size: 22.0,
                    ..Default::default()
                },
            },
            CrosshairPreset {
                id: "crosshair_shared".into(),
                name: "Shared image".into(),
                builtin: false,
                config: CrosshairConfig {
                    image: Some(shared),
                    image_name: Some("shared.png".into()),
                    ..Default::default()
                },
            },
        ];
        let live_preset = config.custom.crosshair[0].clone();
        config.crosshair.active_preset = Some("crosshair_live".into());

        // A file only one preset uses leaves the live image and the other
        // preset alone, and it is not the active preset.
        assert!(!delete_stored_image_in(&dir, &mut config, "1700000000001-shared.png").unwrap());
        assert!(config.custom.crosshair[1].config.image.is_none());
        assert!(config.custom.crosshair[1].config.image_name.is_none());
        assert_eq!(config.custom.crosshair[0], live_preset);
        assert_eq!(config.crosshair.active_preset.as_deref(), Some("crosshair_live"));
        assert_eq!(config.crosshair.image.as_deref(), Some(live.as_str()));

        // Deleting the active preset's image clears its reference, the active
        // marker and the live path, so selecting it later cannot restore a
        // dead path; its shape tuning survives and it applies as a shape preset.
        assert!(delete_stored_image_in(&dir, &mut config, "1700000000000-live.png").unwrap());
        let cleared = config.custom.crosshair[0].clone();
        assert!(cleared.config.image.is_none());
        assert!(cleared.config.image_name.is_none());
        assert!(!cleared.image_mode(), "a preset without an image must apply as a shape preset");
        assert_eq!(cleared.config.size, 22.0, "clearing the image must keep the shape tuning");
        assert_eq!(cleared.config.image_scale, 180.0);
        assert_eq!(config.crosshair.active_preset, None);
        assert!(config.crosshair.image.is_none());
        assert!(config.crosshair.image_name.is_none());
        let mut target = CrosshairConfig { image: Some("current.png".into()), ..Default::default() };
        cleared.apply_to(&mut target);
        assert_eq!(
            target.image.as_deref(),
            Some("current.png"),
            "the cleared preset must keep the user's current image"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_failed_image_action_clears_only_a_vanished_live_image() {
        let dir = std::env::temp_dir().join(format!(
            "crosshairplus-vanished-live-image-{}-{}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0)
        ));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("1700000000000-live.png"), b"png").unwrap();
        fs::write(dir.join("1700000000001-kept.png"), b"png").unwrap();
        let live = dir.join("1700000000000-live.png").to_string_lossy().to_string();
        let kept = dir.join("1700000000001-kept.png").to_string_lossy().to_string();
        let mut crosshair = CrosshairConfig {
            image: Some(live.clone()),
            image_name: Some("live.png".into()),
            ..Default::default()
        };

        // A failed delete can be a locked file; the still-present live image is kept.
        assert!(!clear_vanished_live_image_in(&dir, &mut crosshair, "1700000000000-live.png"));
        assert_eq!(crosshair.image.as_deref(), Some(live.as_str()));

        // Once the file is gone, the same failed action drops the dead path.
        fs::remove_file(dir.join("1700000000000-live.png")).unwrap();
        assert!(clear_vanished_live_image_in(&dir, &mut crosshair, "1700000000000-live.png"));
        assert!(crosshair.image.is_none());
        assert!(crosshair.image_name.is_none());

        // A different live image and unsafe names are never touched.
        crosshair.image = Some(kept.clone());
        crosshair.image_name = Some("kept.png".into());
        assert!(!clear_vanished_live_image_in(&dir, &mut crosshair, "1700000000000-live.png"));
        assert!(!clear_vanished_live_image_in(&dir, &mut crosshair, "../kept.png"));
        assert_eq!(crosshair.image.as_deref(), Some(kept.as_str()));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn applying_an_image_preset_with_a_missing_file_keeps_the_live_image() {
        let dir = std::env::temp_dir().join(format!(
            "crosshairplus-preset-missing-image-{}-{}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0)
        ));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("present.png"), b"png").unwrap();
        let present = dir.join("present.png").to_string_lossy().to_string();
        let missing = dir.join("gone.png").to_string_lossy().to_string();
        let preset = CrosshairPreset {
            id: "crosshair_image".into(),
            name: "Image Aim".into(),
            builtin: false,
            config: CrosshairConfig {
                image: Some(missing),
                image_name: Some("gone.png".into()),
                image_scale: 180.0,
                size: 24.0,
                color: "#FF0000".into(),
                ..Default::default()
            },
        };
        let mut target = CrosshairConfig {
            image: Some(present.clone()),
            image_name: Some("present.png".into()),
            image_scale: 75.0,
            image_rotation: 12.0,
            ..Default::default()
        };

        assert_eq!(preset.apply_to_checked(&mut target).as_deref(), Some("gone.png"));
        assert_eq!(target.image.as_deref(), Some(present.as_str()));
        assert_eq!(target.image_name.as_deref(), Some("present.png"));
        assert_eq!(target.image_scale, 75.0, "the preset's image tuning must not leak in");
        assert_eq!(target.image_rotation, 12.0);
        assert_eq!(target.size, 24.0, "the preset's shape still applies");
        assert_eq!(target.color, "#FF0000");

        // The page applies a preset by sending the whole config; the same guard
        // must keep the live image block and report the missing label.
        let mut incoming = preset.config.clone();
        assert_eq!(
            guard_preset_image(&target, &mut incoming, Some(&preset)).as_deref(),
            Some("gone.png")
        );
        assert_eq!(incoming.image.as_deref(), Some(present.as_str()));
        assert_eq!(incoming.image_name.as_deref(), Some("present.png"));
        assert_eq!(incoming.image_scale, 75.0);
        assert_eq!(incoming.size, 24.0, "the requested shape is kept");

        // A live image that is itself gone must not be "kept": both routes drop
        // it so the failed preset cannot leave a dead path behind.
        let dead_live = CrosshairConfig {
            image: Some("C:\\live\\gone.png".into()),
            image_name: Some("gone.png".into()),
            image_scale: 75.0,
            ..Default::default()
        };
        let mut dead_target = dead_live.clone();
        assert_eq!(preset.apply_to_checked(&mut dead_target).as_deref(), Some("gone.png"));
        assert!(dead_target.image.is_none());
        assert!(dead_target.image_name.is_none());
        assert_eq!(dead_target.size, 24.0, "the preset's shape still applies");
        let mut dead_incoming = preset.config.clone();
        assert_eq!(
            guard_preset_image(&dead_live, &mut dead_incoming, Some(&preset)).as_deref(),
            Some("gone.png")
        );
        assert!(dead_incoming.image.is_none());
        assert!(dead_incoming.image_name.is_none());
        assert_eq!(dead_incoming.size, 24.0, "the requested shape is kept");

        // A different live image is never mistaken for the preset's own image.
        let mut imported = preset.config.clone();
        imported.image = Some(present.clone());
        assert_eq!(guard_preset_image(&target, &mut imported, Some(&preset)), None);
        assert_eq!(imported.image.as_deref(), Some(present.as_str()));

        // A file that still exists installs the preset's image exactly as before.
        let valid = CrosshairPreset {
            id: preset.id.clone(),
            name: preset.name.clone(),
            builtin: false,
            config: CrosshairConfig {
                image: Some(present.clone()),
                image_name: Some("present.png".into()),
                image_scale: 180.0,
                ..preset.config.clone()
            },
        };
        let mut target = CrosshairConfig { image: Some("C:\\live\\current.png".into()), ..Default::default() };
        assert_eq!(valid.apply_to_checked(&mut target), None);
        assert_eq!(target.image.as_deref(), Some(present.as_str()));
        assert_eq!(target.image_name.as_deref(), Some("present.png"));
        assert_eq!(target.image_scale, 180.0);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn three_new_sound_presets_are_distinct_and_non_clipping() {
        let audio = builtin_audio_presets();
        let preset = |id: &str| audio.iter().find(|preset| preset.id == id).unwrap_or_else(|| panic!("missing {id}"));
        let rock = preset("rock");
        let horror = preset("horror");
        let lofi = preset("lofi");
        assert_eq!(rock.name, "🎸 Rock");
        assert_eq!(horror.name, "👻 Horror");
        assert_eq!(lofi.name, "🎧 Lo-Fi / Chill");
        for candidate in [rock, horror, lofi] {
            assert_eq!(candidate.filter_q, 1.5);
            assert!(candidate.volume_guard, "new presets ship with clip guard on");
            assert_eq!(candidate.output_level, 0.0, "new presets stay at 0 dB master");
            assert_eq!(candidate.balance, 0.0);
            assert_eq!(candidate.volume_leveling, 0.0);
            let peak = candidate.bands.iter().cloned().fold(f32::MIN, f32::max);
            assert!(peak <= 5.0, "{} peaks at {peak} dB", candidate.name);
        }
        assert_ne!(rock.bands, horror.bands);
        assert_ne!(horror.bands, lofi.bands);
        assert_ne!(rock.bands, lofi.bands);
        assert!(horror.bands[0] > horror.bands[5], "Horror leans sub-heavy and scooped");
        assert!(lofi.bands[9] < 0.0, "Lo-Fi rolls the top end off");
        assert_eq!(audio.len(), 9);
    }

    #[test]
    fn missing_per_monitor_values_fall_back_to_global() {
        let mut display = DisplayConfig { customize_each: true, ..Default::default() };
        display.global.gamma = 1.35;
        assert_eq!(display.values_for("never-seen").gamma, 1.35);
    }

    #[test]
    fn display_and_crosshair_preset_apply_preserves_non_preset_fields() {
        let mut values = DisplayValues { panel: "oled".into(), ..Default::default() };
        let movie = builtin_display_presets().into_iter().find(|preset| preset.id == "cinema").unwrap();
        movie.apply_to(&mut values);
        assert_eq!(values.panel, "oled");
        assert_eq!(values.gamma, 0.93);

        let mut target = CrosshairConfig {
            enabled: true,
            image: Some("custom.png".into()),
            monitor: Some("monitor-1".into()),
            hide_in_menus: true,
            aim_visibility: Some("only".into()),
            hide_on_rmb: false,
            aim_button: Some("mouse4".into()),
            ..Default::default()
        };
        let plus = builtin_crosshair_presets().into_iter().find(|preset| preset.id == "style_plus").unwrap();
        plus.apply_to(&mut target);
        assert!(target.enabled);
        assert_eq!(target.image.as_deref(), Some("custom.png"));
        assert_eq!(target.monitor.as_deref(), Some("monitor-1"));
        assert!(target.hide_in_menus);
        assert_eq!(target.aim_visibility.as_deref(), Some("only"), "a shape preset must not reset aim visibility");
        assert!(!target.hide_on_rmb);
        assert_eq!(target.aim_button.as_deref(), Some("mouse4"), "a shape preset must not reset the aim button");
        assert_eq!(target.style, "plus");
        assert_eq!(target.size, 17.0);

        let mut legacy = CrosshairConfig { aim_visibility: None, hide_on_rmb: true, ..Default::default() };
        plus.apply_to(&mut legacy);
        assert_eq!(legacy.aim_visibility.as_deref(), Some("hide"), "a legacy hide-on-RMB choice survives a preset");
        assert!(legacy.hide_on_rmb);
    }

    #[test]
    fn aim_button_defaults_and_normalizes() {
        assert_eq!(CrosshairConfig::default().aim_button.as_deref(), Some("right"));

        let mut config = CrosshairConfig { aim_button: None, ..Default::default() };
        config.normalize();
        assert_eq!(config.aim_button.as_deref(), Some("right"), "a legacy config must keep right mouse as the aim button");
        let mut parsed: CrosshairConfig = serde_json::from_str(r#"{"hideOnRmb":true}"#).unwrap();
        parsed.normalize();
        assert_eq!(parsed.aim_button.as_deref(), Some("right"), "a config written before the picker still aims on right mouse");

        for button in ["left", "middle", "mouse4", "mouse5"] {
            config.aim_button = Some(button.into());
            config.normalize();
            assert_eq!(config.aim_button.as_deref(), Some(button));
        }
        config.aim_button = Some("side".into());
        config.normalize();
        assert_eq!(config.aim_button.as_deref(), Some("right"), "an unknown button falls back to right mouse");
    }

    #[test]
    fn auto_panel_is_accepted_and_preserved() {
        let mut values = DisplayValues { panel: "Auto".into(), ..Default::default() };
        values.normalize();
        assert_eq!(values.panel, "auto");
        assert_eq!(DisplayValues::default().panel, "auto");
    }

    #[test]
    fn legacy_active_builtins_migrate_without_touching_custom_presets_or_slots() {
        let mut config = Config::default();
        config.version = 1;
        let old_plus = legacy_crosshair_presets().into_iter().find(|preset| preset.id == "style_plus").unwrap();
        config.crosshair = old_plus.config;
        config.crosshair.enabled = true;
        config.crosshair.image = Some("keep.png".into());
        config.crosshair.active_preset = Some("style_plus".into());

        let old_movie = legacy_display_presets().into_iter().find(|preset| preset.id == "cinema").unwrap();
        config.display.global = old_movie.values;
        config.display.global.panel = "oled".into();
        config.display.active_preset = Some("cinema".into());

        let old_footsteps = legacy_audio_presets().into_iter().find(|preset| preset.id == "fps_footsteps").unwrap();
        config.audio.bands = old_footsteps.bands;
        config.audio.bass = old_footsteps.bass;
        config.audio.treble = old_footsteps.treble;
        config.audio.output_level = old_footsteps.output_level;
        config.audio.echo = old_footsteps.echo;
        config.audio.surround = old_footsteps.surround;
        config.audio.dynamic_boost = old_footsteps.dynamic_boost;
        config.audio.volume_leveling = old_footsteps.volume_leveling;
        config.audio.balance = old_footsteps.balance;
        config.audio.filter_q = old_footsteps.filter_q;
        config.audio.volume_guard = old_footsteps.volume_guard;
        config.audio.active_preset = Some("fps_footsteps".into());

        config.custom.crosshair.push(CrosshairPreset {
            id: "custom-keep".into(),
            name: "Custom Keep".into(),
            builtin: false,
            config: CrosshairConfig::default(),
        });
        config.slots.list[0].name = "Keep Slot".into();

        config.migrate();

        assert_eq!(config.version, CONFIG_VERSION);
        assert_eq!(config.crosshair.size, 17.0);
        assert_eq!(config.crosshair.color, "#00FF66");
        assert!(config.crosshair.enabled);
        assert_eq!(config.crosshair.image.as_deref(), Some("keep.png"));
        assert_eq!(config.display.global.gamma, 0.93);
        assert_eq!(config.display.global.panel, "oled");
        assert_eq!(config.audio.bands[0], -4.0);
        assert_eq!(config.audio.filter_q, 1.5);
        assert_eq!(config.custom.crosshair.len(), 1);
        assert_eq!(config.custom.crosshair[0].id, "custom-keep");
        assert_eq!(config.slots.list[0].name, "Keep Slot");
    }

    #[test]
    fn tweaked_values_and_custom_presets_are_not_rewritten_by_migration() {
        let mut config = Config::default();
        config.version = 1;
        let mut old_movie = legacy_display_presets().into_iter().find(|preset| preset.id == "cinema").unwrap();
        old_movie.values.gamma = 0.90;
        config.display.global = old_movie.values;
        config.display.active_preset = Some("cinema".into());
        config.custom.display.push(DisplayPreset {
            id: "custom-display".into(),
            name: "Custom Display".into(),
            builtin: false,
            values: DisplayValues::default(),
        });
        config.migrate();
        assert_eq!(config.display.global.gamma, 0.90);
        assert_eq!(config.custom.display[0].id, "custom-display");
        assert_eq!(config.version, CONFIG_VERSION);
    }

    #[test]
    fn migration_is_idempotent() {
        let mut config = Config::default();
        config.version = 1;
        let old_movie = legacy_display_presets().into_iter().find(|preset| preset.id == "cinema").unwrap();
        config.display.global = old_movie.values;
        config.display.global.panel = "oled".into();
        config.display.active_preset = Some("cinema".into());
        config.custom.audio.push(AudioPreset {
            id: "custom-audio".into(),
            name: "Custom Audio".into(),
            builtin: false,
            ..AudioPreset::default()
        });

        config.migrate();
        let once = config.clone();
        config.migrate();
        assert_eq!(config, once);
        assert_eq!(config.version, CONFIG_VERSION);
        assert_eq!(config.display.global.gamma, 0.93);
        assert_eq!(config.display.global.panel, "oled");
        assert_eq!(config.custom.audio[0].id, "custom-audio");
    }

    #[test]
    fn legacy_crosshair_presets_normalize_to_plus() {
        let mut config = Config::default();
        config.crosshair.active_preset = Some("style_hairline".into());
        config.slots.list[0].crosshair.preset = Some("style_casual".into());
        config.normalize();
        assert_eq!(config.crosshair.active_preset.as_deref(), Some("style_plus"));
        assert_eq!(config.slots.list[0].crosshair.preset.as_deref(), Some("style_plus"));
        assert_eq!(config.settings.aim_hotkey, "F8");
        assert!(config.settings.display_hotkey.is_empty());
        assert!(config.settings.audio_hotkey.is_empty());
    }

    #[test]
    fn config_json_round_trip() {
        let config = Config::default();
        let text = serde_json::to_string(&config).unwrap();
        let parsed: Config = serde_json::from_str(&text).unwrap();
        assert_eq!(parsed, config);
        let with_bom = format!("\u{feff}{text}");
        assert_eq!(parse_config(&with_bom).unwrap(), config);
    }

    #[test]
    fn prior_snake_case_config_migrates_without_losing_values() {
        let mut config = Config::default();
        config.settings.minimize_to_tray = false;
        config.crosshair.dot_size = 7.0;
        config.display.global.gamma = 0.84;
        config.slots.list[0].name = "Migrated".into();
        let mut value = serde_json::to_value(&config).unwrap();
        fn snake_keys(value: &mut serde_json::Value) {
            match value {
                serde_json::Value::Object(map) => {
                    let old = std::mem::take(map);
                    for (key, mut child) in old {
                        snake_keys(&mut child);
                        let snake = key.chars().enumerate().fold(String::new(), |mut out, (index, ch)| {
                            if ch.is_uppercase() {
                                if index > 0 { out.push('_'); }
                                out.extend(ch.to_lowercase());
                            } else {
                                out.push(ch);
                            }
                            out
                        });
                        map.insert(snake, child);
                    }
                }
                serde_json::Value::Array(items) => items.iter_mut().for_each(snake_keys),
                _ => {}
            }
        }
        snake_keys(&mut value);
        let legacy = serde_json::to_string(&value).unwrap();
        let parsed = parse_config(&legacy).unwrap();
        assert_eq!(parsed.settings.minimize_to_tray, false);
        assert_eq!(parsed.crosshair.dot_size, 7.0);
        assert_eq!(parsed.display.global.gamma, 0.84);
        assert_eq!(parsed.slots.list[0].name, "Migrated");
    }

    #[test]
    fn save_replaces_existing_config_atomically_and_refreshes_the_rolling_backup() {
        let dir = std::env::temp_dir().join(format!(
            "crosshairplus-config-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");
        let mut config = Config::default();
        config.save_to(&path).unwrap();
        let first = fs::read_to_string(&path).unwrap();
        config.crosshair.size = 22.0;
        config.save_to(&path).unwrap();
        let second = fs::read_to_string(&path).unwrap();
        assert_ne!(first, second);
        assert_eq!(fs::read_to_string(dir.join("config.json.bak")).unwrap(), first);
        config.crosshair.size = 24.0;
        config.save_to(&path).unwrap();
        let third = fs::read_to_string(&path).unwrap();
        assert_eq!(fs::read_to_string(dir.join("config.json.bak")).unwrap(), second);
        assert!(!dir.join("config.tmp").exists());
        let parsed: Config = serde_json::from_str(&third).unwrap();
        assert_eq!(parsed.crosshair.size, 24.0);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn custom_presets_are_removed_from_every_module() {
        let mut config = Config::default();
        config.custom.crosshair.push(CrosshairPreset { id: "custom-crosshair".into(), name: "Sandbox Crosshair".into(), builtin: false, config: CrosshairConfig::default() });
        config.custom.display.push(DisplayPreset { id: "custom-display".into(), name: "Sandbox Display".into(), builtin: false, values: DisplayValues::default() });
        config.custom.audio.push(AudioPreset { id: "custom-audio".into(), name: "Sandbox Audio".into(), builtin: false, ..AudioPreset::default() });
        config.crosshair.active_preset = Some("custom-crosshair".into());
        config.display.active_preset = Some("custom-display".into());
        config.audio.active_preset = Some("custom-audio".into());

        assert!(config.remove_custom_preset("crosshair", "custom-crosshair"));
        assert!(config.remove_custom_preset("display", "custom-display"));
        assert!(config.remove_custom_preset("audio", "custom-audio"));
        assert!(config.custom.crosshair.is_empty());
        assert!(config.custom.display.is_empty());
        assert!(config.custom.audio.is_empty());
        assert!(config.crosshair.active_preset.is_none());
        assert!(config.display.active_preset.is_none());
        assert!(config.audio.active_preset.is_none());
        assert!(!config.remove_custom_preset("crosshair", "style_plus"));
        assert!(!config.remove_custom_preset("unknown", "custom-crosshair"));
    }

    #[test]
    fn audio_effects_normalize_to_fxsound_ranges() {
        let mut audio = AudioConfig {
            bass: -3.0,
            treble: 13.0,
            output_level: 30.0,
            echo: 40.0,
            surround: -2.0,
            dynamic_boost: 11.0,
            volume_leveling: 9.0,
            balance: -30.0,
            filter_q: 0.2,
            ..Default::default()
        };
        audio.normalize();
        assert_eq!(audio.bass, 0.0);
        assert_eq!(audio.treble, 10.0);
        assert_eq!(audio.output_level, 20.0);
        assert_eq!(audio.echo, 10.0);
        assert_eq!(audio.surround, 0.0);
        assert_eq!(audio.dynamic_boost, 10.0);
        assert_eq!(audio.volume_leveling, 4.0);
        assert_eq!(audio.balance, -20.0);
        assert_eq!(audio.filter_q, 1.0);
    }

    fn load_edited_config(text: &str) -> (Config, Vec<String>, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "crosshairplus-config-import-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");
        fs::write(&path, text).unwrap();
        let (config, warnings) = Config::load_from_reporting(&path);
        (config, warnings, dir)
    }

    #[test]
    fn a_hand_edited_imported_profile_is_validated_on_load() {
        use crate::audio::apo_import::{BAND_FREQ_RANGE, BAND_GAIN_RANGE, PREAMP_RANGE};
        // `1e39` parses as an infinite f32, which no importer would ever produce.
        let (config, warnings, dir) = load_edited_config(
            r#"{"audio":{"imported":{"name":"  Hand edited  ","raw":"GraphicEQ: 20 99","parametric":false,"preamp":99.0,"bands":[{"freq":20.0,"gain":99.0},{"freq":1e39,"gain":1.0},{"freq":62.0,"gain":1.0}]}}}"#,
        );
        assert!(
            warnings.iter().any(|warning| warning.contains("were clamped")),
            "the load must report what it cleaned up: {warnings:?}"
        );
        let profile = config.audio.imported.as_ref().expect("a usable band keeps the profile");
        assert!((profile.preamp - PREAMP_RANGE.1).abs() < 0.001, "preamp {}", profile.preamp);
        assert_eq!(profile.bands.len(), 2, "the non-finite band is dropped: {:?}", profile.bands);
        assert!(profile.bands.iter().all(|band| band.freq.is_finite() && band.gain.is_finite()));
        assert!(profile
            .bands
            .iter()
            .all(|band| (BAND_FREQ_RANGE.0..=BAND_FREQ_RANGE.1).contains(&band.freq)));
        assert!(profile
            .bands
            .iter()
            .all(|band| (BAND_GAIN_RANGE.0..=BAND_GAIN_RANGE.1).contains(&band.gain)));
        assert_eq!(profile.name, "Hand edited", "the display name is trimmed");
        // Nothing bad reaches the curve the overlay and FxSound read.
        assert!(crate::audio::dsp_math::effective_bands(&config.audio).iter().all(|gain| gain.is_finite()));
        assert!(crate::audio::dsp_math::fx_bands(&config.audio).1.iter().all(|gain| gain.is_finite()));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_hand_edited_crash_recovery_snapshot_is_validated_before_it_restores() {
        use crate::audio::apo_import::{BAND_GAIN_RANGE, PREAMP_RANGE};
        let (mut config, warnings, dir) = load_edited_config(
            r#"{"slots":{"active":"slot-a","list":[{"id":"slot-a","name":"A","emoji":"🎯","crosshair":{"on":true},"display":{"on":true},"audio":{"on":true}}],"baseline":{"crosshair":{},"display":{},"audio":{"imported":{"name":"Snap","raw":"","parametric":false,"preamp":99.0,"bands":[{"freq":31.0,"gain":99.0},{"freq":1e39,"gain":1.0}]}}}}}"#,
        );
        assert!(!warnings.is_empty(), "a cleaned recovery snapshot must be reported");
        // The snapshot itself is cleaned on load, not only after it is used.
        let snapshot = config.slots.baseline.as_ref().expect("snapshot kept");
        let profile = snapshot.audio.imported.as_ref().expect("a usable band keeps the profile");
        assert!((profile.preamp - PREAMP_RANGE.1).abs() < 0.001, "preamp {}", profile.preamp);
        assert_eq!(profile.bands, vec![ImportedBand { freq: 31.0, gain: BAND_GAIN_RANGE.1 }]);

        // Startup recovery copies the snapshot over the live audio.
        assert!(crate::slots::restore_session_baseline(&mut config));
        assert_eq!(
            config.audio.imported.as_ref().expect("the cleaned profile is restored").bands,
            vec![ImportedBand { freq: 31.0, gain: BAND_GAIN_RANGE.1 }]
        );
        assert!(crate::audio::dsp_math::effective_bands(&config.audio).iter().all(|gain| gain.is_finite()));
        assert!(crate::audio::dsp_math::fx_bands(&config.audio).1.iter().all(|gain| gain.is_finite()));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_hand_edited_profile_with_an_invalid_preamp_is_dropped_on_load() {
        let (config, warnings, dir) = load_edited_config(
            r#"{"audio":{"imported":{"name":"Bad","raw":"","parametric":false,"preamp":1e39,"bands":[{"freq":31.0,"gain":2.0}]}}}"#,
        );
        assert!(config.audio.imported.is_none(), "a non-finite preamp must not reach the curve");
        assert!(crate::audio::dsp_math::effective_bands(&config.audio).iter().all(|gain| gain.is_finite()));
        // The dropped curve is explained, not just logged.
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("'Bad' was not applied because its preamp is not a valid number"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn cleanup_warnings_are_reported_once_per_load_and_do_not_repeat() {
        let (mut config, warnings, dir) = load_edited_config(
            r#"{"audio":{"imported":{"name":"Bad","raw":"","parametric":false,"preamp":1e39,"bands":[{"freq":31.0,"gain":2.0}]}}}"#,
        );
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        // Normalizing runs on every save and slot change; the cleaned config must
        // stay quiet so the report is never repeated.
        assert!(config.normalize().is_empty());
        assert!(config.normalize().is_empty());
        // Nor may the cleaned values warn again after a round trip through disk.
        let path = dir.join("saved.json");
        config.save_to(&path).unwrap();
        let (_, reloaded) = Config::load_from_reporting(&path);
        assert!(reloaded.is_empty(), "{reloaded:?}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_clean_config_loads_without_warnings() {
        let (config, warnings, dir) = load_edited_config(r#"{"audio":{"bands":[3.0,0,0,0,0,0,0,0,0,0]}}"#);
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(config.audio.bands[0], 3.0);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn corrupt_configs_quarantine_and_start_from_defaults() {
        for text in [
            "{\"version\":1,\"crosshair\":",
            "",
            "not json at all",
            "{\"crosshair\":\"not-an-object\"}",
            "{\"version\":1,\"crosshair\":{\"size\":\"huge\"}}",
        ] {
            let (config, warnings, dir) = load_edited_config(text);
            let path = dir.join("config.json");
            assert_eq!(config.crosshair.style, Config::default().crosshair.style, "{text}");
            assert!(config.crosshair.size.is_finite(), "{text}");
            let quarantine = corrupt_path(&path);
            assert!(quarantine.exists(), "a corrupt config must be preserved: {text}");
            assert_eq!(fs::read_to_string(&quarantine).unwrap(), text, "the quarantine must keep the damaged bytes: {text}");
            assert!(!backup_path(&path).exists(), "a corrupt load must not create a backup: {text}");
            assert!(!warnings.is_empty(), "the fallback must be explained: {text}");
            let _ = fs::remove_dir_all(&dir);
        }
    }

    #[test]
    fn corrupt_config_recovers_the_last_valid_backup() {
        let dir = std::env::temp_dir().join(format!(
            "crosshairplus-config-recover-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");
        let broken = "{not json";
        fs::write(&path, broken).unwrap();
        let mut good = Config::default();
        good.settings.start_with_windows = true;
        good.audio.bands[0] = 3.0;
        let good_text = serde_json::to_string_pretty(&good).unwrap();
        fs::write(backup_path(&path), &good_text).unwrap();

        let (config, warnings) = Config::load_from_reporting(&path);
        assert!(config.settings.start_with_windows);
        assert_eq!(config.audio.bands[0], 3.0);
        assert!(
            warnings.iter().any(|warning| warning.contains("recovered the last saved copy")
                && warning.contains("config.json.bak")
                && warning.contains("config.corrupt.json")),
            "{warnings:?}"
        );
        assert_eq!(fs::read_to_string(corrupt_path(&path)).unwrap(), broken);
        assert_eq!(fs::read_to_string(backup_path(&path)).unwrap(), good_text);

        config.save_to(&path).unwrap();
        assert_eq!(
            fs::read_to_string(backup_path(&path)).unwrap(),
            good_text,
            "saving after recovery must not replace the good backup with the corrupt live file"
        );
        let reparsed: Config = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert!(reparsed.settings.start_with_windows);
        assert!(!path.with_extension("tmp").exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn corrupt_config_with_a_bad_backup_falls_back_to_defaults() {
        let dir = std::env::temp_dir().join(format!(
            "crosshairplus-config-bad-backup-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");
        let broken = "not json";
        fs::write(&path, broken).unwrap();
        fs::write(backup_path(&path), "also not json").unwrap();

        let (config, warnings) = Config::load_from_reporting(&path);
        assert_eq!(config.crosshair.style, Config::default().crosshair.style);
        assert!(
            warnings.iter().any(|warning| warning.contains("default settings")),
            "{warnings:?}"
        );
        assert_eq!(fs::read_to_string(corrupt_path(&path)).unwrap(), broken);
        assert_eq!(fs::read_to_string(backup_path(&path)).unwrap(), "also not json");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn saving_never_leaves_a_zero_byte_or_temporary_config() {
        let dir = std::env::temp_dir().join(format!(
            "crosshairplus-config-atomic-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");
        let config = Config::default();
        config.save_to(&path).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.len() > 2, "a save must produce real JSON");
        assert!(serde_json::from_str::<Config>(&text).is_ok());
        assert!(!path.with_extension("tmp").exists(), "the temporary file must be renamed, not left behind");
        config.save_to(&path).unwrap();
        assert_eq!(
            fs::read_to_string(backup_path(&path)).unwrap(),
            text,
            "a later save must keep the previous file as .bak"
        );
        assert!(serde_json::from_str::<Config>(&std::fs::read_to_string(&path).unwrap()).is_ok());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn legacy_hide_on_rmb_migrates_and_round_trips_old_new_old() {
        let legacy = serde_json::json!({"version": 2, "crosshair": {"hideOnRmb": true}});
        let mut config: Config = serde_json::from_value(legacy).unwrap();
        config.normalize();
        assert_eq!(config.crosshair.aim_visibility.as_deref(), Some("hide"));
        assert!(config.crosshair.hide_on_rmb, "old builds must keep reading the legacy bool");

        let saved = serde_json::to_value(&config).unwrap();
        assert_eq!(saved["crosshair"]["aimVisibility"], "hide");
        assert_eq!(saved["crosshair"]["hideOnRmb"], true);

        let mut reparsed: Config = serde_json::from_value(saved).unwrap();
        reparsed.normalize();
        assert_eq!(reparsed.crosshair.aim_visibility.as_deref(), Some("hide"));
        assert!(reparsed.crosshair.hide_on_rmb);
    }

    #[test]
    fn osd_toast_defaults_on_and_cycle_hotkey_defaults_empty() {
        let config = Config::default();
        assert!(config.settings.osd_toast);
        assert!(config.settings.cycle_hotkey.is_empty());
        let saved = serde_json::to_value(&config).unwrap();
        assert_eq!(saved["settings"]["osdToast"], true);
        assert_eq!(saved["settings"]["cycleHotkey"], "");
        let parsed: Config = serde_json::from_value(serde_json::json!({"settings": {}})).unwrap();
        assert!(parsed.settings.osd_toast, "a missing OSD field must stay enabled");
    }

    #[test]
    fn slot_names_are_24_chars_and_emoji_set_is_unique() {
        let mut config = Config::default();
        config.slots.list[0].name = "x".repeat(40);
        config.normalize();
        assert_eq!(config.slots.list[0].name.chars().count(), MAX_SLOT_NAME);

        let emoji = visible_emoji();
        assert_eq!(emoji.len(), 64);
        let unique: std::collections::HashSet<_> = emoji.iter().collect();
        assert_eq!(unique.len(), emoji.len());
    }
}

#[cfg(test)]
pub fn visible_emoji() -> Vec<&'static str> {
    vec![
        "🎯", "⚡", "🎮", "🕹️", "🔫", "🏹", "🛡️", "🎬", "🌙", "☀️", "🌿", "🔥", "❄️", "💀", "👾", "🚀",
        "🎧", "🔊", "🎵", "🎚️", "🥇", "🏆", "⭐", "✨", "💎", "💡", "🧠", "👁️", "🦇", "🐺", "🦊", "🐉",
        "🍿", "🎞️", "📺", "🖥️", "🖱️", "⌨️", "🎨", "🛠️", "⚙️", "🔧", "🧩", "🧊", "🌊", "🌪️", "🌌", "🪐",
        "☄️", "🛸", "👑", "💥", "🎪", "🎲", "♟️", "🏁", "🚦", "🟢", "🔵", "🔴", "🟣", "🟡", "⚪", "⚫",
    ]
}
