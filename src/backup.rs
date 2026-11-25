//! Single-file backup and restore for user-created content.
//!
//! The archive deliberately contains only portable user content: custom
//! presets, slot definitions, imported crosshair files and the imported EQ
//! profile. Machine-specific settings and the current display/audio calibration
//! remain in the live config.

use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipArchive, ZipWriter};

use crate::config::{
    builtin_audio_presets, builtin_crosshair_presets, builtin_display_presets, AudioPreset,
    Config, CustomPresets, ImportedProfile, Slot, MAX_SLOT_NAME, MAX_SLOTS, RETIRED_PRESET_IDS,
};
use crate::slots;

pub const BACKUP_FORMAT: u32 = 1;
const MAX_BACKUP_IMAGES: usize = 500;
const MAX_BACKUP_ENTRIES: usize = 700;
const MAX_BACKUP_UNCOMPRESSED: u64 = 256 * 1024 * 1024;
const MAX_BACKUP_ENTRY: u64 = 64 * 1024 * 1024;
const MAX_BACKUP_JSON: u64 = 8 * 1024 * 1024;
const MAX_RESTORED_PRESETS: usize = 100;
/// Safety copy of the live config, written just before a restore replaces it.
pub const PRE_RESTORE_FILE: &str = "config.pre-restore.json";

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct BackupManifest {
    pub format: u32,
    pub app_version: String,
    pub created_unix: u64,
    pub images: Vec<BackupImage>,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct BackupImage {
    /// Original absolute path, used only to rewrite references on restore.
    pub original: String,
    /// Safe path inside the ZIP.
    pub archive: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct BackupUser {
    pub custom: CustomPresets,
    pub slots: Vec<Slot>,
    pub audio_import: Option<ImportedProfile>,
    pub crosshair_image: Option<String>,
    pub crosshair_image_name: Option<String>,
}

#[derive(Serialize, Clone, Debug, Default)]
#[serde(rename_all = "camelCase")]
pub struct BackupPreview {
    pub app_version: String,
    pub created_unix: u64,
    pub crosshair_presets: usize,
    pub display_presets: usize,
    pub audio_presets: usize,
    pub slots: usize,
    pub images: usize,
    pub eq_name: Option<String>,
    pub warnings: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct RestoreReport {
    pub preview: BackupPreview,
    pub warnings: Vec<String>,
}

struct Inspected {
    manifest: BackupManifest,
    user: BackupUser,
}

fn preview(manifest: &BackupManifest, user: &BackupUser, warnings: Vec<String>) -> BackupPreview {
    BackupPreview {
        app_version: manifest.app_version.clone(),
        created_unix: manifest.created_unix,
        crosshair_presets: user.custom.crosshair.len(),
        display_presets: user.custom.display.len(),
        audio_presets: user.custom.audio.len(),
        slots: user.slots.len(),
        images: manifest.images.len(),
        eq_name: user.audio_import.as_ref().map(|profile| profile.name.clone()),
        warnings,
    }
}

fn read_entry(archive: &mut ZipArchive<File>, name: &str, limit: u64) -> Result<Vec<u8>, String> {
    let file = archive.by_name(name).map_err(|err| format!("backup is missing {name}: {err}"))?;
    if file.encrypted() {
        return Err(format!("backup entry {name} is encrypted"));
    }
    if file.size() > limit {
        return Err(format!("backup entry {name} is too large"));
    }
    let mut bytes = Vec::new();
    file.take(limit + 1)
        .read_to_end(&mut bytes)
        .map_err(|err| format!("could not read {name}: {err}"))?;
    if bytes.len() as u64 > limit {
        return Err(format!("backup entry {name} is too large"));
    }
    Ok(bytes)
}

fn safe_archive_path(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('/')
        && !name.contains('\\')
        && !name.split('/').any(|part| part.is_empty() || part == "." || part == "..")
}

fn inspect(path: &Path) -> Result<Inspected, String> {
    let file = File::open(path).map_err(|err| format!("could not open backup: {err}"))?;
    let mut archive = ZipArchive::new(file).map_err(|err| format!("not a valid ZIP backup: {err}"))?;
    if archive.len() == 0 || archive.len() > MAX_BACKUP_ENTRIES {
        return Err("backup has an unsupported number of entries".into());
    }
    let mut total = 0u64;
    let mut names = HashSet::new();
    for index in 0..archive.len() {
        let entry = archive.by_index(index).map_err(|err| format!("invalid backup entry: {err}"))?;
        let name = entry.name().to_string();
        if entry.is_dir() || !safe_archive_path(&name) {
            return Err(format!("backup contains an unsafe path: {name}"));
        }
        if entry.encrypted() {
            return Err(format!("backup entry {name} is encrypted"));
        }
        total = total.saturating_add(entry.size());
        if total > MAX_BACKUP_UNCOMPRESSED {
            return Err("backup is too large after extraction".into());
        }
        names.insert(name);
    }
    for required in ["manifest.json", "user.json"] {
        if !names.contains(required) {
            return Err(format!("backup is missing {required}"));
        }
    }
    for name in &names {
        if name != "manifest.json" && name != "user.json" && !name.starts_with("crosshairs/") {
            return Err(format!("backup contains an unexpected entry: {name}"));
        }
    }

    let manifest: BackupManifest = serde_json::from_slice(&read_entry(&mut archive, "manifest.json", MAX_BACKUP_JSON)?)
        .map_err(|err| format!("backup manifest is invalid: {err}"))?;
    if manifest.format != BACKUP_FORMAT {
        return Err(format!("backup format {} is not supported", manifest.format));
    }
    if manifest.images.len() > MAX_BACKUP_IMAGES {
        return Err("backup contains too many images".into());
    }
    let user: BackupUser = serde_json::from_slice(&read_entry(&mut archive, "user.json", MAX_BACKUP_JSON)?)
        .map_err(|err| format!("backup user data is invalid: {err}"))?;

    let mut listed = HashSet::new();
    for image in &manifest.images {
        if !safe_archive_path(&image.archive) || !image.archive.starts_with("crosshairs/") {
            return Err(format!("backup image path is unsafe: {}", image.archive));
        }
        if !names.contains(&image.archive) {
            return Err(format!("backup image is missing: {}", image.archive));
        }
        listed.insert(image.archive.clone());
    }
    for name in names.iter().filter(|name| name.starts_with("crosshairs/")) {
        if !listed.contains(name) {
            return Err(format!("backup contains an unlisted image: {name}"));
        }
    }
    Ok(Inspected { manifest, user })
}

pub fn inspect_backup(path: &Path) -> Result<BackupPreview, String> {
    let inspected = inspect(path)?;
    Ok(preview(&inspected.manifest, &inspected.user, Vec::new()))
}

fn safe_image_name(original: &str, index: usize) -> String {
    let path = Path::new(original);
    let ext = path
        .extension()
        .and_then(|value| value.to_str())
        .map(|value| value.chars().filter(|ch| ch.is_ascii_alphanumeric()).collect::<String>().to_ascii_lowercase())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "bin".into());
    let stem = path
        .file_stem()
        .and_then(|value| value.to_str())
        .map(|value| {
            value
                .chars()
                .map(|ch| if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' { ch } else { '_' })
                .collect::<String>()
        })
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "image".into());
    format!("{index:04}-{stem}.{ext}")
}

/// Whether `path` holds exactly these bytes, checked by length first so a
/// mismatch costs one metadata call rather than a full read.
fn file_matches(path: &Path, bytes: &[u8]) -> bool {
    fs::metadata(path).map(|meta| meta.len() == bytes.len() as u64).unwrap_or(false)
        && fs::read(path).map(|existing| existing == bytes).unwrap_or(false)
}

/// The extension a picture is stored under. The renderer picks a decoder from
/// the path, so two files that merely share bytes under different extensions
/// are not interchangeable and must keep their own entry.
fn image_extension_label(path: &Path) -> String {
    path.extension()
        .and_then(|value| value.to_str())
        .map(|value| value.to_ascii_lowercase())
        .unwrap_or_default()
}

/// Finds an existing crosshair file with exactly these bytes under this
/// extension, so restoring the same backup twice reuses its picture instead of
/// writing another copy whose name would imply a different format.
fn find_identical_image(dir: &Path, bytes: &[u8], extension: &str) -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> = fs::read_dir(dir)
        .ok()?
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.is_file() && image_extension_label(path) == extension)
        .collect();
    candidates.sort();
    candidates.into_iter().find(|candidate| file_matches(candidate, bytes))
}

/// One stored picture: the archive entry that holds its bytes and the file the
/// backup took it from (kept so a manifest stays readable).
struct StoredImage {
    entry: String,
    source: String,
}

/// The pictures of one backup, keyed by extension plus their own bytes.
///
/// Identical files share one [`StoredImage`], so the archive holds one entry per
/// distinct picture and every scanned file — duplicates included — points at the
/// entry that holds it. Because the key is the content itself, two files can
/// only merge when they are byte-for-byte equal; there is no hash to collide.
/// The extension is part of the key so a picture is never renamed to a format
/// the renderer would decode differently.
#[derive(Default)]
struct ImageStore {
    images: HashMap<(String, Vec<u8>), StoredImage>,
    by_path: HashMap<String, String>,
}

impl ImageStore {
    /// The archive entry holding this reference, or `None` when the backup did
    /// not store the file. This is the only way a reference is written, so a
    /// reference can never name an entry the archive does not contain.
    fn reference(&self, path: Option<&str>) -> Option<String> {
        path.and_then(|path| self.by_path.get(path).cloned())
    }

    /// Bytes and entries in archive order, which is also the order they are
    /// written in.
    fn ordered(&self) -> Vec<(&Vec<u8>, &StoredImage)> {
        let mut images: Vec<(&Vec<u8>, &StoredImage)> =
            self.images.iter().map(|((_, bytes), image)| (bytes, image)).collect();
        images.sort_by(|a, b| a.1.entry.cmp(&b.1.entry));
        images
    }
}

/// Reads every crosshair file once: identical bytes are stored once, every source
/// path points at the entry that holds them, and the archive limits are enforced
/// before anything is written.
fn plan_images(dir: &Path) -> Result<ImageStore, String> {
    let mut files = Vec::new();
    if dir.exists() {
        let entries = fs::read_dir(dir).map_err(|err| format!("could not read crosshair images: {err}"))?;
        for entry in entries.flatten() {
            let file_path = entry.path();
            if file_path.is_file() {
                files.push(file_path);
            }
        }
    }
    files.sort();
    let mut store = ImageStore::default();
    let mut total = 0u64;
    for file_path in files {
        let size = fs::metadata(&file_path)
            .map_err(|err| format!("could not read {}: {err}", file_path.display()))?
            .len();
        if size > MAX_BACKUP_ENTRY {
            return Err(format!("{} is too large to back up.", file_path.display()));
        }
        let bytes = fs::read(&file_path).map_err(|err| format!("could not read {}: {err}", file_path.display()))?;
        let length = bytes.len() as u64;
        let source = file_path.to_string_lossy().to_string();
        let identity = (image_extension_label(&file_path), bytes);
        let entry = match store.images.get(&identity) {
            Some(image) => image.entry.clone(),
            None => {
                if store.images.len() >= MAX_BACKUP_IMAGES {
                    return Err("There are too many crosshair images to back up.".into());
                }
                total = total.saturating_add(length);
                if total > MAX_BACKUP_UNCOMPRESSED {
                    return Err("The crosshair images are too large to back up.".into());
                }
                let entry = format!("crosshairs/{}", safe_image_name(&source, store.images.len()));
                store.images.insert(identity, StoredImage { entry: entry.clone(), source: source.clone() });
                entry
            }
        };
        store.by_path.insert(source, entry);
    }
    Ok(store)
}

pub fn create_backup(path: &Path, config: &Config, data_dir: &Path) -> Result<BackupPreview, String> {
    let store = plan_images(&data_dir.join("crosshairs"))?;
    let mut warnings = Vec::new();
    let mut user = BackupUser {
        custom: config.custom.clone(),
        slots: config.slots.list.clone(),
        audio_import: config.audio.imported.clone(),
        crosshair_image: config.crosshair.image.clone(),
        crosshair_image_name: config.crosshair.image_name.clone(),
    };
    // Every reference becomes the entry that stores its bytes. A picture the
    // backup did not store is dropped here, once, together with its display
    // name — the archive never carries a path it cannot resolve.
    let mut dropped = 0;
    for preset in &mut user.custom.crosshair {
        if preset.config.image.is_some() {
            preset.config.image = store.reference(preset.config.image.as_deref());
            if preset.config.image.is_none() {
                preset.config.image_name = None;
                dropped += 1;
            }
        }
    }
    if user.crosshair_image.is_some() {
        user.crosshair_image = store.reference(user.crosshair_image.as_deref());
        if user.crosshair_image.is_none() {
            user.crosshair_image_name = None;
            dropped += 1;
        }
    }
    if dropped > 0 {
        warnings.push(format!(
            "{dropped} crosshair image(s) are missing from the data folder and were left out of the backup."
        ));
    }

    let created_unix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0);
    let ordered = store.ordered();
    let manifest = BackupManifest {
        format: BACKUP_FORMAT,
        app_version: crate::config::APP_VERSION.into(),
        created_unix,
        images: ordered
            .iter()
            .map(|(_, image)| BackupImage { archive: image.entry.clone(), original: image.source.clone() })
            .collect(),
    };

    let temp = path.with_extension("zip.tmp");
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|err| format!("could not create backup folder: {err}"))?;
    }
    let file = File::create(&temp).map_err(|err| format!("could not create backup: {err}"))?;
    let mut archive = ZipWriter::new(file);
    let options = SimpleFileOptions::default()
        .compression_method(CompressionMethod::Deflated)
        .unix_permissions(0o644);
    let write_result = (|| -> Result<(), String> {
        // Bytes first: a reference is only written for an entry that already
        // holds its picture.
        for (bytes, image) in &ordered {
            archive
                .start_file(&image.entry, options)
                .map_err(|err| format!("could not add image to backup: {err}"))?;
            archive.write_all(bytes).map_err(|err| format!("could not add image to backup: {err}"))?;
        }
        let manifest_json = serde_json::to_vec_pretty(&manifest).map_err(|err| format!("manifest encoding failed: {err}"))?;
        let user_json = serde_json::to_vec_pretty(&user).map_err(|err| format!("backup data encoding failed: {err}"))?;
        archive
            .start_file("manifest.json", options)
            .map_err(|err| format!("could not write manifest: {err}"))?;
        archive.write_all(&manifest_json).map_err(|err| format!("could not write manifest: {err}"))?;
        archive
            .start_file("user.json", options)
            .map_err(|err| format!("could not write user data: {err}"))?;
        archive.write_all(&user_json).map_err(|err| format!("could not write user data: {err}"))?;
        archive.finish().map_err(|err| format!("could not finish backup: {err}"))?;
        Ok(())
    })();
    if let Err(err) = write_result {
        let _ = fs::remove_file(&temp);
        return Err(err);
    }
    let finalize = (|| -> Result<(), String> {
        if path.exists() {
            fs::remove_file(path).map_err(|err| format!("could not replace the old backup: {err}"))?;
        }
        fs::rename(&temp, path).map_err(|err| format!("could not save the backup: {err}"))
    })();
    if let Err(err) = finalize {
        let _ = fs::remove_file(&temp);
        return Err(err);
    }
    Ok(preview(&manifest, &user, warnings))
}

fn unique_id(prefix: &str, index: usize, used: &mut HashSet<String>) -> String {
    let mut id = format!("{prefix}-{index}");
    while used.contains(&id) {
        id.push('x');
    }
    used.insert(id.clone());
    id
}

/// Assigns the restored ID for each preset of one module and records every
/// rename, so slot references can follow their preset to the new ID.
///
/// Generated IDs are checked against every ID the archive mentions in this
/// module, not only the ones already restored: a replacement must never take
/// an ID that another archived preset owns, including one the preset cap drops.
fn plan_preset_ids(
    originals: &[String],
    prefix: &str,
    reserved: &HashSet<String>,
) -> (Vec<String>, HashMap<String, String>) {
    let mut taken: HashSet<String> = originals.iter().cloned().collect();
    let mut claimed = HashSet::new();
    let mut remap = HashMap::new();
    let mut ids = Vec::new();
    for (index, original) in originals.iter().take(MAX_RESTORED_PRESETS).enumerate() {
        let id = if original.trim().is_empty() || reserved.contains(original) || !claimed.insert(original.clone()) {
            unique_id(prefix, index, &mut taken)
        } else {
            original.clone()
        };
        remap.entry(original.clone()).or_insert_with(|| id.clone());
        ids.push(id);
    }
    (ids, remap)
}

fn remap_image(path: Option<&str>, map: &HashMap<String, PathBuf>, warnings: &mut Vec<String>) -> Option<String> {
    let path = path?;
    match map.get(path) {
        Some(final_path) => Some(final_path.to_string_lossy().to_string()),
        None => {
            warnings.push(format!("Imported image is not in the backup: {path}"));
            None
        }
    }
}

/// Preset IDs are only unique inside their own module, so a renamed preset is
/// tracked per module and slots can follow it instead of losing the reference.
struct PresetRemap {
    crosshair: HashMap<String, String>,
    display: HashMap<String, String>,
    audio: HashMap<String, String>,
}

/// Built-in preset IDs per module: a slot may name one without the archive
/// carrying a copy of it.
struct BuiltinIds {
    crosshair: HashSet<String>,
    display: HashSet<String>,
    audio: HashSet<String>,
}

impl BuiltinIds {
    fn load() -> Self {
        Self {
            crosshair: builtin_crosshair_presets().into_iter().map(|preset| preset.id).collect(),
            display: builtin_display_presets().into_iter().map(|preset| preset.id).collect(),
            audio: builtin_audio_presets().into_iter().map(|preset| preset.id).collect(),
        }
    }
}

/// The preset side of a restore, shared by preset and slot sanitizing.
struct RestoredPresets {
    custom: CustomPresets,
    remap: PresetRemap,
    builtins: BuiltinIds,
}

fn sanitize_presets(custom: CustomPresets, warnings: &mut Vec<String>) -> RestoredPresets {
    let builtins = BuiltinIds::load();
    // Retired IDs are reserved as well: normalize would rewrite them.
    let reserved_crosshair: HashSet<String> = builtins
        .crosshair
        .iter()
        .cloned()
        .chain(RETIRED_PRESET_IDS.iter().map(|id| id.to_string()))
        .collect();
    for (count, label) in [
        (custom.crosshair.len(), "aim"),
        (custom.display.len(), "display"),
        (custom.audio.len(), "sound"),
    ] {
        if count > MAX_RESTORED_PRESETS {
            warnings.push(format!("Only the first {MAX_RESTORED_PRESETS} {label} presets were restored."));
        }
    }
    // Each module keeps its own set: an aim preset and a sound preset may
    // legitimately share one ID, exactly as they can in the live config.
    let crosshair_originals: Vec<String> = custom.crosshair.iter().map(|preset| preset.id.clone()).collect();
    let (crosshair_ids, crosshair_remap) = plan_preset_ids(&crosshair_originals, "restored-crosshair", &reserved_crosshair);
    let display_originals: Vec<String> = custom.display.iter().map(|preset| preset.id.clone()).collect();
    let (display_ids, display_remap) = plan_preset_ids(&display_originals, "restored-display", &builtins.display);
    let audio_originals: Vec<String> = custom.audio.iter().map(|preset| preset.id.clone()).collect();
    let (audio_ids, audio_remap) = plan_preset_ids(&audio_originals, "restored-audio", &builtins.audio);
    let mut out = CustomPresets::default();

    for (mut preset, id) in custom.crosshair.into_iter().zip(crosshair_ids) {
        preset.builtin = false;
        preset.id = id;
        if preset.name.trim().is_empty() {
            preset.name = "Restored aim preset".into();
        }
        preset.config.normalize();
        out.crosshair.push(preset);
    }
    for (mut preset, id) in custom.display.into_iter().zip(display_ids) {
        preset.builtin = false;
        preset.id = id;
        if preset.name.trim().is_empty() {
            preset.name = "Restored display preset".into();
        }
        preset.values.normalize();
        out.display.push(preset);
    }
    for (mut preset, id) in custom.audio.into_iter().zip(audio_ids) {
        preset.builtin = false;
        preset.id = id;
        if preset.name.trim().is_empty() {
            preset.name = "Restored sound preset".into();
        }
        normalize_audio_preset(&mut preset);
        out.audio.push(preset);
    }
    RestoredPresets {
        custom: out,
        remap: PresetRemap { crosshair: crosshair_remap, display: display_remap, audio: audio_remap },
        builtins,
    }
}

fn normalize_audio_preset(preset: &mut AudioPreset) {
    for band in &mut preset.bands {
        *band = band.clamp(-12.0, 12.0);
    }
    preset.bass = preset.bass.clamp(0.0, 10.0);
    preset.treble = preset.treble.clamp(0.0, 10.0);
    preset.output_level = preset.output_level.clamp(-20.0, 20.0);
    preset.echo = preset.echo.clamp(0.0, 10.0);
    preset.surround = preset.surround.clamp(0.0, 10.0);
    preset.dynamic_boost = preset.dynamic_boost.clamp(0.0, 10.0);
    preset.volume_leveling = preset.volume_leveling.clamp(0.0, 4.0);
    preset.balance = preset.balance.clamp(-20.0, 20.0);
    preset.filter_q = preset.filter_q.clamp(1.0, 3.0);
}

/// Follows a renamed preset in one slot module.
///
/// Every restored preset has a remap entry, so an ID that is not in the remap
/// can only be a built-in. Anything else is a reference that did not survive
/// the archive — dropped by the preset cap, or already dangling — and is
/// cleared with the usual warning. It must never attach to a renamed preset
/// that happens to own that ID now.
fn restore_slot_preset(
    slot_name: &str,
    module: &str,
    preset: &mut Option<String>,
    remap: &HashMap<String, String>,
    builtin: &HashSet<String>,
    warnings: &mut Vec<String>,
) {
    let Some(reference) = preset.as_deref() else { return };
    if let Some(restored) = remap.get(reference) {
        *preset = Some(restored.clone());
    } else if !builtin.contains(reference) {
        warnings.push(format!(
            "{slot_name} preset '{reference}' is missing; the slot will use the current {module} settings."
        ));
        *preset = None;
    }
}

/// Rebuilds `candidate.slots.list` from the archive in order, so restored
/// hotkeys are checked against the slots that actually precede them.
fn sanitize_slots(
    candidate: &mut Config,
    restored: Vec<Slot>,
    remap: &PresetRemap,
    builtins: &BuiltinIds,
    warnings: &mut Vec<String>,
) {
    let restored_count = restored.len();
    let mut used_ids = HashSet::new();
    let mut used_emojis = HashSet::new();
    let emoji_fallback = ["🎯", "⚡", "🎮", "🕹️", "🔫", "🛡️", "🎬", "🌙"];
    candidate.slots.list.clear();
    for (index, mut slot) in restored.into_iter().take(MAX_SLOTS).enumerate() {
        if slot.id.trim().is_empty() || !used_ids.insert(slot.id.clone()) {
            slot.id = unique_id("restored-slot", index, &mut used_ids);
        }
        slot.name = slot.name.trim().chars().take(MAX_SLOT_NAME).collect();
        if slot.name.is_empty() {
            slot.name = format!("Restored slot {}", index + 1);
        }
        if slot.emoji.trim().is_empty() || !used_emojis.insert(slot.emoji.clone()) {
            slot.emoji = emoji_fallback
                .iter()
                .find(|emoji| !used_emojis.contains(**emoji))
                .copied()
                .unwrap_or("🎯")
                .to_string();
            used_emojis.insert(slot.emoji.clone());
        }
        restore_slot_preset(
            &slot.name,
            "aim",
            &mut slot.crosshair.preset,
            &remap.crosshair,
            &builtins.crosshair,
            warnings,
        );
        restore_slot_preset(
            &slot.name,
            "display",
            &mut slot.display.preset,
            &remap.display,
            &builtins.display,
            warnings,
        );
        restore_slot_preset(
            &slot.name,
            "sound",
            &mut slot.audio.preset,
            &remap.audio,
            &builtins.audio,
            warnings,
        );
        if let Some(hotkey) = slot.hotkey.clone() {
            if slots::validate_hotkey(candidate, &hotkey, slots::HotkeyOwner::Slot(&slot.id)).is_err() {
                warnings.push(format!("Hotkey {} for {} is already in use; it was cleared.", hotkey, slot.name));
                slot.hotkey = None;
            }
        }
        candidate.slots.list.push(slot);
    }
    if restored_count > MAX_SLOTS {
        warnings.push(format!("Only the first {MAX_SLOTS} slots were restored."));
    }
}

pub fn restore_backup(
    path: &Path,
    config: &mut Config,
    store_path: &Path,
    data_dir: &Path,
) -> Result<RestoreReport, String> {
    let inspected = inspect(path)?;
    let mut warnings = Vec::new();
    // The report describes the archive, so count its content before that
    // content is moved into the restored config.
    let mut summary = preview(&inspected.manifest, &inspected.user, Vec::new());
    let Inspected { manifest, user } = inspected;
    let BackupUser { custom, slots: restored_slots, audio_import, crosshair_image, crosshair_image_name } = user;
    let mut candidate = config.clone();
    // A restored slot list must never inherit a half-active slot session.
    slots::deactivate_slot(&mut candidate);
    candidate.slots.active = None;
    candidate.slots.baseline = None;

    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or(0);
    let stage = data_dir.join(format!(".restore-{stamp}"));
    fs::create_dir_all(&stage).map_err(|err| format!("could not create restore staging folder: {err}"))?;
    let mut archive = ZipArchive::new(File::open(path).map_err(|err| format!("could not reopen backup: {err}"))?)
        .map_err(|err| format!("not a valid ZIP backup: {err}"))?;
    // Staged files, moved into place only after the config is ready.
    let mut staged: Vec<(PathBuf, PathBuf)> = Vec::new();
    // Extension plus content -> the file that now holds it, so one archive
    // cannot restore the same picture twice and never reuses a file under a
    // different extension.
    let mut resolved: HashMap<(String, Vec<u8>), PathBuf> = HashMap::new();
    // Archive entry (and the original path, for archives written before
    // references named entries) -> the restored file.
    let mut image_map: HashMap<String, PathBuf> = HashMap::new();
    let crosshairs = data_dir.join("crosshairs");
    fs::create_dir_all(&crosshairs).map_err(|err| format!("could not create crosshair folder: {err}"))?;
    let extraction = (|| -> Result<(), String> {
        for (index, image) in manifest.images.iter().enumerate() {
            let bytes = read_entry(&mut archive, &image.archive, MAX_BACKUP_ENTRY)?;
            let identity = (image_extension_label(Path::new(&image.archive)), bytes);
            // Reuse a picture already in the folder (a second restore of this
            // archive) or one staged by this restore, instead of copying it.
            let final_path = if let Some(existing) = resolved.get(&identity) {
                existing.clone()
            } else {
                let existing = match find_identical_image(&crosshairs, &identity.1, &identity.0) {
                    Some(existing) => existing,
                    None => {
                        let base = Path::new(&image.archive)
                            .file_name()
                            .and_then(|value| value.to_str())
                            .unwrap_or("image.bin");
                        // The index keeps staged names unique even when two
                        // entries share a file name.
                        let staged_path = stage.join(format!("{index:04}-{base}"));
                        fs::write(&staged_path, &identity.1)
                            .map_err(|err| format!("could not stage {}: {err}", image.archive))?;
                        let final_path = crosshairs.join(format!("restore-{stamp}-{index:04}-{base}"));
                        staged.push((staged_path, final_path.clone()));
                        final_path
                    }
                };
                resolved.insert(identity, existing.clone());
                existing
            };
            image_map.insert(image.archive.clone(), final_path.clone());
            if !image.original.is_empty() {
                image_map.insert(image.original.clone(), final_path);
            }
        }
        Ok(())
    })();
    if let Err(err) = extraction {
        let _ = fs::remove_dir_all(&stage);
        return Err(err);
    }

    let RestoredPresets { custom: restored_custom, remap: preset_remap, builtins: preset_builtins } =
        sanitize_presets(custom, &mut warnings);
    candidate.custom = restored_custom;
    if candidate
        .crosshair
        .active_preset
        .as_ref()
        .is_some_and(|id| candidate.find_crosshair_preset(id).is_none())
    {
        candidate.crosshair.active_preset = None;
    }
    if candidate
        .display
        .active_preset
        .as_ref()
        .is_some_and(|id| candidate.find_display_preset(id).is_none())
    {
        candidate.display.active_preset = None;
    }
    for preset in &mut candidate.custom.crosshair {
        let remapped = remap_image(preset.config.image.as_deref(), &image_map, &mut warnings);
        preset.config.image = remapped;
    }
    candidate.crosshair.image = remap_image(crosshair_image.as_deref(), &image_map, &mut warnings);
    candidate.crosshair.image_name = if candidate.crosshair.image.is_some() {
        crosshair_image_name
    } else {
        None
    };
    candidate.audio.imported = audio_import;
    // The live sound settings are kept as they are, so no restored sound preset
    // is marked active.
    candidate.audio.active_preset = None;
    sanitize_slots(&mut candidate, restored_slots, &preset_remap, &preset_builtins, &mut warnings);
    // One sanitize for the archive's profile, the same one a config load runs;
    // its report joins the other restore warnings.
    warnings.extend(candidate.normalize());

    // Keep the current config for manual rollback, then move staged images
    // into place before committing the new config.
    if store_path.exists() {
        let rollback = pre_restore_path(data_dir);
        if let Err(err) = fs::copy(store_path, &rollback) {
            let _ = fs::remove_dir_all(&stage);
            return Err(format!("could not create a safety copy: {err}"));
        }
    }
    let mut moved = Vec::new();
    for (from, to) in &staged {
        if let Err(err) = fs::rename(from, to) {
            for path in &moved {
                let _ = fs::remove_file(path);
            }
            let _ = fs::remove_dir_all(&stage);
            return Err(format!("could not finish restoring images: {err}"));
        }
        moved.push(to.clone());
    }
    let _ = fs::remove_dir_all(&stage);
    if let Err(err) = candidate.save_to(store_path) {
        for path in &moved {
            let _ = fs::remove_file(path);
        }
        return Err(format!("could not save restored config: {err}"));
    }
    *config = candidate;
    summary.warnings = warnings.clone();
    Ok(RestoreReport { preview: summary, warnings })
}

/// The safety copy left by the most recent restore.
pub fn pre_restore_path(data_dir: &Path) -> PathBuf {
    data_dir.join(PRE_RESTORE_FILE)
}

/// Whether there is a restore available to undo.
pub fn restore_undo_available(data_dir: &Path) -> bool {
    pre_restore_path(data_dir).is_file()
}

/// Reverts the last restore from the copy it saved before replacing the config.
///
/// The copy is parsed strictly, the reverted config is written first, and the
/// copy is dropped last: a failed undo leaves both the live config and the undo
/// path intact, and a successful one cannot be replayed.
pub fn undo_last_restore(store_path: &Path, data_dir: &Path) -> Result<(Config, Vec<String>), String> {
    let rollback = pre_restore_path(data_dir);
    if !rollback.is_file() {
        return Err("There is no restore to undo.".into());
    }
    // The undo copy is a full config, so it goes through the same load-time
    // cleanup; whatever that had to drop is reported back to the caller.
    let (reverted, warnings) = Config::load_strict(&rollback)?;
    reverted
        .save_to(store_path)
        .map_err(|err| format!("Could not save the reverted config: {err}"))?;
    fs::remove_file(&rollback).map_err(|err| {
        format!("The config was reverted but the undo copy could not be removed: {err}")
    })?;
    Ok((reverted, warnings))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::apo_import::{
        sanitize_imported, BAND_FREQ_RANGE, BAND_GAIN_RANGE, MAX_IMPORT_BANDS, MAX_IMPORT_NAME, MAX_IMPORT_RAW,
        PREAMP_RANGE,
    };
    use crate::config::{CrosshairPreset, ImportedBand};

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(name: &str) -> Self {
            let stamp = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
            let path = std::env::temp_dir().join(format!("crosshair-plus-backup-{name}-{}-{stamp}", std::process::id()));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn sample_config(data: &Path) -> Config {
        let images = data.join("crosshairs");
        fs::create_dir_all(&images).unwrap();
        let image = images.join("sample.png");
        fs::write(&image, b"not really a png").unwrap();
        let mut config = Config::default();
        config.crosshair.image = Some(image.to_string_lossy().to_string());
        config.crosshair.image_name = Some("sample.png".into());
        config.custom.crosshair.push(CrosshairPreset {
            id: "custom-aim".into(),
            name: "My aim".into(),
            builtin: false,
            config: crate::config::CrosshairConfig {
                image: Some(image.to_string_lossy().to_string()),
                ..Default::default()
            },
        });
        config.audio.imported = Some(ImportedProfile {
            name: "Desk EQ".into(),
            raw: "GraphicEQ: 31 2".into(),
            bands: vec![ImportedBand { freq: 31.0, gain: 2.0 }],
            parametric: false,
            preamp: -1.0,
        });
        config.slots.list[0].hotkey = Some("F9".into());
        config
    }

    #[test]
    fn round_trip_restores_custom_content_and_images() {
        let source = TempDir::new("source");
        let target = TempDir::new("target");
        let config = sample_config(&source.0);
        let backup_path = source.0.join("backup.zip");
        let created = create_backup(&backup_path, &config, &source.0).unwrap();
        assert_eq!(created.crosshair_presets, 1);
        assert_eq!(created.images, 1);
        let inspected = inspect_backup(&backup_path).unwrap();
        assert_eq!(inspected.eq_name.as_deref(), Some("Desk EQ"));

        let mut target_config = Config::default();
        let store = target.0.join("config.json");
        target_config.save_to(&store).unwrap();
        let report = restore_backup(&backup_path, &mut target_config, &store, &target.0).unwrap();
        assert_eq!(report.preview.crosshair_presets, 1);
        assert_eq!(target_config.custom.crosshair.len(), 1);
        assert_eq!(target_config.audio.imported.as_ref().unwrap().name, "Desk EQ");
        let image = target_config.crosshair.image.as_ref().expect("active image restored");
        assert!(Path::new(image).exists());
        assert!(image.contains("crosshairs"));
    }

    #[test]
    fn shared_preset_id_restores_in_display_and_audio_with_slot_references_intact() {
        use crate::config::{AudioPreset, DisplayPreset, DisplayValues, SlotModule};

        let source = TempDir::new("shared-preset");
        let mut config = Config::default();
        config.custom.display.push(DisplayPreset {
            id: "custom-shared".into(),
            name: "My screen".into(),
            builtin: false,
            values: DisplayValues { brightness: 12.0, ..Default::default() },
        });
        config.custom.audio.push(AudioPreset {
            id: "custom-shared".into(),
            name: "My sound".into(),
            builtin: false,
            bands: [1.5; 10],
            ..Default::default()
        });
        config.slots.list = vec![Slot {
            id: "slot-shared".into(),
            name: "Shared".into(),
            emoji: "🎯".into(),
            hotkey: Some("F7".into()),
            target_exe: None,
            crosshair: SlotModule::default(),
            display: SlotModule { on: true, preset: Some("custom-shared".into()) },
            audio: SlotModule { on: true, preset: Some("custom-shared".into()) },
        }];
        let backup = source.0.join("shared.zip");
        let exported = create_backup(&backup, &config, &source.0).unwrap();
        assert_eq!(exported.display_presets, 1);
        assert_eq!(exported.audio_presets, 1);
        // Export must have written a real, readable archive, not just a summary.
        assert!(backup.exists() && fs::metadata(&backup).unwrap().len() > 0);
        let mut archive = ZipArchive::new(File::open(&backup).unwrap()).unwrap();
        for required in ["manifest.json", "user.json"] {
            assert!(archive.by_name(required).is_ok(), "archive is missing {required}");
        }

        let mut restored = Config::default();
        let store = source.0.join("shared.json");
        restored.save_to(&store).unwrap();
        let report = restore_backup(&backup, &mut restored, &store, &source.0).unwrap();

        assert!(report.warnings.is_empty(), "unexpected warnings: {:?}", report.warnings);
        assert_eq!(restored.custom.display.len(), 1);
        assert_eq!(restored.custom.audio.len(), 1);
        // Preset IDs only conflict within a module, so both keep the shared ID.
        assert_eq!(restored.custom.display[0].id, "custom-shared");
        assert_eq!(restored.custom.audio[0].id, "custom-shared");
        let slot = restored.slots.list.iter().find(|slot| slot.id == "slot-shared").expect("shared slot restored");
        assert!(slot.display.on && slot.audio.on, "both module toggles should survive the round-trip");
        assert_eq!(slot.display.preset.as_deref(), Some("custom-shared"));
        assert_eq!(slot.audio.preset.as_deref(), Some("custom-shared"));
        assert_eq!(restored.find_display_preset("custom-shared").unwrap().values.brightness, 12.0);
        assert_eq!(restored.find_audio_preset("custom-shared").unwrap().bands[0], 1.5);
        // The lists the UI receives must expose the shared preset in both modules.
        assert!(restored.all_display_presets().iter().any(|preset| preset.id == "custom-shared"));
        assert!(restored.all_audio_presets().iter().any(|preset| preset.id == "custom-shared"));

        // Restored state must also survive a reload from disk, not just memory.
        let reloaded = Config::load_from(&store);
        assert_eq!(reloaded.custom.display[0].id, "custom-shared");
        assert_eq!(reloaded.custom.audio[0].id, "custom-shared");
        let reloaded_slot = reloaded.slots.list.iter().find(|slot| slot.id == "slot-shared").expect("shared slot persisted");
        assert_eq!(reloaded_slot.display.preset.as_deref(), Some("custom-shared"));
        assert_eq!(reloaded_slot.audio.preset.as_deref(), Some("custom-shared"));
    }

    #[test]
    fn renamed_preset_references_follow_the_new_id_in_slots() {
        use crate::config::{CrosshairConfig, SlotModule};

        let source = TempDir::new("remap-preset");
        let mut config = Config::default();
        config.custom.crosshair.push(CrosshairPreset {
            // Collides with a built-in, so restore has to rename it.
            id: "style_plus".into(),
            name: "My plus".into(),
            builtin: false,
            config: CrosshairConfig::default(),
        });
        config.slots.list = vec![Slot {
            id: "slot-plus".into(),
            name: "Plus".into(),
            emoji: "🎯".into(),
            hotkey: Some("F7".into()),
            target_exe: None,
            crosshair: SlotModule { on: true, preset: Some("style_plus".into()) },
            display: SlotModule::default(),
            audio: SlotModule::default(),
        }];
        let backup = source.0.join("remap.zip");
        create_backup(&backup, &config, &source.0).unwrap();

        let mut restored = Config::default();
        let store = source.0.join("remap.json");
        restored.save_to(&store).unwrap();
        let report = restore_backup(&backup, &mut restored, &store, &source.0).unwrap();

        assert_eq!(restored.custom.crosshair.len(), 1);
        let restored_id = restored.custom.crosshair[0].id.clone();
        assert_ne!(restored_id, "style_plus", "a built-in collision should be renamed");
        assert_eq!(restored.custom.crosshair[0].name, "My plus");
        let slot = restored.slots.list.iter().find(|slot| slot.id == "slot-plus").expect("slot restored");
        assert_eq!(slot.crosshair.preset.as_deref(), Some(restored_id.as_str()));
        assert!(!report.warnings.iter().any(|warning| warning.contains("is missing")));
    }

    #[test]
    fn legacy_crosshair_id_is_renamed_so_the_slot_keeps_its_preset() {
        use crate::config::{CrosshairConfig, SlotModule};

        let source = TempDir::new("legacy-preset");
        let mut config = Config::default();
        config.custom.crosshair.push(CrosshairPreset {
            // `Config::normalize` rewrites slot references from this retired ID
            // to the built-in `style_plus`, so the restore must not keep it.
            id: "style_casual".into(),
            name: "Old casual".into(),
            builtin: false,
            config: CrosshairConfig::default(),
        });
        config.slots.list = vec![Slot {
            id: "slot-legacy".into(),
            name: "Legacy".into(),
            emoji: "🎯".into(),
            hotkey: None,
            target_exe: None,
            crosshair: SlotModule { on: true, preset: Some("style_casual".into()) },
            display: SlotModule::default(),
            audio: SlotModule::default(),
        }];
        let backup = source.0.join("legacy.zip");
        create_backup(&backup, &config, &source.0).unwrap();

        let mut restored = Config::default();
        let store = source.0.join("legacy.json");
        restored.save_to(&store).unwrap();
        let report = restore_backup(&backup, &mut restored, &store, &source.0).unwrap();

        assert_eq!(restored.custom.crosshair.len(), 1);
        let restored_id = restored.custom.crosshair[0].id.clone();
        assert_ne!(restored_id, "style_casual");
        let slot = restored.slots.list.iter().find(|slot| slot.id == "slot-legacy").expect("slot restored");
        assert_eq!(slot.crosshair.preset.as_deref(), Some(restored_id.as_str()));
        assert!(!report.warnings.iter().any(|warning| warning.contains("is missing")));
        // Normalization runs during restore and again on load; neither may move
        // the slot onto the built-in.
        let reloaded = Config::load_from(&store);
        assert_eq!(reloaded.slots.list[0].crosshair.preset.as_deref(), Some(restored_id.as_str()));
    }

    #[test]
    fn preset_limit_is_reported_instead_of_only_looking_like_a_missing_preset() {
        use crate::config::{DisplayPreset, DisplayValues, SlotModule};

        let source = TempDir::new("preset-limit");
        let mut config = Config::default();
        for index in 0..=MAX_RESTORED_PRESETS {
            config.custom.display.push(DisplayPreset {
                id: format!("custom-display-{index}"),
                name: format!("Screen {index}"),
                builtin: false,
                values: DisplayValues::default(),
            });
        }
        let dropped = format!("custom-display-{MAX_RESTORED_PRESETS}");
        config.slots.list = vec![Slot {
            id: "slot-limit".into(),
            name: "Limit".into(),
            emoji: "🎯".into(),
            hotkey: None,
            target_exe: None,
            crosshair: SlotModule::default(),
            display: SlotModule { on: true, preset: Some(dropped.clone()) },
            audio: SlotModule::default(),
        }];
        let backup = source.0.join("limit.zip");
        create_backup(&backup, &config, &source.0).unwrap();

        let mut restored = Config::default();
        let store = source.0.join("limit.json");
        restored.save_to(&store).unwrap();
        let report = restore_backup(&backup, &mut restored, &store, &source.0).unwrap();

        assert_eq!(restored.custom.display.len(), MAX_RESTORED_PRESETS);
        assert!(
            report.warnings.iter().any(|warning| warning.contains("Only the first 100 display presets were restored.")),
            "the preset limit must be explained: {:?}",
            report.warnings
        );
        assert!(report.warnings.iter().any(|warning| warning.contains(&format!("'{dropped}' is missing"))));
        let slot = restored.slots.list.iter().find(|slot| slot.id == "slot-limit").unwrap();
        assert_eq!(slot.display.preset, None);
    }

    #[test]
    fn a_renamed_preset_cannot_take_the_id_of_a_preset_dropped_by_the_cap() {
        use crate::config::{DisplayPreset, DisplayValues, SlotModule};

        let source = TempDir::new("cap-collision");
        let mut config = Config::default();
        // Index 7 has no usable ID, so restore has to rename it. Without the
        // seeded generator it would receive "restored-display-7" — the ID of
        // the preset at the cap boundary that is dropped on restore.
        for index in 0..=MAX_RESTORED_PRESETS {
            let id = match index {
                7 => String::new(),
                MAX_RESTORED_PRESETS => "restored-display-7".to_string(),
                _ => format!("custom-display-{index}"),
            };
            config.custom.display.push(DisplayPreset {
                id,
                name: format!("Screen {index}"),
                builtin: false,
                values: DisplayValues::default(),
            });
        }
        config.slots.list = vec![Slot {
            id: "slot-cap".into(),
            name: "Cap".into(),
            emoji: "🎯".into(),
            hotkey: None,
            target_exe: None,
            crosshair: SlotModule::default(),
            display: SlotModule { on: true, preset: Some("restored-display-7".into()) },
            audio: SlotModule::default(),
        }];
        let backup = source.0.join("cap.zip");
        create_backup(&backup, &config, &source.0).unwrap();

        let mut restored = Config::default();
        let store = source.0.join("cap.json");
        restored.save_to(&store).unwrap();
        let report = restore_backup(&backup, &mut restored, &store, &source.0).unwrap();

        let slot = restored.slots.list.iter().find(|slot| slot.id == "slot-cap").expect("slot restored");
        assert_eq!(slot.display.preset, None, "the dropped preset must not be substituted");
        assert!(
            report.warnings.iter().any(|warning| warning.contains("'restored-display-7' is missing")),
            "the dropped reference must be reported: {:?}",
            report.warnings
        );
        // The replacement ID must avoid the ID the archive already used, even
        // though that preset is dropped by the cap.
        assert!(
            restored.custom.display.iter().all(|preset| preset.id != "restored-display-7"),
            "a generated ID must not take another archived preset's ID"
        );
    }

    #[test]
    fn a_dangling_slot_reference_never_attaches_to_a_renamed_preset() {
        use crate::config::{DisplayPreset, DisplayValues, SlotModule};

        let source = TempDir::new("dangling-ref");
        let mut config = Config::default();
        config.custom.display.push(DisplayPreset {
            // Renamed on restore; it ends up owning the ID the slot names.
            id: String::new(),
            name: "Nameless".into(),
            builtin: false,
            values: DisplayValues { brightness: 9.0, ..Default::default() },
        });
        config.slots.list = vec![Slot {
            id: "slot-dangling".into(),
            name: "Dangling".into(),
            emoji: "🎯".into(),
            hotkey: None,
            target_exe: None,
            crosshair: SlotModule::default(),
            display: SlotModule { on: true, preset: Some("restored-display-0".into()) },
            audio: SlotModule::default(),
        }];
        let backup = source.0.join("dangling.zip");
        create_backup(&backup, &config, &source.0).unwrap();

        let mut restored = Config::default();
        let store = source.0.join("dangling.json");
        restored.save_to(&store).unwrap();
        let report = restore_backup(&backup, &mut restored, &store, &source.0).unwrap();

        // The renamed preset owns that ID, but the slot never referenced it.
        assert!(restored.custom.display.iter().any(|preset| preset.id == "restored-display-0"));
        let slot = restored.slots.list.iter().find(|slot| slot.id == "slot-dangling").expect("slot restored");
        assert_eq!(slot.display.preset, None, "a dangling reference must be cleared");
        assert!(
            report.warnings.iter().any(|warning| warning.contains("'restored-display-0' is missing")),
            "the dangling reference must be reported: {:?}",
            report.warnings
        );
    }

    #[test]
    fn corrupt_backup_leaves_config_untouched() {
        let dir = TempDir::new("corrupt");
        let backup_path = dir.0.join("bad.zip");
        fs::write(&backup_path, b"not a zip").unwrap();
        let mut config = Config::default();
        config.crosshair.size = 19.0;
        let before = config.clone();
        let store = dir.0.join("config.json");
        config.save_to(&store).unwrap();
        let before_file = fs::read(&store).unwrap();
        assert!(restore_backup(&backup_path, &mut config, &store, &dir.0).is_err());
        assert_eq!(config.crosshair.size, before.crosshair.size);
        assert_eq!(fs::read(&store).unwrap(), before_file);
        assert!(!dir.0.join("crosshairs").exists());
    }

    #[test]
    fn traversal_entries_are_rejected() {
        let dir = TempDir::new("traversal");
        let backup_path = dir.0.join("bad.zip");
        let file = File::create(&backup_path).unwrap();
        let mut archive = ZipWriter::new(file);
        let options = SimpleFileOptions::default();
        let manifest = BackupManifest {
            format: BACKUP_FORMAT,
            app_version: "1".into(),
            created_unix: 0,
            images: Vec::new(),
        };
        archive.start_file("manifest.json", options).unwrap();
        archive.write_all(&serde_json::to_vec(&manifest).unwrap()).unwrap();
        archive.start_file("user.json", options).unwrap();
        archive.write_all(b"{}").unwrap();
        archive.start_file("../escape.txt", options).unwrap();
        archive.write_all(b"nope").unwrap();
        archive.finish().unwrap();
        assert!(inspect_backup(&backup_path).is_err());
    }

    #[test]
    fn missing_image_and_hotkey_conflict_are_handled() {
        let dir = TempDir::new("missing");
        let backup_path = dir.0.join("bad.zip");
        let file = File::create(&backup_path).unwrap();
        let mut archive = ZipWriter::new(file);
        let options = SimpleFileOptions::default();
        let manifest = BackupManifest {
            format: BACKUP_FORMAT,
            app_version: "1".into(),
            created_unix: 1,
            images: vec![BackupImage { original: "old.png".into(), archive: "crosshairs/missing.png".into() }],
        };
        archive.start_file("manifest.json", options).unwrap();
        archive.write_all(&serde_json::to_vec(&manifest).unwrap()).unwrap();
        archive.start_file("user.json", options).unwrap();
        archive.write_all(b"{}").unwrap();
        archive.finish().unwrap();
        assert!(inspect_backup(&backup_path).is_err());

        let source = TempDir::new("hotkey-source");
        let mut config = sample_config(&source.0);
        config.settings.aim_hotkey = "F9".into();
        let backup = source.0.join("backup.zip");
        create_backup(&backup, &config, &source.0).unwrap();
        let mut restored = Config::default();
        restored.settings.aim_hotkey = "F9".into();
        let store = source.0.join("restored.json");
        restored.save_to(&store).unwrap();
        let report = restore_backup(&backup, &mut restored, &store, &source.0).unwrap();
        assert!(report.warnings.iter().any(|warning| warning.contains("Hotkey F9")));
    }

    #[test]
    fn restored_eq_profile_is_clamped_and_capped_before_it_reaches_the_curve() {
        let dir = TempDir::new("eq-profile");
        let mut config = Config::default();
        let mut bands: Vec<ImportedBand> = (0..40)
            .map(|index| ImportedBand { freq: 20.0 + index as f32 * 100.0, gain: 3.0 })
            .collect();
        bands.push(ImportedBand { freq: 5.0, gain: 400.0 });
        bands.push(ImportedBand { freq: 30000.0, gain: -300.0 });
        config.audio.imported = Some(ImportedProfile {
            name: "Too loud".into(),
            raw: "GraphicEQ: 5 400; 30000 -300".into(),
            bands,
            parametric: false,
            preamp: 40.0,
        });
        let backup = dir.0.join("eq.zip");
        create_backup(&backup, &config, &dir.0).unwrap();

        let mut restored = Config::default();
        let store = dir.0.join("config.json");
        restored.save_to(&store).unwrap();
        let report = restore_backup(&backup, &mut restored, &store, &dir.0).unwrap();
        let profile = restored.audio.imported.as_ref().expect("profile restored");
        assert!((profile.preamp - PREAMP_RANGE.1).abs() < 0.001, "preamp {} was not clamped", profile.preamp);
        assert_eq!(profile.bands.len(), MAX_IMPORT_BANDS);
        assert!(
            profile
                .bands
                .iter()
                .all(|band| (BAND_FREQ_RANGE.0..=BAND_FREQ_RANGE.1).contains(&band.freq)),
            "out-of-range frequencies survived: {:?}",
            profile.bands
        );
        assert!(
            profile
                .bands
                .iter()
                .all(|band| (BAND_GAIN_RANGE.0..=BAND_GAIN_RANGE.1).contains(&band.gain)),
            "out-of-range gains survived: {:?}",
            profile.bands
        );
        assert!(
            report.warnings.iter().any(|warning| warning.contains("'Too loud' were clamped")),
            "clamping must be reported: {:?}",
            report.warnings
        );
        assert!(
            report.warnings.iter().any(|warning| warning.contains("first 32 bands")),
            "the band cap must be reported: {:?}",
            report.warnings
        );

        // The clamped profile must be what is persisted, not only what is live.
        let reloaded = Config::load_from(&store);
        let stored = reloaded.audio.imported.as_ref().expect("profile persisted");
        assert!((stored.preamp - PREAMP_RANGE.1).abs() < 0.001);
        assert_eq!(stored.bands.len(), MAX_IMPORT_BANDS);
    }

    fn image_count(dir: &Path) -> usize {
        fs::read_dir(dir)
            .map(|entries| entries.flatten().filter(|entry| entry.path().is_file()).count())
            .unwrap_or(0)
    }

    fn archived_image_count(path: &Path) -> usize {
        let mut archive = ZipArchive::new(File::open(path).unwrap()).unwrap();
        let mut count = 0;
        for index in 0..archive.len() {
            if archive
                .by_index(index)
                .map(|entry| entry.name().starts_with("crosshairs/"))
                .unwrap_or(false)
            {
                count += 1;
            }
        }
        count
    }

    #[test]
    fn restoring_the_same_backup_reuses_images_and_later_backups_do_not_grow() {
        let source = TempDir::new("image-reuse-source");
        let target = TempDir::new("image-reuse-target");
        let images = source.0.join("crosshairs");
        fs::create_dir_all(&images).unwrap();
        let original = images.join("cool.png");
        fs::write(&original, b"same-picture-bytes").unwrap();
        let mut config = Config::default();
        config.crosshair.image = Some(original.to_string_lossy().to_string());
        config.crosshair.image_name = Some("cool.png".into());
        let backup = source.0.join("cool.zip");
        create_backup(&backup, &config, &source.0).unwrap();

        let mut restored = Config::default();
        let store = target.0.join("config.json");
        restored.save_to(&store).unwrap();
        restore_backup(&backup, &mut restored, &store, &target.0).unwrap();
        let first = restored.crosshair.image.clone().expect("image restored");
        let target_images = target.0.join("crosshairs");
        assert_eq!(image_count(&target_images), 1);

        // Restoring the same archive again must reuse that file, not copy it.
        let report = restore_backup(&backup, &mut restored, &store, &target.0).unwrap();
        assert_eq!(restored.crosshair.image.as_deref(), Some(first.as_str()));
        assert_eq!(image_count(&target_images), 1, "a second restore must not add a copy");
        assert!(report.warnings.is_empty(), "{:?}", report.warnings);

        // Stale copies beside the picture must collapse to one archived image,
        // and a reference to a skipped copy must still resolve.
        fs::write(target_images.join("cool.png"), b"same-picture-bytes").unwrap();
        fs::write(target_images.join("restore-0001-0000-cool.png"), b"same-picture-bytes").unwrap();
        let second = target.0.join("second.zip");
        let preview = create_backup(&second, &restored, &target.0).unwrap();
        assert_eq!(preview.images, 1, "identical copies must collapse to one image");
        assert_eq!(archived_image_count(&second), 1, "the archive must not store the copies");

        let fresh = TempDir::new("image-reuse-fresh");
        let mut fresh_config = Config::default();
        let fresh_store = fresh.0.join("config.json");
        fresh_config.save_to(&fresh_store).unwrap();
        let report = restore_backup(&second, &mut fresh_config, &fresh_store, &fresh.0).unwrap();
        let restored_image = fresh_config.crosshair.image.clone().expect("image still resolved");
        assert_eq!(fs::read(&restored_image).unwrap(), b"same-picture-bytes");
        assert!(report.warnings.is_empty(), "{:?}", report.warnings);
    }

    #[test]
    fn presets_sharing_one_image_store_it_once_and_restore_the_same_bytes() {
        let source = TempDir::new("shared-image");
        let target = TempDir::new("shared-image-target");
        let images = source.0.join("crosshairs");
        fs::create_dir_all(&images).unwrap();
        let shared = images.join("shared.png");
        fs::write(&shared, b"shared-picture-bytes").unwrap();
        let shared_path = shared.to_string_lossy().to_string();
        let mut config = Config::default();
        config.crosshair.image = Some(shared_path.clone());
        config.crosshair.image_name = Some("shared.png".into());
        for index in 0..2 {
            config.custom.crosshair.push(CrosshairPreset {
                id: format!("custom-shared-{index}"),
                name: format!("Shared {index}"),
                builtin: false,
                config: crate::config::CrosshairConfig {
                    image: Some(shared_path.clone()),
                    image_name: Some("shared.png".into()),
                    ..Default::default()
                },
            });
        }
        let backup = source.0.join("shared.zip");
        let preview = create_backup(&backup, &config, &source.0).unwrap();
        assert!(preview.warnings.is_empty(), "{:?}", preview.warnings);
        assert_eq!(preview.images, 1, "one picture is one entry");
        assert_eq!(archived_image_count(&backup), 1, "the archive stores it once");

        let mut restored = Config::default();
        let store = target.0.join("config.json");
        restored.save_to(&store).unwrap();
        let report = restore_backup(&backup, &mut restored, &store, &target.0).unwrap();
        assert!(report.warnings.is_empty(), "{:?}", report.warnings);
        assert_eq!(restored.custom.crosshair.len(), 2);
        let active = restored.crosshair.image.clone().expect("active image restored");
        assert_eq!(fs::read(&active).unwrap(), b"shared-picture-bytes");
        for preset in &restored.custom.crosshair {
            let image = preset.config.image.as_ref().expect("preset image restored");
            assert_eq!(fs::read(image).unwrap(), b"shared-picture-bytes");
        }
        assert_eq!(image_count(&target.0.join("crosshairs")), 1, "shared bytes are restored once");
    }

    #[test]
    fn different_pictures_with_the_same_extension_are_never_merged() {
        let source = TempDir::new("distinct-images");
        let target = TempDir::new("distinct-images-target");
        let images = source.0.join("crosshairs");
        fs::create_dir_all(&images).unwrap();
        let first = images.join("first.png");
        let second = images.join("second.png");
        // Same length, same extension, different pictures.
        fs::write(&first, b"picture-one").unwrap();
        fs::write(&second, b"picture-two").unwrap();
        assert!(!file_matches(&second, b"picture-one"), "a same-length file is not the same picture");

        let mut config = Config::default();
        for (index, path) in [&first, &second].into_iter().enumerate() {
            config.custom.crosshair.push(CrosshairPreset {
                id: format!("custom-distinct-{index}"),
                name: format!("Distinct {index}"),
                builtin: false,
                config: crate::config::CrosshairConfig {
                    image: Some(path.to_string_lossy().to_string()),
                    ..Default::default()
                },
            });
        }
        let backup = source.0.join("distinct.zip");
        let preview = create_backup(&backup, &config, &source.0).unwrap();
        assert_eq!(preview.images, 2, "different pictures keep their own entry");
        assert_eq!(archived_image_count(&backup), 2);

        let mut restored = Config::default();
        let store = target.0.join("config.json");
        restored.save_to(&store).unwrap();
        let report = restore_backup(&backup, &mut restored, &store, &target.0).unwrap();
        assert!(report.warnings.is_empty(), "{:?}", report.warnings);
        assert_eq!(fs::read(restored.custom.crosshair[0].config.image.as_ref().unwrap()).unwrap(), b"picture-one");
        assert_eq!(fs::read(restored.custom.crosshair[1].config.image.as_ref().unwrap()).unwrap(), b"picture-two");
    }

    #[test]
    fn identical_bytes_under_different_extensions_keep_their_own_entry() {
        let source = TempDir::new("extensions");
        let target = TempDir::new("extensions-target");
        let images = source.0.join("crosshairs");
        fs::create_dir_all(&images).unwrap();
        // The same bytes under two extensions: the renderer decodes from the
        // path, so neither may be renamed to the other's format.
        let png = images.join("logo.png");
        let svg = images.join("logo.svg");
        fs::write(&png, b"<svg/>").unwrap();
        fs::write(&svg, b"<svg/>").unwrap();
        let mut config = Config::default();
        for (index, path) in [&png, &svg].into_iter().enumerate() {
            config.custom.crosshair.push(CrosshairPreset {
                id: format!("custom-ext-{index}"),
                name: format!("Ext {index}"),
                builtin: false,
                config: crate::config::CrosshairConfig {
                    image: Some(path.to_string_lossy().to_string()),
                    ..Default::default()
                },
            });
        }
        let backup = source.0.join("extensions.zip");
        let preview = create_backup(&backup, &config, &source.0).unwrap();
        assert_eq!(preview.images, 2, "different extensions keep their own entry");
        assert_eq!(archived_image_count(&backup), 2);

        let mut restored = Config::default();
        let store = target.0.join("config.json");
        restored.save_to(&store).unwrap();
        let report = restore_backup(&backup, &mut restored, &store, &target.0).unwrap();
        assert!(report.warnings.is_empty(), "{:?}", report.warnings);
        for (preset, extension) in restored.custom.crosshair.iter().zip(["png", "svg"]) {
            let image = preset.config.image.as_ref().expect("preset image restored");
            assert_eq!(Path::new(image).extension().and_then(|value| value.to_str()), Some(extension));
            assert_eq!(fs::read(image).unwrap(), b"<svg/>");
        }
    }

    #[test]
    fn a_reference_to_an_unstored_image_is_dropped_once_and_reported() {
        let source = TempDir::new("missing-image");
        let target = TempDir::new("missing-image-target");
        fs::create_dir_all(source.0.join("crosshairs")).unwrap();
        let missing = source.0.join("crosshairs").join("gone.png").to_string_lossy().to_string();
        let mut config = Config::default();
        config.crosshair.image = Some(missing.clone());
        config.crosshair.image_name = Some("gone.png".into());
        config.custom.crosshair.push(CrosshairPreset {
            id: "custom-gone".into(),
            name: "Gone".into(),
            builtin: false,
            config: crate::config::CrosshairConfig {
                image: Some(missing.clone()),
                image_name: Some("gone.png".into()),
                ..Default::default()
            },
        });
        let backup = source.0.join("missing.zip");
        let preview = create_backup(&backup, &config, &source.0).unwrap();
        assert_eq!(preview.images, 0);
        assert_eq!(archived_image_count(&backup), 0);
        assert!(
            preview.warnings.iter().any(|warning| warning.contains("2 crosshair image(s) are missing")),
            "dropping references must be reported: {:?}",
            preview.warnings
        );

        // The archive carries no path a restore could fail to resolve.
        let mut archive = ZipArchive::new(File::open(&backup).unwrap()).unwrap();
        let user: BackupUser = serde_json::from_slice(&read_entry(&mut archive, "user.json", MAX_BACKUP_JSON).unwrap()).unwrap();
        assert!(user.crosshair_image.is_none());
        assert!(user.crosshair_image_name.is_none());
        assert!(user.custom.crosshair[0].config.image.is_none());

        let mut restored = Config::default();
        let store = target.0.join("config.json");
        restored.save_to(&store).unwrap();
        let report = restore_backup(&backup, &mut restored, &store, &target.0).unwrap();
        assert!(restored.crosshair.image.is_none());
        assert!(restored.crosshair.image_name.is_none());
        assert!(restored.custom.crosshair[0].config.image.is_none());
        assert!(report.warnings.is_empty(), "nothing dangles: {:?}", report.warnings);
    }

    #[test]
    fn a_reference_to_an_absent_entry_is_warned_about_instead_of_dangling() {
        let dir = TempDir::new("dangling-ref");
        let path = dir.0.join("crafted.zip");
        let manifest = BackupManifest {
            format: BACKUP_FORMAT,
            app_version: "test".into(),
            created_unix: 0,
            images: Vec::new(),
        };
        let mut user = BackupUser::default();
        user.crosshair_image = Some("crosshairs/nope.png".into());
        user.crosshair_image_name = Some("nope.png".into());
        user.custom.crosshair.push(CrosshairPreset {
            id: "custom-nope".into(),
            name: "Nope".into(),
            builtin: false,
            config: crate::config::CrosshairConfig {
                image: Some("crosshairs/nope.png".into()),
                ..Default::default()
            },
        });
        let mut archive = ZipWriter::new(File::create(&path).unwrap());
        let options = SimpleFileOptions::default();
        archive.start_file("manifest.json", options).unwrap();
        archive.write_all(&serde_json::to_vec(&manifest).unwrap()).unwrap();
        archive.start_file("user.json", options).unwrap();
        archive.write_all(&serde_json::to_vec(&user).unwrap()).unwrap();
        archive.finish().unwrap();

        let mut restored = Config::default();
        let store = dir.0.join("config.json");
        restored.save_to(&store).unwrap();
        let report = restore_backup(&path, &mut restored, &store, &dir.0).unwrap();
        assert!(restored.crosshair.image.is_none());
        assert!(restored.crosshair.image_name.is_none());
        assert_eq!(restored.custom.crosshair.len(), 1);
        assert!(restored.custom.crosshair[0].config.image.is_none());
        assert_eq!(
            report.warnings.iter().filter(|warning| warning.contains("is not in the backup")).count(),
            2,
            "each dangling reference must be reported: {:?}",
            report.warnings
        );
        let reloaded = Config::load_from(&store);
        assert!(reloaded.crosshair.image.is_none());
        assert!(reloaded.custom.crosshair[0].config.image.is_none());
    }

    #[test]
    fn a_failed_save_leaves_no_archive_and_no_temporary_file() {
        let dir = TempDir::new("failed-save");
        let images = dir.0.join("crosshairs");
        fs::create_dir_all(&images).unwrap();
        let image = images.join("a.png");
        fs::write(&image, b"picture").unwrap();
        let mut config = Config::default();
        config.crosshair.image = Some(image.to_string_lossy().to_string());
        // A folder where the archive should go fails the final replace.
        let target = dir.0.join("backup.zip");
        fs::create_dir_all(&target).unwrap();
        assert!(create_backup(&target, &config, &dir.0).is_err());
        assert!(target.is_dir(), "the failed target is left as it was");
        let leftovers: Vec<String> = fs::read_dir(&dir.0)
            .unwrap()
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().to_string())
            .filter(|name| name.contains("tmp"))
            .collect();
        assert!(leftovers.is_empty(), "no temporary archive may survive: {leftovers:?}");
    }

    #[test]
    fn undoing_a_restore_reports_a_profile_it_had_to_clean() {
        let dir = TempDir::new("undo-cleanup");
        let store = dir.0.join("config.json");
        fs::write(&store, b"{}").unwrap();
        // The undo copy is a hand-editable file too: this one holds a preamp that
        // cannot be a number, so undo must drop the profile and say so.
        fs::write(
            pre_restore_path(&dir.0),
            r#"{"audio":{"imported":{"name":"Hand edited","raw":"","parametric":false,"preamp":1e39,"bands":[{"freq":31.0,"gain":2.0}]}}}"#,
        )
        .unwrap();

        let (reverted, warnings) = undo_last_restore(&store, &dir.0).expect("the restore should be undoable");
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("preamp is not a valid number"), "{warnings:?}");
        assert!(reverted.audio.imported.is_none(), "the corrupt profile must not come back");
        let saved = Config::load_from(&store);
        assert!(saved.audio.imported.is_none(), "the cleanup must be what got saved");
        assert!(!restore_undo_available(&dir.0));
    }

    #[test]
    fn undo_last_restore_puts_the_pre_restore_config_back_and_is_one_shot() {
        let dir = TempDir::new("undo");
        let mut config = Config::default();
        config.custom.crosshair.push(CrosshairPreset {
            id: "custom-before".into(),
            name: "Before restore".into(),
            builtin: false,
            config: crate::config::CrosshairConfig::default(),
        });
        config.audio.imported = Some(ImportedProfile {
            name: "Before EQ".into(),
            raw: "GraphicEQ: 31 2".into(),
            bands: vec![ImportedBand { freq: 31.0, gain: 2.0 }],
            parametric: false,
            preamp: -1.0,
        });
        let store = dir.0.join("config.json");
        config.save_to(&store).unwrap();
        let before = Config::load_from(&store);
        assert!(!restore_undo_available(&dir.0), "nothing to undo before a restore");

        let backup = dir.0.join("empty.zip");
        create_backup(&backup, &Config::default(), &dir.0).unwrap();
        let mut live = before.clone();
        restore_backup(&backup, &mut live, &store, &dir.0).unwrap();
        assert!(restore_undo_available(&dir.0), "a restore must leave the undo copy behind");
        assert!(live.custom.crosshair.is_empty(), "the restore replaced the presets");
        assert!(live.audio.imported.is_none());

        let (reverted, warnings) = undo_last_restore(&store, &dir.0).expect("the restore should be undoable");
        assert!(warnings.is_empty(), "a clean undo copy reports nothing: {warnings:?}");
        assert_eq!(reverted.custom.crosshair.len(), before.custom.crosshair.len());
        assert_eq!(reverted.custom.crosshair[0].id, "custom-before");
        assert_eq!(reverted.audio.imported.as_ref().unwrap().name, "Before EQ");

        // The undo is what is saved, and it cannot be replayed.
        let reloaded = Config::load_from(&store);
        assert_eq!(reloaded.custom.crosshair.len(), before.custom.crosshair.len());
        assert_eq!(reloaded.audio.imported.as_ref().unwrap().name, "Before EQ");
        assert!(!restore_undo_available(&dir.0), "the undo copy must be consumed");
        assert!(undo_last_restore(&store, &dir.0).is_err());
    }

    #[test]
    fn restored_eq_profile_with_non_finite_values_is_rejected() {
        let base = ImportedProfile {
            name: "Broken".into(),
            raw: String::new(),
            bands: vec![ImportedBand { freq: 31.0, gain: 2.0 }],
            parametric: false,
            preamp: -1.0,
        };

        let mut warnings = Vec::new();
        let rejected = sanitize_imported(Some(ImportedProfile { preamp: f32::NAN, ..base.clone() }), &mut warnings);
        assert!(rejected.is_none(), "a NaN preamp must not be applied");
        assert!(
            warnings.iter().any(|warning| warning.contains("preamp is not a valid number")),
            "the rejection must be reported: {warnings:?}"
        );

        let mut warnings = Vec::new();
        let corrupt = ImportedProfile {
            bands: vec![
                ImportedBand { freq: f32::NAN, gain: 2.0 },
                ImportedBand { freq: 1000.0, gain: f32::INFINITY },
            ],
            ..base.clone()
        };
        assert!(sanitize_imported(Some(corrupt), &mut warnings).is_none());
        assert!(
            warnings.iter().any(|warning| warning.contains("none of its bands are valid numbers")),
            "the rejection must be reported: {warnings:?}"
        );

        // One unusable band must not throw away the rest of the curve.
        let mut warnings = Vec::new();
        let partly_corrupt = ImportedProfile {
            name: "L".repeat(MAX_IMPORT_NAME + 20),
            raw: "x".repeat(MAX_IMPORT_RAW + 500),
            bands: vec![
                ImportedBand { freq: 31.0, gain: 2.0 },
                ImportedBand { freq: 62.0, gain: f32::NAN },
            ],
            ..base
        };
        let kept = sanitize_imported(Some(partly_corrupt), &mut warnings).expect("a usable band keeps the profile");
        assert_eq!(kept.bands, vec![ImportedBand { freq: 31.0, gain: 2.0 }]);
        assert_eq!(kept.name.chars().count(), MAX_IMPORT_NAME, "a restored name must be capped like an imported one");
        assert_eq!(kept.raw.chars().count(), MAX_IMPORT_RAW, "restored raw text must be capped like an imported one");
        assert!(
            warnings.iter().any(|warning| warning.contains("Dropped 1 band(s)")),
            "the dropped band must be reported: {warnings:?}"
        );
    }

    /// Copies a backup while replacing `user.json`, so a test can archive values
    /// no importer would ever produce.
    fn rewrite_user_json(source: &Path, target: &Path, edit: impl Fn(&mut serde_json::Value)) {
        let mut archive = ZipArchive::new(File::open(source).unwrap()).unwrap();
        let mut out = ZipWriter::new(File::create(target).unwrap());
        let options = SimpleFileOptions::default();
        for index in 0..archive.len() {
            let mut entry = archive.by_index(index).unwrap();
            let name = entry.name().to_string();
            let mut bytes = Vec::new();
            entry.read_to_end(&mut bytes).unwrap();
            drop(entry);
            if name == "user.json" {
                let mut value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
                edit(&mut value);
                bytes = serde_json::to_vec(&value).unwrap();
            }
            out.start_file(name, options).unwrap();
            out.write_all(&bytes).unwrap();
        }
        out.finish().unwrap();
    }

    /// A backup carrying a sound profile whose numbers the importer accepts.
    fn archive_with_a_valid_profile(name: &str) -> (TempDir, PathBuf) {
        let source = TempDir::new(name);
        let mut config = Config::default();
        config.audio.imported = Some(ImportedProfile {
            name: "Desk EQ".into(),
            raw: "GraphicEQ: 31 2; 62 -1".into(),
            bands: vec![
                ImportedBand { freq: 31.0, gain: 2.0 },
                ImportedBand { freq: 62.0, gain: -1.0 },
            ],
            parametric: false,
            preamp: -1.0,
        });
        let backup = source.0.join("clean.zip");
        create_backup(&backup, &config, &source.0).unwrap();
        (source, backup)
    }

    #[test]
    fn a_restored_profile_with_an_infinite_preamp_is_rejected() {
        let (source, clean) = archive_with_a_valid_profile("archive-inf-preamp");
        let edited = source.0.join("inf-preamp.zip");
        // `1e39` is a valid JSON number that overflows f32 to infinity.
        rewrite_user_json(&clean, &edited, |user| {
            user["audioImport"]["preamp"] = serde_json::Value::from(1e39_f64);
        });

        // The premise: the archive's own user.json now yields an infinite preamp.
        let mut archive = ZipArchive::new(File::open(&edited).unwrap()).unwrap();
        let raw: BackupUser =
            serde_json::from_slice(&read_entry(&mut archive, "user.json", MAX_BACKUP_JSON).unwrap()).unwrap();
        assert!(raw.audio_import.as_ref().unwrap().preamp.is_infinite());

        let mut restored = Config::default();
        let store = source.0.join("inf-preamp.json");
        restored.save_to(&store).unwrap();
        let report = restore_backup(&edited, &mut restored, &store, &source.0).unwrap();
        assert!(restored.audio.imported.is_none(), "an infinite preamp must not reach the curve");
        assert!(
            report.warnings.iter().any(|warning| warning.contains("preamp is not a valid number")),
            "the rejection must be reported: {:?}",
            report.warnings
        );
        assert!(crate::audio::dsp_math::effective_bands(&restored.audio).iter().all(|gain| gain.is_finite()));
        assert!(crate::audio::dsp_math::fx_bands(&restored.audio).1.iter().all(|gain| gain.is_finite()));
        // The rejected profile is not persisted either.
        assert!(Config::load_from(&store).audio.imported.is_none());
    }

    #[test]
    fn a_restored_profile_with_an_infinite_band_gain_drops_only_that_band() {
        let (source, clean) = archive_with_a_valid_profile("archive-inf-band");
        let edited = source.0.join("inf-band.zip");
        rewrite_user_json(&clean, &edited, |user| {
            user["audioImport"]["bands"][1]["gain"] = serde_json::Value::from(1e39_f64);
        });
        let mut archive = ZipArchive::new(File::open(&edited).unwrap()).unwrap();
        let raw: BackupUser =
            serde_json::from_slice(&read_entry(&mut archive, "user.json", MAX_BACKUP_JSON).unwrap()).unwrap();
        assert!(raw.audio_import.as_ref().unwrap().bands[1].gain.is_infinite());

        let mut restored = Config::default();
        let store = source.0.join("inf-band.json");
        restored.save_to(&store).unwrap();
        let report = restore_backup(&edited, &mut restored, &store, &source.0).unwrap();
        let profile = restored.audio.imported.as_ref().expect("a usable band keeps the profile");
        assert_eq!(profile.bands, vec![ImportedBand { freq: 31.0, gain: 2.0 }]);
        assert!(
            report.warnings.iter().any(|warning| warning.contains("Dropped 1 band(s)")),
            "the dropped band must be reported: {:?}",
            report.warnings
        );
        assert!(crate::audio::dsp_math::effective_bands(&restored.audio).iter().all(|gain| gain.is_finite()));
        let reloaded = Config::load_from(&store);
        assert_eq!(
            reloaded.audio.imported.expect("the profile is persisted").bands,
            vec![ImportedBand { freq: 31.0, gain: 2.0 }]
        );
    }
}
