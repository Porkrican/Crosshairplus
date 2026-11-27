//! Silent HDR/ACM return recovery.
//!
//! Windows drops the desktop gamma ramp when HDR or Advanced Color turns on and
//! does not put the user's calibration back when it turns off. The monitor
//! refresh that follows `WM_DISPLAYCHANGE` is the reliable moment to re-assert
//! the active LUT. Tracking which displays were blocked keeps that extra apply
//! limited to a real blocked -> available transition.

use std::collections::BTreeSet;

use crate::display::warning;
use crate::monitors::MonitorInfo;

#[derive(Default, Debug)]
pub struct BlockedDisplays {
    blocked: BTreeSet<String>,
}

fn blocked(monitor: &MonitorInfo) -> bool {
    matches!(
        warning::for_monitor(monitor).map(|warning| warning.code),
        Some(ref code) if code == "hdr" || code == "acm"
    )
}

impl BlockedDisplays {
    /// Update the tracked set from a fresh monitor probe and return the ids
    /// that left HDR/ACM blocking. A display that disappeared is forgotten
    /// without a trigger: there is nothing to re-apply on it.
    pub fn observe(&mut self, monitors: &[MonitorInfo]) -> Vec<String> {
        let current: BTreeSet<String> = monitors
            .iter()
            .filter(|monitor| blocked(monitor))
            .map(|monitor| monitor.id.clone())
            .collect();
        let released: Vec<String> = self
            .blocked
            .difference(&current)
            .filter(|id| monitors.iter().any(|monitor| &monitor.id == *id))
            .cloned()
            .collect();
        self.blocked = current;
        released
    }

    #[cfg(test)]
    pub fn tracked(&self) -> &BTreeSet<String> {
        &self.blocked
    }
}

/// The silent recovery only matters while Display owns the desktop and the user
/// is not holding Compare, which intentionally shows the uncalibrated view.
pub fn should_reapply(display_enabled: bool, compare_active: bool, released: &[String]) -> bool {
    display_enabled && !compare_active && !released.is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn monitor(id: &str, hdr: bool, acm: bool) -> MonitorInfo {
        MonitorInfo {
            id: id.into(),
            name: id.into(),
            hdr,
            advanced_color: acm,
            ..Default::default()
        }
    }

    #[test]
    fn only_blocked_to_available_transitions_are_released() {
        let mut state = BlockedDisplays::default();
        assert!(state
            .observe(&[monitor("a", true, false), monitor("b", false, false)])
            .is_empty());
        assert_eq!(
            state.observe(&[monitor("a", false, false), monitor("b", false, false)]),
            vec!["a".to_string()]
        );
        assert!(
            state.observe(&[monitor("a", false, false), monitor("b", false, false)]).is_empty(),
            "a repeated SDR refresh must not re-trigger"
        );
        assert!(
            state.observe(&[monitor("a", false, true)]).is_empty(),
            "HDR to ACM stays blocked"
        );
        assert_eq!(state.observe(&[monitor("a", false, false)]), vec!["a".to_string()]);
    }

    #[test]
    fn disconnected_displays_are_forgotten_without_a_trigger() {
        let mut state = BlockedDisplays::default();
        state.observe(&[monitor("a", true, false), monitor("b", false, true)]);
        assert!(
            state.observe(&[monitor("a", true, false)]).is_empty(),
            "unplugging a blocked display is not an HDR->SDR return"
        );
        assert_eq!(state.tracked().len(), 1);
        assert!(state.tracked().contains("a"));
    }

    #[test]
    fn recovery_needs_display_on_and_no_compare_hold() {
        let released = vec!["a".to_string()];
        assert!(should_reapply(true, false, &released));
        assert!(!should_reapply(false, false, &released), "Display off must stay off");
        assert!(!should_reapply(true, true, &released), "a held Compare must keep the neutral view");
        assert!(!should_reapply(true, false, &[]), "no transition, no extra apply");
    }
}
