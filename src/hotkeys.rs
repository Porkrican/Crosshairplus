use crate::config::Config;
use crate::overlay::HotkeyBinding;
use crate::slots;

pub const AIM: i32 = 1;
pub const RESET: i32 = 2;
pub const DISPLAY: i32 = 3;
pub const AUDIO: i32 = 4;
pub const RESTORE: i32 = 5;
pub const CYCLE: i32 = 6;
pub const SLOT_BASE: i32 = 100;

pub fn bindings(config: &Config) -> Vec<HotkeyBinding> {
    if !config.settings.hotkeys_enabled {
        return Vec::new();
    }
    let mut bindings = vec![
        HotkeyBinding { id: RESET, mods: 0x0002 | 0x0004, vk: 0x23, key: "Ctrl+Shift+End".into(), slot_id: None },
    ];
    for (id, value) in [
        (AIM, config.settings.aim_hotkey.as_str()),
        (DISPLAY, config.settings.display_hotkey.as_str()),
        (AUDIO, config.settings.audio_hotkey.as_str()),
        (RESTORE, config.settings.restore_hotkey.as_str()),
        (CYCLE, config.settings.cycle_hotkey.as_str()),
    ] {
        let Some((mods, vk)) = slots::parse_hotkey(value) else { continue };
        bindings.push(HotkeyBinding {
            id,
            mods,
            vk,
            key: slots::canonical_hotkey(value).unwrap_or_else(|| value.to_string()),
            slot_id: None,
        });
    }
    for (index, slot) in config.slots.list.iter().take(crate::config::MAX_SLOTS).enumerate() {
        let Some(text) = slot.hotkey.as_deref().filter(|h| !h.trim().is_empty()) else { continue };
        let Some((mods, vk)) = slots::parse_hotkey(text) else { continue };
        bindings.push(HotkeyBinding {
            id: SLOT_BASE + index as i32,
            mods,
            vk,
            key: text.to_string(),
            slot_id: Some(slot.id.clone()),
        });
    }
    bindings
}

pub fn slot_for_hotkey_id(config: &Config, id: i32) -> Option<String> {
    let index = (id - SLOT_BASE) as usize;
    config.slots.list.get(index).map(|s| s.id.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_bindings_include_aim_reset_and_slots() {
        let config = Config::default();
        let default_bindings = bindings(&config);
        assert!(default_bindings.iter().any(|b| b.id == AIM && b.key == "F8"));
        assert!(default_bindings.iter().any(|b| b.id == RESET && b.key == "Ctrl+Shift+End"));
        assert!(!default_bindings.iter().any(|b| b.id == DISPLAY || b.id == AUDIO || b.id == RESTORE || b.id == CYCLE));
        assert_eq!(default_bindings.iter().filter(|b| b.slot_id.is_some()).count(), 3);

        let mut custom = Config::default();
        custom.settings.display_hotkey = "Ctrl+Shift+1".into();
        custom.settings.audio_hotkey = "Alt+F7".into();
        custom.settings.restore_hotkey = "Ctrl+Shift+F12".into();
        custom.settings.cycle_hotkey = "Ctrl+Shift+F11".into();
        let custom_bindings = bindings(&custom);
        assert!(custom_bindings.iter().any(|b| b.id == DISPLAY && b.key == "Ctrl+Shift+1"));
        assert!(custom_bindings.iter().any(|b| b.id == RESTORE && b.key == "Ctrl+Shift+F12"));
        assert!(custom_bindings.iter().any(|b| b.id == AUDIO && b.key == "Alt+F7"));
        assert!(custom_bindings.iter().any(|b| b.id == CYCLE && b.key == "Ctrl+Shift+F11"));
        // Cycle stays below SLOT_BASE so it can never be mistaken for a slot.
        assert!(custom_bindings.iter().filter(|b| b.slot_id.is_some()).all(|b| b.id >= SLOT_BASE));
        assert_eq!(slot_for_hotkey_id(&config, SLOT_BASE), Some("slot-fps".into()));
    }

    #[test]
    fn disabling_hotkeys_removes_every_binding() {
        let mut config = Config::default();
        config.settings.hotkeys_enabled = false;
        assert!(bindings(&config).is_empty());
    }
}
