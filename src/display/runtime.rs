//! Off-UI-thread display application.
//!
//! `SetDeviceGammaRamp` and the verification read-back talk to the display
//! driver and can stall for seconds on some systems. The UI only sends the
//! latest configuration; the worker owns the baselines and reports outcomes
//! back through the event loop.
//!
//! The mailbox is deliberately a small state machine rather than a FIFO. A
//! slider drag can produce dozens of applies while one driver call is still in
//! flight; only the newest apply matters. Restore must never wait behind those
//! applies, and a neutral preview (Hold to Compare) takes precedence over a
//! queued calibration apply.

use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::Duration;

use tao::event_loop::EventLoopProxy;

use crate::app::AppEvent;
use crate::config::DisplayConfig;
use crate::display::DisplayEngine;
use crate::monitors::MonitorInfo;

struct ApplyJob {
    seq: u64,
    monitors: Vec<MonitorInfo>,
    config: DisplayConfig,
}

enum DisplayJob {
    Apply(ApplyJob),
    Neutral { monitors: Vec<MonitorInfo>, config: DisplayConfig },
    EnsureBaselines(Vec<MonitorInfo>),
    Restore(Vec<Sender<()>>),
    Shutdown,
}

#[derive(Default)]
struct MailboxState {
    apply: Option<ApplyJob>,
    next_seq: u64,
    neutral: Option<(Vec<MonitorInfo>, DisplayConfig)>,
    ensure: Option<Vec<MonitorInfo>>,
    restore: bool,
    restore_acks: Vec<Sender<()>>,
    shutdown: bool,
}

#[derive(Default)]
struct DisplayMailbox {
    state: Mutex<MailboxState>,
    ready: Condvar,
}

impl DisplayMailbox {
    fn apply(&self, monitors: &[MonitorInfo], config: &DisplayConfig) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.next_seq += 1;
        state.apply = Some(ApplyJob {
            seq: state.next_seq,
            monitors: monitors.to_vec(),
            config: config.clone(),
        });
        self.ready.notify_one();
    }

    fn neutral(&self, monitors: &[MonitorInfo], config: &DisplayConfig) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.neutral = Some((monitors.to_vec(), config.clone()));
        self.ready.notify_one();
    }

    fn ensure_baselines(&self, monitors: &[MonitorInfo]) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.ensure = Some(monitors.to_vec());
        self.ready.notify_one();
    }

    /// Restore is the highest-priority job. Clearing the queued calibration
    /// work here is what makes suspend/exit deterministic even when a driver
    /// call or a slider burst is still queued.
    fn restore(&self, ack: Sender<()>) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.restore = true;
        state.restore_acks.push(ack);
        state.apply = None;
        state.neutral = None;
        state.ensure = None;
        self.ready.notify_one();
    }

    fn shutdown(&self) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.shutdown = true;
        state.apply = None;
        state.neutral = None;
        state.ensure = None;
        self.ready.notify_one();
    }

    fn take(state: &mut MailboxState) -> Option<DisplayJob> {
        if state.restore {
            state.restore = false;
            return Some(DisplayJob::Restore(std::mem::take(&mut state.restore_acks)));
        }
        if let Some((monitors, config)) = state.neutral.take() {
            return Some(DisplayJob::Neutral { monitors, config });
        }
        if let Some(job) = state.apply.take() {
            return Some(DisplayJob::Apply(job));
        }
        if let Some(monitors) = state.ensure.take() {
            return Some(DisplayJob::EnsureBaselines(monitors));
        }
        if state.shutdown {
            return Some(DisplayJob::Shutdown);
        }
        None
    }

    fn next(&self) -> DisplayJob {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            if let Some(job) = Self::take(&mut state) {
                return job;
            }
            state = self.ready.wait(state).unwrap_or_else(|e| e.into_inner());
        }
    }

    #[cfg(test)]
    fn try_next(&self) -> Option<DisplayJob> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        Self::take(&mut state)
    }

    /// An apply outcome is stale when another apply, a restore or shutdown was
    /// requested while the driver call was running. The newer work owns the
    /// eventual UI state; replaying the old LUT would make the sliders jump.
    fn is_current(&self, seq: u64) -> bool {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        seq == state.next_seq && !state.restore && !state.shutdown
    }
}

pub struct DisplayRuntime {
    mailbox: Arc<DisplayMailbox>,
}

impl DisplayRuntime {
    pub fn start(proxy: EventLoopProxy<AppEvent>, monitors: Vec<MonitorInfo>, preview_only: bool) -> Self {
        let mailbox = Arc::new(DisplayMailbox::default());
        let worker_mailbox = mailbox.clone();
        thread::spawn(move || {
            // A preview-only worker must not even capture the real baseline:
            // its shutdown restore would otherwise write those ramps back.
            let mut engine = if preview_only {
                DisplayEngine::preview_only()
            } else {
                DisplayEngine::new(&monitors)
            };
            loop {
                match worker_mailbox.next() {
                    DisplayJob::Apply(job) => {
                        let outcome = engine.apply(&job.monitors, &job.config);
                        if worker_mailbox.is_current(job.seq) {
                            let _ = proxy.send_event(AppEvent::DisplayOutcome(outcome));
                        } else {
                            crate::logging::log(&format!(
                                "display: dropped stale outcome seq={} (current={})",
                                job.seq,
                                worker_mailbox.state.lock().unwrap_or_else(|e| e.into_inner()).next_seq
                            ));
                        }
                    }
                    DisplayJob::Neutral { monitors, config } => {
                        crate::logging::log("display: neutral preview requested");
                        engine.preview_neutral(&monitors, &config);
                    }
                    DisplayJob::EnsureBaselines(monitors) => engine.ensure_baselines(&monitors),
                    DisplayJob::Restore(acks) => {
                        engine.restore_all();
                        for ack in acks {
                            let _ = ack.send(());
                        }
                    }
                    DisplayJob::Shutdown => {
                        engine.restore_all();
                        break;
                    }
                }
            }
        });
        Self { mailbox }
    }

    pub fn apply(&self, monitors: &[MonitorInfo], config: &DisplayConfig) {
        self.mailbox.apply(monitors, config);
    }

    /// Write identity ramps to the shown monitors without changing the saved
    /// baselines or the engine's applied/restore bookkeeping. This is the
    /// temporary Hold-to-Compare path, not a calibration change.
    pub fn preview_neutral(&self, monitors: &[MonitorInfo], config: &DisplayConfig) {
        self.mailbox.neutral(monitors, config);
    }

    pub fn ensure_baselines(&self, monitors: &[MonitorInfo]) {
        self.mailbox.ensure_baselines(monitors);
    }

    /// Restore on the worker and wait briefly. Reserved for suspend, exit and
    /// explicit reset actions where completion matters more than latency.
    pub fn restore_blocking(&self) {
        let (ack, done) = mpsc::channel();
        self.mailbox.restore(ack);
        let _ = done.recv_timeout(Duration::from_secs(2));
    }

    pub fn shutdown(&self) {
        self.restore_blocking();
        self.mailbox.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn monitors() -> Vec<MonitorInfo> {
        vec![MonitorInfo::default()]
    }

    #[test]
    fn newest_apply_replaces_queued_applies() {
        let mailbox = DisplayMailbox::default();
        mailbox.apply(&monitors(), &DisplayConfig::default());
        mailbox.apply(&monitors(), &DisplayConfig::default());
        let Some(DisplayJob::Apply(job)) = mailbox.try_next() else { panic!("expected one apply") };
        assert_eq!(job.seq, 2);
        assert!(!mailbox.is_current(1), "the replaced apply outcome is stale");
        assert!(mailbox.is_current(2));
        assert!(mailbox.try_next().is_none(), "only the latest apply should run");
    }

    #[test]
    fn neutral_preview_preempts_a_queued_apply() {
        let mailbox = DisplayMailbox::default();
        mailbox.apply(&monitors(), &DisplayConfig::default());
        mailbox.neutral(&monitors(), &DisplayConfig::default());
        assert!(matches!(mailbox.try_next(), Some(DisplayJob::Neutral { .. })));
        assert!(matches!(mailbox.try_next(), Some(DisplayJob::Apply(_))));
        assert!(mailbox.try_next().is_none());
    }

    #[test]
    fn restore_cancels_queued_work_and_acks() {
        let mailbox = DisplayMailbox::default();
        mailbox.apply(&monitors(), &DisplayConfig::default());
        mailbox.neutral(&monitors(), &DisplayConfig::default());
        mailbox.ensure_baselines(&monitors());
        let (ack, done) = mpsc::channel();
        mailbox.restore(ack);
        let Some(DisplayJob::Restore(acks)) = mailbox.try_next() else { panic!("expected restore") };
        assert_eq!(acks.len(), 1);
        for ack in acks {
            ack.send(()).unwrap();
        }
        assert!(done.recv_timeout(Duration::from_secs(1)).is_ok());
        assert!(mailbox.try_next().is_none(), "restore must clear apply, neutral and baseline work");
    }

    #[test]
    fn baseline_work_is_lower_priority_than_apply() {
        let mailbox = DisplayMailbox::default();
        mailbox.ensure_baselines(&monitors());
        mailbox.apply(&monitors(), &DisplayConfig::default());
        assert!(matches!(mailbox.try_next(), Some(DisplayJob::Apply(_))));
        assert!(matches!(mailbox.try_next(), Some(DisplayJob::EnsureBaselines(_))));
    }

    #[test]
    fn shutdown_discards_queued_work_and_invalidates_outcomes() {
        let mailbox = DisplayMailbox::default();
        mailbox.apply(&monitors(), &DisplayConfig::default());
        mailbox.neutral(&monitors(), &DisplayConfig::default());
        mailbox.shutdown();
        assert!(matches!(mailbox.try_next(), Some(DisplayJob::Shutdown)));
        assert!(!mailbox.is_current(1));
        assert!(!matches!(
            mailbox.try_next(),
            Some(DisplayJob::Apply(_)) | Some(DisplayJob::Neutral { .. }) | Some(DisplayJob::EnsureBaselines(_))
        ));
    }
}
