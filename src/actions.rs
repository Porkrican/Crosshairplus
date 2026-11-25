use std::fs;
use std::path::{Path, PathBuf};

use crate::config::{Config, DisplayConfig, Settings};
use crate::ipc::Command;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToggleEffect {
    Crosshair,
    Display,
    Audio,
}

impl ToggleEffect {
    pub fn applies_display(self) -> bool {
        self == Self::Display
    }

    pub fn applies_audio(self) -> bool {
        self == Self::Audio
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Effect {
    ConfigurationSaved,
    Toggle(ToggleEffect),
    DisplayReset,
    FactoryReset { removed_config: bool, removed_images: usize },
    DataFolderReady(PathBuf),
    External(&'static str),
}

pub fn valid_preset_module(module: &str) -> bool {
    matches!(module, "crosshair" | "display" | "audio")
}

pub fn valid_tab(tab: &str) -> bool {
    matches!(tab, "aim" | "display" | "audio" | "slots")
}

pub fn validate_preset_name(module: &str, name: &str) -> Result<(), String> {
    if !valid_preset_module(module) {
        return Err(format!("unknown preset module '{module}'"));
    }
    if name.trim().is_empty() {
        return Err("preset name cannot be empty".into());
    }
    Ok(())
}

pub fn validate_custom_preset(config: &Config, module: &str, id: &str) -> Result<(), String> {
    let found = match module {
        "crosshair" => config.custom.crosshair.iter().any(|preset| preset.id == id),
        "display" => config.custom.display.iter().any(|preset| preset.id == id),
        "audio" => config.custom.audio.iter().any(|preset| preset.id == id),
        _ => return Err(format!("unknown preset module '{module}'")),
    };
    if found {
        Ok(())
    } else {
        Err(format!("unknown custom preset '{id}'"))
    }
}

fn save(config: &Config, path: &Path) -> Result<(), String> {
    config.save_to(path).map_err(|err| format!("config save failed: {err}"))
}

pub fn toggle_module_in_memory(config: &mut Config, module: &str) -> Result<ToggleEffect, String> {
    let effect = match module {
        "crosshair" => {
            config.crosshair.enabled = !config.crosshair.enabled;
            ToggleEffect::Crosshair
        }
        "display" => {
            config.display.enabled = !config.display.enabled;
            ToggleEffect::Display
        }
        "audio" => {
            config.audio.enabled = !config.audio.enabled;
            ToggleEffect::Audio
        }
        _ => return Err(format!("unknown module '{module}'")),
    };
    Ok(effect)
}

pub fn toggle_module(config: &mut Config, module: &str, store_path: &Path) -> Result<ToggleEffect, String> {
    let effect = toggle_module_in_memory(config, module)?;
    save(config, store_path)?;
    Ok(effect)
}

pub fn set_settings(config: &mut Config, settings: Settings, store_path: &Path) -> Result<(), String> {
    config.settings = settings;
    save(config, store_path)
}

/// Wipe the user's own data and persist a pristine default config: the app
/// behaves like a fresh install. The diagnostics log and the FxSound safety
/// backup are deliberately kept (the latter is what restores audio state).
pub fn factory_reset(config: &mut Config, store_path: &Path, data_dir: &Path) -> Result<(bool, usize), String> {
    let removed_config = store_path.exists();
    // Remove the preserved copies too, or the old settings stay on disk after
    // a reset. `backup_path` is the same path `Config::save_to` writes.
    for stale in [crate::config::backup_path(store_path), store_path.with_extension("tmp")] {
        remove_if_exists(&stale)?;
    }
    let images = data_dir.join("crosshairs");
    let removed_images = if images.exists() {
        fs::read_dir(&images).map(|entries| entries.flatten().count()).unwrap_or(0)
    } else {
        0
    };
    if images.exists() {
        fs::remove_dir_all(&images).map_err(|err| format!("could not remove imported images: {err}"))?;
    }
    if removed_config {
        fs::remove_file(store_path).map_err(|err| format!("could not erase config: {err}"))?;
    }
    *config = Config::default();
    save(config, store_path)?;
    Ok((removed_config, removed_images))
}

fn remove_if_exists(path: &Path) -> Result<(), String> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(format!("could not remove {}: {err}", path.display())),
    }
}

pub fn execute(command: &Command, config: &mut Config, store_path: &Path, data_dir: &Path) -> Result<Effect, String> {
    match command {
        Command::ToggleMaster { module } => Ok(Effect::Toggle(toggle_module(config, module, store_path)?)),
        Command::SettingsChanged { settings } => {
            set_settings(config, settings.clone(), store_path)?;
            Ok(Effect::ConfigurationSaved)
        }
        Command::ResetDisplay => {
            config.display = DisplayConfig::default();
            save(config, store_path)?;
            Ok(Effect::DisplayReset)
        }
        Command::FactoryReset => {
            let (removed_config, removed_images) = factory_reset(config, store_path, data_dir)?;
            Ok(Effect::FactoryReset { removed_config, removed_images })
        }
        Command::OpenDataFolder => {
            fs::create_dir_all(data_dir).map_err(|err| format!("could not create data folder: {err}"))?;
            Ok(Effect::DataFolderReady(data_dir.to_path_buf()))
        }
        Command::SetupAudio => Ok(Effect::External("audio-setup")),
        Command::RepairAudio => Ok(Effect::External("audio-repair")),
        Command::RepairManagedPreset => Ok(Effect::External("audio-preset-repair")),
        Command::UnstickAudio => Ok(Effect::External("audio-unstick")),
        Command::OpenFxSound => Ok(Effect::External("open-fxsound")),
        Command::OpenFxSoundPresets => Ok(Effect::External("open-fxsound-presets")),
        Command::OpenWindowsDisplaySettings => Ok(Effect::External("open-windows-display")),
        _ => Err("command is not a core config action".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempStore {
        dir: PathBuf,
        path: PathBuf,
        data: PathBuf,
    }

    impl TempStore {
        fn new(name: &str) -> Self {
            let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
            let dir = std::env::temp_dir().join(format!("crosshair-plus-{name}-{}-{stamp}", std::process::id()));
            let data = dir.join("data");
            let path = dir.join("config.json");
            fs::create_dir_all(&dir).unwrap();
            Config::default().save_to(&path).unwrap();
            Self { dir, path, data }
        }

        fn load(&self) -> Config {
            Config::load_from(&self.path)
        }
    }

    impl Drop for TempStore {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.dir);
        }
    }

    fn command(text: &str) -> Command {
        crate::ipc::parse(text).expect("test command must parse")
    }

    fn apply_runtime_effect(
        effect: ToggleEffect,
        config: &Config,
        overlay: &crate::overlay::OverlayHandle,
        display: &mut crate::display::DisplayEngine,
        audio: &crate::audio::AudioRuntime,
    ) {
        match effect {
            ToggleEffect::Crosshair => overlay.update(crate::overlay::Input {
                config: config.crosshair.clone(),
                rect: None,
                hotkeys: Vec::new(),
                monitor_label: "test".into(),
            }),
            ToggleEffect::Display => {
                display.apply(&[], &config.display);
            }
            ToggleEffect::Audio => audio.apply(config.audio.clone()),
        }
    }

    fn toggle_effect(effect: Effect) -> ToggleEffect {
        match effect {
            Effect::Toggle(effect) => effect,
            other => panic!("expected a toggle effect, got {other:?}"),
        }
    }

    #[test]
    fn every_settings_toggle_persists_to_disk() {
        let toggles: [fn(&mut Settings); 6] = [
            |s| s.start_with_windows = !s.start_with_windows,
            |s| s.start_minimized = !s.start_minimized,
            |s| s.minimize_to_tray = !s.minimize_to_tray,
            |s| s.restore_display_on_exit = !s.restore_display_on_exit,
            |s| s.hotkeys_enabled = !s.hotkeys_enabled,
            |s| s.auto_revert_slots = !s.auto_revert_slots,
        ];
        for (index, flip) in toggles.into_iter().enumerate() {
            let store = TempStore::new(&format!("settings-{index}"));
            let before = store.load().settings;
            let mut expected = before.clone();
            flip(&mut expected);
            let payload = format!(r#"{{"v":1,"type":"SettingsChanged","payload":{{"settings":{}}}}}"#, serde_json::to_string(&expected).unwrap());
            let cmd = command(&payload);
            assert_eq!(execute(&cmd, &mut store.load(), &store.path, &store.data).unwrap(), Effect::ConfigurationSaved);
            let reloaded = store.load();
            assert_eq!(reloaded.settings, expected, "settings toggle {index} was not persisted");
            assert_ne!(reloaded.settings, before, "settings toggle {index} did not change");
        }
    }

    #[test]
    fn every_header_toggle_persists_to_disk() {
        for (module, expected) in [
            ("crosshair", ToggleEffect::Crosshair),
            ("display", ToggleEffect::Display),
            ("audio", ToggleEffect::Audio),
        ] {
            let store = TempStore::new(&format!("header-{module}"));
            let before = store.load();
            let cmd = command(&format!(r#"{{"v":1,"type":"ToggleMaster","payload":{{"module":"{module}"}}}}"#));
            let effect = execute(&cmd, &mut store.load(), &store.path, &store.data).unwrap();
            assert_eq!(effect, Effect::Toggle(expected));
            let after = store.load();
            let (was, now) = match module {
                "crosshair" => (before.crosshair.enabled, after.crosshair.enabled),
                "display" => (before.display.enabled, after.display.enabled),
                "audio" => (before.audio.enabled, after.audio.enabled),
                _ => unreachable!(),
            };
            assert_ne!(now, was, "{module} header toggle did not persist");
        }
    }

    #[test]
    fn header_crosshair_toggle_changes_overlay_runtime_state() {
        let store = TempStore::new("header-crosshair-runtime");
        let mut config = store.load();
        config.crosshair.enabled = true;
        let overlay = crate::overlay::OverlayHandle::headless();
        let mut display = crate::display::DisplayEngine::headless();
        let audio = crate::audio::AudioRuntime::headless();

        let off = command(r#"{"v":1,"type":"ToggleMaster","payload":{"module":"crosshair"}}"#);
        let effect = execute(&off, &mut config, &store.path, &store.data).unwrap();
        apply_runtime_effect(toggle_effect(effect), &config, &overlay, &mut display, &audio);
        let hidden = overlay.state();
        assert!(!hidden.enabled && hidden.hidden && !hidden.visible, "crosshair off must hide the real overlay state: {hidden:?}");
        assert_eq!(hidden.renders, 0, "crosshair off must not repaint the overlay");

        let on = command(r#"{"v":1,"type":"ToggleMaster","payload":{"module":"crosshair"}}"#);
        let effect = execute(&on, &mut config, &store.path, &store.data).unwrap();
        apply_runtime_effect(toggle_effect(effect), &config, &overlay, &mut display, &audio);
        overlay.record_render();
        let shown = overlay.state();
        assert!(shown.enabled && shown.visible && !shown.hidden, "crosshair on must restore overlay visibility: {shown:?}");
        assert!(shown.render_requests > hidden.render_requests, "crosshair on must request a repaint: {shown:?}");
    }

    #[test]
    fn header_display_toggle_calls_display_and_updates_applied_state() {
        let store = TempStore::new("header-display-runtime");
        let mut config = store.load();
        config.display.enabled = true;
        let overlay = crate::overlay::OverlayHandle::headless();
        let mut display = crate::display::DisplayEngine::headless();
        let audio = crate::audio::AudioRuntime::headless();

        let off = command(r#"{"v":1,"type":"ToggleMaster","payload":{"module":"display"}}"#);
        let effect = execute(&off, &mut config, &store.path, &store.data).unwrap();
        apply_runtime_effect(toggle_effect(effect), &config, &overlay, &mut display, &audio);
        let restored = display.state();
        assert!(!restored.enabled && !restored.applied && !restored.needs_restore, "display off must restore and clear applied state: {restored:?}");
        assert_eq!(restored.restore_calls, 1, "display off must invoke the restore path: {restored:?}");

        let on = command(r#"{"v":1,"type":"ToggleMaster","payload":{"module":"display"}}"#);
        let effect = execute(&on, &mut config, &store.path, &store.data).unwrap();
        apply_runtime_effect(toggle_effect(effect), &config, &overlay, &mut display, &audio);
        let applied = display.state();
        assert!(applied.enabled && applied.applied && applied.needs_restore, "display on must apply and record restore need: {applied:?}");
        assert!(applied.apply_calls > restored.apply_calls, "display on must invoke the apply path: {applied:?}");
    }

    #[test]
    fn header_audio_toggle_changes_audio_runtime_state() {
        let store = TempStore::new("header-audio-runtime");
        let mut config = store.load();
        config.audio.enabled = true;
        let overlay = crate::overlay::OverlayHandle::headless();
        let mut display = crate::display::DisplayEngine::headless();
        let audio = crate::audio::AudioRuntime::headless();

        let off = command(r#"{"v":1,"type":"ToggleMaster","payload":{"module":"audio"}}"#);
        let effect = execute(&off, &mut config, &store.path, &store.data).unwrap();
        apply_runtime_effect(toggle_effect(effect), &config, &overlay, &mut display, &audio);
        let muted = audio.state();
        assert!(!muted.enabled, "audio off must disable the audio runtime: {muted:?}");
        assert_eq!(muted.apply_calls, 1, "audio off must invoke the audio apply path: {muted:?}");

        let on = command(r#"{"v":1,"type":"ToggleMaster","payload":{"module":"audio"}}"#);
        let effect = execute(&on, &mut config, &store.path, &store.data).unwrap();
        apply_runtime_effect(toggle_effect(effect), &config, &overlay, &mut display, &audio);
        let enabled = audio.state();
        assert!(enabled.enabled, "audio on must re-enable the audio runtime: {enabled:?}");
        assert!(enabled.apply_calls > muted.apply_calls, "audio on must invoke the audio apply path: {enabled:?}");
    }

    #[test]
    fn maintenance_actions_have_observable_effects() {
        let data_store = TempStore::new("data-folder");
        let data = data_store.data.join("nested");
        assert!(!data.exists());
        let effect = execute(&Command::OpenDataFolder, &mut data_store.load(), &data_store.path, &data).unwrap();
        assert_eq!(effect, Effect::DataFolderReady(data.clone()));
        assert!(data.exists(), "open data folder did not create the folder");

        let reset_store = TempStore::new("factory-reset");
        let images = reset_store.data.join("crosshairs");
        fs::create_dir_all(&images).unwrap();
        fs::write(images.join("imported.png"), b"x").unwrap();
        let stale = crate::config::backup_path(&reset_store.path);
        fs::write(&stale, b"{}").unwrap();
        let tmp = reset_store.path.with_extension("tmp");
        fs::write(&tmp, b"{}").unwrap();
        let mut custom = reset_store.load();
        custom.crosshair.enabled = true;
        custom.settings.start_with_windows = true;
        custom.crosshair.image = Some(images.join("imported.png").to_string_lossy().to_string());
        custom.save_to(&reset_store.path).unwrap();
        let effect = execute(&Command::FactoryReset, &mut reset_store.load(), &reset_store.path, &reset_store.data).unwrap();
        assert_eq!(effect, Effect::FactoryReset { removed_config: true, removed_images: 1 });
        assert_eq!(reset_store.load(), Config::default(), "factory reset did not persist defaults");
        assert!(!images.exists(), "factory reset left imported images behind");
        assert!(!stale.exists(), "factory reset left the erased-config backup behind");
        assert!(!tmp.exists(), "factory reset left a stale atomic-save temp file behind");

        let display_store = TempStore::new("reset-display");
        let mut custom = display_store.load();
        custom.display.global.gamma = 0.71;
        custom.save_to(&display_store.path).unwrap();
        assert_eq!(execute(&Command::ResetDisplay, &mut display_store.load(), &display_store.path, &display_store.data).unwrap(), Effect::DisplayReset);
        assert_eq!(display_store.load().display, DisplayConfig::default());
    }

    #[test]
    fn unknown_module_is_rejected_without_writing() {
        let store = TempStore::new("reject");
        let before = fs::read_to_string(&store.path).unwrap();
        let err = execute(&Command::ToggleMaster { module: "nope".into() }, &mut store.load(), &store.path, &store.data).unwrap_err();
        assert!(err.contains("unknown module"));
        assert_eq!(fs::read_to_string(&store.path).unwrap(), before);
    }

    #[test]
    fn in_memory_toggle_does_not_write_the_store() {
        let store = TempStore::new("capture-memory");
        let before = fs::read_to_string(&store.path).unwrap();
        let mut config = store.load();
        let effect = toggle_module_in_memory(&mut config, "crosshair").unwrap();
        assert_eq!(effect, ToggleEffect::Crosshair);
        assert_eq!(fs::read_to_string(&store.path).unwrap(), before);
    }

    #[test]
    fn invalid_preset_and_slot_targets_are_rejected() {
        let store = TempStore::new("validation");
        let config = store.load();
        assert!(validate_preset_name("crosshair", "  ").is_err());
        assert!(validate_preset_name("bogus", "Name").is_err());
        assert!(validate_custom_preset(&config, "audio", "missing").is_err());
        assert!(!valid_tab("unknown") && valid_tab("slots"));
    }

    #[test]
    fn external_commands_declare_their_effect() {
        let store = TempStore::new("external");
        for command in [Command::SetupAudio, Command::RepairAudio, Command::UnstickAudio, Command::OpenFxSound, Command::OpenWindowsDisplaySettings] {
            let effect = execute(&command, &mut store.load(), &store.path, &store.data).unwrap();
            assert!(matches!(effect, Effect::External(_)));
        }
    }
}