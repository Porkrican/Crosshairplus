use tray_icon::{TrayIcon, TrayIconBuilder};
use serde::Serialize;

use crate::config::Config;
use crate::render::scene;

pub struct TrayHandles {
    pub tray: TrayIcon,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModuleEntry {
    pub id: &'static str,
    pub label: &'static str,
    pub emoji: &'static str,
    pub enabled: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SlotEntry {
    pub id: String,
    pub name: String,
    pub emoji: String,
    pub active: bool,
}

/// The three module rows shown at the top of the themed tray flyout.
pub fn module_entries(config: &Config) -> [ModuleEntry; 3] {
    [
        ModuleEntry { id: "crosshair", label: "Aim", emoji: "🎯", enabled: config.crosshair.enabled },
        ModuleEntry { id: "display", label: "Display", emoji: "🖥️", enabled: config.display.enabled },
        ModuleEntry { id: "audio", label: "Sound", emoji: "🔊", enabled: config.audio.enabled },
    ]
}

/// Slots in config order, including the emoji and active marker the flyout
/// needs. The native Windows menu was never able to render these faithfully.
pub fn slot_entries(config: &Config) -> Vec<SlotEntry> {
    config
        .slots
        .list
        .iter()
        .map(|slot| SlotEntry {
            id: slot.id.clone(),
            name: slot.name.clone(),
            emoji: if slot.emoji.is_empty() { "⚡".into() } else { slot.emoji.clone() },
            active: config.slots.active.as_deref() == Some(slot.id.as_str()),
        })
        .collect()
}

pub fn create(_config: &Config) -> Result<TrayHandles, Box<dyn std::error::Error>> {
    let icon = scene::icon_rgba(32).ok().and_then(|(rgba, size)| tray_icon::Icon::from_rgba(rgba, size, size).ok());
    let tray = TrayIconBuilder::new()
        .with_icon(icon.ok_or("tray icon unavailable")?)
        .with_tooltip("Crosshair+")
        .build()?;
    Ok(TrayHandles { tray })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn module_rows_use_the_requested_labels_and_state() {
        let mut config = Config::default();
        config.crosshair.enabled = true;
        config.display.enabled = false;
        config.audio.enabled = true;
        let entries = module_entries(&config);
        assert_eq!(entries.iter().map(|entry| entry.id).collect::<Vec<_>>(), vec!["crosshair", "display", "audio"]);
        assert_eq!(entries.iter().map(|entry| entry.label).collect::<Vec<_>>(), vec!["Aim", "Display", "Sound"]);
        assert_eq!(entries.iter().map(|entry| entry.emoji).collect::<Vec<_>>(), vec!["🎯", "🖥️", "🔊"]);
        assert_eq!(entries.iter().map(|entry| entry.enabled).collect::<Vec<_>>(), vec![true, false, true]);
    }

    #[test]
    fn slot_rows_mark_the_active_slot_and_keep_emoji() {
        let mut config = Config::default();
        config.slots.list[0].emoji = "🎮".into();
        config.slots.active = Some(config.slots.list[0].id.clone());
        let entries = slot_entries(&config);
        assert_eq!(entries.len(), config.slots.list.len());
        assert_eq!(entries[0].emoji, "🎮");
        assert!(entries[0].active);
        assert!(entries.iter().skip(1).all(|entry| !entry.active));
    }

    #[test]
    fn empty_slot_emoji_falls_back_to_the_slot_mark() {
        let mut config = Config::default();
        config.slots.list[0].emoji.clear();
        assert_eq!(slot_entries(&config)[0].emoji, "⚡");
    }
}
