//! Resource sampling and verdict plumbing for the `--stress` QA harness.
//!
//! Counts are per process. WebView2 hosts the page in its own child processes,
//! so these numbers describe this process's GDI/USER/handle/working-set trend.

use std::sync::atomic::{AtomicBool, Ordering};

use windows::Win32::Foundation::HANDLE;
use windows::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
use windows::Win32::System::Threading::{GetCurrentProcess, GetGuiResources, GetProcessHandleCount, GR_GDIOBJECTS, GR_USEROBJECTS};

static FAILED: AtomicBool = AtomicBool::new(false);

pub fn mark_failed() {
    FAILED.store(true, Ordering::SeqCst);
}

pub fn failed() -> bool {
    FAILED.load(Ordering::SeqCst)
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Metrics {
    pub gdi: u32,
    pub user: u32,
    pub handles: u32,
    pub working_set_kb: u64,
}

impl Metrics {
    pub fn sample() -> Self {
        unsafe {
            let process: HANDLE = GetCurrentProcess();
            let mut handles = 0u32;
            let _ = GetProcessHandleCount(process, &mut handles);
            let mut counters = PROCESS_MEMORY_COUNTERS::default();
            let _ = GetProcessMemoryInfo(
                process,
                &mut counters as *mut PROCESS_MEMORY_COUNTERS,
                std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
            );
            Self {
                gdi: GetGuiResources(process, GR_GDIOBJECTS),
                user: GetGuiResources(process, GR_USEROBJECTS),
                handles,
                working_set_kb: counters.WorkingSetSize as u64 / 1024,
            }
        }
    }

    /// Growth over a baseline, saturating so a drop never reads as a leak.
    pub fn growth_over(self, baseline: Self) -> Self {
        Self {
            gdi: self.gdi.saturating_sub(baseline.gdi),
            user: self.user.saturating_sub(baseline.user),
            handles: self.handles.saturating_sub(baseline.handles),
            working_set_kb: self.working_set_kb.saturating_sub(baseline.working_set_kb),
        }
    }

    pub fn as_json(self) -> serde_json::Value {
        serde_json::json!({
            "gdi": self.gdi,
            "user": self.user,
            "handles": self.handles,
            "workingSetKb": self.working_set_kb,
        })
    }
}

/// Budget for a 120-tab / 900-message / 300-redraw run: generous enough for
/// transient caches, tight enough to catch a per-cycle leak.
pub fn growth_within_budget(growth: Metrics) -> bool {
    growth.gdi <= 32 && growth.user <= 24 && growth.handles <= 128 && growth.working_set_kb <= 128 * 1024
}

pub fn describe(growth: Metrics) -> String {
    format!(
        "gdi=+{} user=+{} handles=+{} working_set_kb=+{}",
        growth.gdi, growth.user, growth.handles, growth.working_set_kb
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn growth_saturates_and_the_budget_is_directional() {
        let base = Metrics { gdi: 10, user: 10, handles: 100, working_set_kb: 10_000 };
        let shrunk = Metrics { gdi: 8, user: 9, handles: 90, working_set_kb: 9_000 }.growth_over(base);
        assert_eq!(shrunk, Metrics::default(), "a smaller sample must not read as growth");
        let grown = Metrics { gdi: 60, user: 30, handles: 100, working_set_kb: 10_000 }.growth_over(base);
        assert!(!growth_within_budget(grown));
        assert!(growth_within_budget(Metrics { gdi: 4, user: 2, handles: 8, working_set_kb: 4096 }));
    }
}
