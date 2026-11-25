pub mod apo_import;
pub mod dsp_math;
pub mod fxsound;
pub mod fxsound_stub;
pub mod verify;
pub mod wasapi_meter;

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tao::event_loop::EventLoopProxy;

use crate::app::AppEvent;
use crate::config::AudioConfig;
use crate::ipc::DiagnosticAction;

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct AudioDevice {
    pub id: String,
    pub name: String,
    pub is_default: bool,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct FxSoundStatus {
    pub installed: bool,
    pub running: bool,
    pub version: String,
    pub cli_ready: bool,
    pub power: bool,
    /// FxSound's selected preset. Crosshair+ selects its own managed preset
    /// before changing EQ values so user presets stay untouched.
    pub preset: Option<String>,
    pub num_bands: usize,
    pub output: Option<String>,
    pub message: String,
    pub path: Option<String>,
    pub filter_q: f32,
    pub latest_version: Option<String>,
    pub update_available: bool,
    pub busy: bool,
    pub task: Option<String>,
}

enum AudioCmd {
    Apply(AudioConfig, bool),
    Setup,
    Repair,
    RepairPreset(AudioConfig),
    Unstick(AudioConfig),
    Open,
    Refresh { force: bool },
    Shutdown,
}

/// Every piece of shared FxSound state lives behind one lock.
///
/// The previous layout used three separate mutexes and composed them while one
/// was already held; `std::sync::Mutex` is not re-entrant, so the first update
/// check (four seconds after launch) deadlocked the worker that held `latest`
/// and every status reader behind it. One lock plus single-snapshot reads keeps
/// that class of bug structurally impossible.
#[derive(Default)]
struct StatusCache {
    status: FxSoundStatus,
    latest: Option<String>,
    busy: Option<String>,
}

impl StatusCache {
    fn snapshot(&self) -> FxSoundStatus {
        let mut snapshot = self.status.clone();
        snapshot.latest_version = self.latest.clone();
        snapshot.update_available = snapshot.installed
            && snapshot
                .latest_version
                .as_deref()
                .map(|version| fxsound::version_is_newer(version, &snapshot.version))
                .unwrap_or(false);
        snapshot.task = self.busy.clone();
        snapshot.busy = snapshot.task.is_some();
        snapshot
    }
}

fn cache_lock(cache: &Mutex<StatusCache>) -> MutexGuard<'_, StatusCache> {
    cache.lock().unwrap_or_else(|held| held.into_inner())
}

/// Read a consistent status without ever holding the lock across other work.
fn snapshot(cache: &Mutex<StatusCache>) -> FxSoundStatus {
    cache_lock(cache).snapshot()
}

fn store_status(cache: &Mutex<StatusCache>, status: FxSoundStatus) {
    cache_lock(cache).status = status;
}

fn set_busy(cache: &Mutex<StatusCache>, task: Option<String>) {
    cache_lock(cache).busy = task;
}

/// Remember the latest published version. Returns true only when it changed.
/// The guard is dropped before the caller publishes a snapshot.
fn note_latest(cache: &Mutex<StatusCache>, version: &str) -> bool {
    let mut cache = cache_lock(cache);
    if cache.latest.as_deref() == Some(version) {
        return false;
    }
    cache.latest = Some(version.to_string());
    true
}

/// The exact path the update loop runs: note a version, then build the status
/// that gets published. Keeping it in one function lets the regression test
/// exercise the real sequence instead of a copy.
fn publish_latest(cache: &Mutex<StatusCache>, version: &str) -> Option<FxSoundStatus> {
    if note_latest(cache, version) {
        Some(snapshot(cache))
    } else {
        None
    }
}

pub struct AudioRuntime {
    tx: mpsc::Sender<AudioCmd>,
    meter_enabled: Arc<AtomicBool>,
    device: Arc<Mutex<Option<String>>>,
    cache: Arc<Mutex<StatusCache>>,
    update_check: Arc<AtomicBool>,
    proxy: Option<EventLoopProxy<AppEvent>>,
    state: Arc<Mutex<AudioRuntimeState>>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AudioRuntimeState {
    pub enabled: bool,
    pub apply_calls: u64,
    pub last_device: Option<String>,
    pub meter_enabled: bool,
}

impl AudioRuntime {
    pub fn start(proxy: EventLoopProxy<AppEvent>) -> Self {
        let (tx, rx) = mpsc::channel::<AudioCmd>();
        let meter_enabled = Arc::new(AtomicBool::new(false));
        let device = Arc::new(Mutex::new(None));
        let cache = Arc::new(Mutex::new(StatusCache {
            status: fxsound::status(),
            ..Default::default()
        }));
        let update_check = Arc::new(AtomicBool::new(false));
        let state = Arc::new(Mutex::new(AudioRuntimeState::default()));

        {
            let proxy = proxy.clone();
            let device = device.clone();
            let cache = cache.clone();
            thread::spawn(move || audio_worker(rx, proxy, device, cache));
        }
        {
            let proxy = proxy.clone();
            let enabled = meter_enabled.clone();
            let device = device.clone();
            thread::spawn(move || meter_worker(proxy, enabled, device));
        }
        Self {
            tx,
            meter_enabled,
            device,
            cache,
            update_check,
            proxy: Some(proxy),
            state,
        }
    }

    #[cfg(test)]
    pub fn headless() -> Self {
        let (tx, _rx) = mpsc::channel();
        Self {
            tx,
            meter_enabled: Arc::new(AtomicBool::new(false)),
            device: Arc::new(Mutex::new(None)),
            cache: Arc::new(Mutex::new(StatusCache::default())),
            update_check: Arc::new(AtomicBool::new(false)),
            proxy: None,
            state: Arc::new(Mutex::new(AudioRuntimeState::default())),
        }
    }

    fn queue_apply(&self, config: AudioConfig, quiet: bool) {
        {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            state.enabled = config.enabled;
            state.apply_calls += 1;
            state.last_device = config.device.clone();
        }
        *self.device.lock().unwrap_or_else(|e| e.into_inner()) = config.device.clone();
        if self.tx.send(AudioCmd::Apply(config, quiet)).is_err() {
            crate::logging::log("audio: worker unavailable; apply request dropped");
        }
    }

    pub fn apply(&self, config: AudioConfig) {
        self.queue_apply(config, false);
    }

    /// Startup reconciliation is best-effort: it must never turn a closed or
    /// busy FxSound into a red launch error.
    pub fn reconcile(&self, config: AudioConfig) {
        self.queue_apply(config, true);
    }
    pub fn setup(&self) {
        if self.tx.send(AudioCmd::Setup).is_err() {
            crate::logging::log("audio: worker unavailable; setup request dropped");
        }
    }
    pub fn repair(&self) {
        if self.tx.send(AudioCmd::Repair).is_err() {
            crate::logging::log("audio: worker unavailable; repair request dropped");
        }
    }
    pub fn repair_preset(&self, config: AudioConfig) {
        if self.tx.send(AudioCmd::RepairPreset(config)).is_err() {
            crate::logging::log("audio: worker unavailable; preset repair request dropped");
        }
    }
    /// Manual recovery for a stale FxSound output. The worker blocks its queue
    /// while the restart runs, so one request cannot race another apply.
    pub fn unstick(&self, config: AudioConfig) {
        if self.tx.send(AudioCmd::Unstick(config)).is_err() {
            crate::logging::log("audio: worker unavailable; unstick request dropped");
        }
    }
    pub fn open(&self) {
        if self.tx.send(AudioCmd::Open).is_err() {
            crate::logging::log("audio: worker unavailable; open request dropped");
        }
    }
    pub fn refresh(&self, force: bool) {
        let _ = self.tx.send(AudioCmd::Refresh { force });
    }
    pub fn shutdown(&self) {
        let _ = self.tx.send(AudioCmd::Shutdown);
    }
    pub fn set_meter_enabled(&self, enabled: bool) {
        self.meter_enabled.store(enabled, Ordering::Relaxed);
        self.state.lock().unwrap_or_else(|e| e.into_inner()).meter_enabled = enabled;
    }
    #[cfg(test)]
    pub fn state(&self) -> AudioRuntimeState {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
    pub fn status(&self) -> FxSoundStatus {
        snapshot(&self.cache)
    }

    /// Start the low-frequency background update check. The first check runs a
    /// few seconds after launch; later checks run every six hours. Network
    /// failures are silent and never block the UI.
    pub fn check_updates(&self) {
        self.start_update_loop(Duration::from_secs(4), fxsound::latest_release_version);
    }

    /// Run the real update-check publish path without the network. Used by the
    /// interaction and FxSound verification modes so a regression that parks a
    /// status reader is caught instead of skipped.
    pub fn check_updates_with(&self, provider: fn() -> Option<String>, after: Duration) {
        self.start_update_loop(after, provider);
    }

    fn start_update_loop(&self, after: Duration, provider: fn() -> Option<String>) {
        if self.update_check.swap(true, Ordering::Relaxed) {
            return;
        }
        let Some(proxy) = self.proxy.clone() else { return };
        let cache = self.cache.clone();
        thread::spawn(move || {
            let mut delay = after;
            loop {
                thread::sleep(delay);
                delay = Duration::from_secs(6 * 60 * 60);
                let Some(version) = provider() else { continue };
                if let Some(status) = publish_latest(&cache, &version) {
                    crate::logging::log(&format!("fxsound: update check reported {version}"));
                    let _ = proxy.send_event(AppEvent::FxSound(status));
                }
            }
        });
    }

    pub fn devices() -> Vec<AudioDevice> {
        match wasapi_meter::enumerate_render_devices() {
            Ok(devices) => devices,
            Err(err) => {
                crate::logging::log(&format!("audio: render endpoint enumeration failed: {err}"));
                Vec::new()
            }
        }
    }
}

impl Drop for AudioRuntime {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn publish_status(proxy: &EventLoopProxy<AppEvent>, cache: &Mutex<StatusCache>) {
    let _ = proxy.send_event(AppEvent::FxSound(snapshot(cache)));
}

fn action(id: &str, label: &str) -> DiagnosticAction {
    DiagnosticAction { id: id.into(), label: label.into() }
}

fn apply_recovery(report: &fxsound::ApplyReport) -> Option<(String, String, Vec<DiagnosticAction>)> {
    let mut actions = Vec::new();
    let mut parts = Vec::new();
    if let Some(missing) = &report.fallback_device {
        let active = report.active_output.as_deref().unwrap_or("the current Windows default");
        parts.push(format!(
            "Saved output \"{missing}\" isn't connected. FxSound is using \"{active}\"."
        ));
        actions.push(action("use-default-output", "Use Windows default"));
        actions.push(action("choose-output", "Choose device…"));
    }
    if !report.mismatches.is_empty() {
        let shown: Vec<String> = report.mismatches.iter().take(3).cloned().collect();
        let more = report.mismatches.len().saturating_sub(shown.len());
        let mut detail = shown.join(", ");
        if more > 0 {
            detail.push_str(&format!(", and {more} more"));
        }
        parts.push(format!("FxSound ignored: {detail}."));
        actions.push(action("unstick-audio", "Restart FxSound"));
    }
    if parts.is_empty() {
        None
    } else {
        let code = if report.fallback_device.is_some() {
            "audio_output_fallback"
        } else {
            "audio_mismatch"
        };
        Some((code.into(), parts.join(" "), actions))
    }
}

fn preset_conflict_notice(conflict: fxsound::PresetConflict) -> (String, Vec<DiagnosticAction>) {
    let message = match conflict {
        fxsound::PresetConflict::UserFile(path) => format!(
            "A user-owned Crosshair+ preset is blocking managed sound ({}).",
            path.display()
        ),
        fxsound::PresetConflict::StatusOnly => {
            "FxSound is listing a Crosshair+ preset that Crosshair+ does not own.".into()
        }
    };
    (
        message,
        vec![
            action("repair-managed-preset", "Back up existing & recreate managed preset"),
            action("open-fxsound-presets", "Open presets folder"),
        ],
    )
}

fn preset_conflict_recovery() -> Option<(String, Vec<DiagnosticAction>)> {
    Some(preset_conflict_notice(fxsound::managed_preset_conflict()?))
}

fn run_unstick(
    config: &AudioConfig,
    pending: &mut Option<(AudioConfig, bool)>,
    cache: &Mutex<StatusCache>,
    emit: &mut dyn FnMut(AppEvent),
    run: impl FnOnce(&AudioConfig) -> Result<FxSoundStatus, String>,
    current_status: impl Fn() -> FxSoundStatus,
) {
    // A queued slider apply would run after the restart and could put back the
    // stale state we just cleared.
    *pending = None;
    set_busy(cache, Some("Restarting FxSound…".into()));
    emit(AppEvent::FxSound(snapshot(cache)));
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run(config)))
        .unwrap_or_else(|_| Err("FxSound unstick panicked.".into()));
    set_busy(cache, None);
    match result {
        Ok(status) => {
            store_status(cache, status.clone());
            let output = status.output.unwrap_or_else(|| "the current output".into());
            emit(AppEvent::AudioNotice(format!("FxSound restarted on {output}.")));
        }
        Err(err) => {
            crate::logging::log(&format!("fxsound: unstick failed: {err}"));
            let mut status = current_status();
            status.message = "FxSound could not be restarted.".into();
            store_status(cache, status);
            emit(AppEvent::AudioNotice(format!("Could not unstick FxSound: {err}")));
        }
    }
    emit(AppEvent::FxSound(snapshot(cache)));
}

#[allow(clippy::too_many_arguments)]
fn run_repair_preset(
    config: &AudioConfig,
    pending: &mut Option<(AudioConfig, bool)>,
    cache: &Mutex<StatusCache>,
    emit: &mut dyn FnMut(AppEvent),
    last_recovery: &mut Option<String>,
    run: impl FnOnce(&AudioConfig) -> Result<(Option<PathBuf>, fxsound::ApplyReport), String>,
    current_status: impl Fn() -> FxSoundStatus,
    conflict: impl FnOnce() -> Option<(String, Vec<DiagnosticAction>)>,
) {
    // A queued slider apply would race the repair and overwrite the managed
    // preset immediately after it is recreated.
    *pending = None;
    set_busy(cache, Some("Repairing sound preset…".into()));
    emit(AppEvent::FxSound(snapshot(cache)));
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run(config)))
        .unwrap_or_else(|_| Err("Sound preset repair panicked.".into()));
    set_busy(cache, None);
    match result {
        Ok((backup, report)) => {
            store_status(cache, report.status.clone());
            let message = match backup {
                Some(path) => format!(
                    "Backed up your Crosshair+ preset to {} and recreated the managed one.",
                    path.display()
                ),
                None => "Recreated the managed Crosshair+ preset.".into(),
            };
            emit(AppEvent::AudioNotice(message));
            if let Some((code, message, actions)) = apply_recovery(&report) {
                if last_recovery.as_deref() != Some(message.as_str()) {
                    *last_recovery = Some(message.clone());
                    emit(AppEvent::AudioRecovery { code, message, actions });
                }
            } else {
                *last_recovery = None;
            }
        }
        Err(err) => {
            crate::logging::log(&format!("fxsound: preset repair failed: {err}"));
            let mut status = current_status();
            status.message = "FxSound could not repair the managed preset.".into();
            store_status(cache, status);
            if let Some((message, actions)) = conflict() {
                if last_recovery.as_deref() != Some(message.as_str()) {
                    *last_recovery = Some(message.clone());
                    emit(AppEvent::AudioRecovery {
                        code: "audio_preset_conflict".into(),
                        message,
                        actions,
                    });
                }
            }
        }
    }
    emit(AppEvent::FxSound(snapshot(cache)));
}

fn audio_worker(
    rx: mpsc::Receiver<AudioCmd>,
    proxy: EventLoopProxy<AppEvent>,
    device: Arc<Mutex<Option<String>>>,
    cache: Arc<Mutex<StatusCache>>,
) {
    let mut pending: Option<(AudioConfig, bool)> = None;
    let mut last_cli_refresh: Option<Instant> = None;
    let mut last_recovery: Option<String> = None;
    loop {
        let command = if pending.is_some() {
            match rx.recv_timeout(Duration::from_millis(220)) {
                Ok(cmd) => Some(cmd),
                Err(mpsc::RecvTimeoutError::Timeout) => None,
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        } else {
            match rx.recv() {
                Ok(cmd) => Some(cmd),
                Err(_) => break,
            }
        };
        if let Some(cmd) = command {
            match cmd {
                AudioCmd::Apply(config, quiet) => pending = Some((config, quiet)),
                AudioCmd::Setup => {
                    set_busy(&cache, Some("Installing FxSound…".into()));
                    publish_status(&proxy, &cache);
                    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| fxsound::setup(&proxy))).unwrap_or_else(|_| Err("FxSound setup panicked.".into()));
                    set_busy(&cache, None);
                    match result {
                        Ok(()) => {
                            let status = fxsound::status();
                            store_status(&cache, status);
                            let _ = proxy.send_event(AppEvent::AudioNotice("FxSound installed.".into()));
                        }
                        Err(err) => {
                            crate::logging::log(&format!("fxsound: setup failed: {err}"));
                            let mut status = fxsound::status();
                            status.message = "Install failed.".into();
                            store_status(&cache, status);
                            let _ = proxy.send_event(AppEvent::AudioNotice("Install failed.".into()));
                        }
                    }
                    publish_status(&proxy, &cache);
                }
                AudioCmd::Open => {
                    set_busy(&cache, Some("Opening FxSound…".into()));
                    publish_status(&proxy, &cache);
                    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(crate::audio::fxsound::open_fxsound));
                    set_busy(&cache, None);
                    match result {
                        Ok(Ok(())) => {}
                        Ok(Err(err)) => {
                            crate::logging::log(&format!("fxsound: open failed: {err}"));
                            let _ = proxy.send_event(AppEvent::AudioNotice(format!("Could not open FxSound: {err}")));
                        }
                        Err(_) => {
                            crate::logging::log("fxsound: open panicked");
                            let _ = proxy.send_event(AppEvent::AudioNotice("Could not open FxSound.".into()));
                        }
                    }
                    let status = fxsound::status();
                    store_status(&cache, status);
                    publish_status(&proxy, &cache);
                }
                AudioCmd::Refresh { force } => {
                    // The periodic UI poll is cheap and only re-reads the file;
                    // a tab entry (force) and a 30-second cadence ask FxSound
                    // itself to rewrite status.json so stale data cannot stick.
                    let cli_due = force
                        || last_cli_refresh
                            .map(|at| at.elapsed() >= Duration::from_secs(30))
                            .unwrap_or(true);
                    let status = if cli_due {
                        last_cli_refresh = Some(Instant::now());
                        fxsound::refresh_status()
                    } else {
                        fxsound::status()
                    };
                    store_status(&cache, status);
                    publish_status(&proxy, &cache);
                }
                AudioCmd::Unstick(config) => {
                    run_unstick(&config, &mut pending, &cache, &mut |event| {
                        let _ = proxy.send_event(event);
                    }, fxsound::unstick, fxsound::status);
                }
                AudioCmd::RepairPreset(config) => {
                    run_repair_preset(
                        &config,
                        &mut pending,
                        &cache,
                        &mut |event| {
                            let _ = proxy.send_event(event);
                        },
                        &mut last_recovery,
                        |config| {
                            fxsound::repair_managed_preset(config)
                                .and_then(|repair| fxsound::apply_report(config).map(|report| (repair, report)))
                        },
                        fxsound::status,
                        preset_conflict_recovery,
                    );
                }
                AudioCmd::Repair => {
                    set_busy(&cache, Some("Reinstalling FxSound…".into()));
                    publish_status(&proxy, &cache);
                    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        fxsound::repair_with(|text| {
                            set_busy(&cache, Some(text.to_string()));
                            publish_status(&proxy, &cache);
                        })
                    }))
                    .unwrap_or_else(|_| Err("FxSound reinstall panicked.".into()));
                    set_busy(&cache, None);
                    match result {
                        Ok(_) => {
                            let status = fxsound::status();
                            store_status(&cache, status);
                            let _ = proxy.send_event(AppEvent::AudioNotice("FxSound reinstalled.".into()));
                        }
                        Err(err) => {
                            crate::logging::log(&format!("fxsound: reinstall failed: {err}"));
                            let mut status = fxsound::status();
                            status.message = "Reinstall failed.".into();
                            store_status(&cache, status);
                            let _ = proxy.send_event(AppEvent::AudioNotice("Reinstall failed.".into()));
                        }
                    }
                    publish_status(&proxy, &cache);
                }
                AudioCmd::Shutdown => break,
            }
        }
        if let Some((config, quiet)) = pending.clone() {
            if rx.try_recv().is_err() {
                pending = None;
                if config.device.is_some() {
                    *device.lock().unwrap_or_else(|e| e.into_inner()) = config.device.clone();
                }
                let started = Instant::now();
                set_busy(&cache, Some("Applying FxSound…".into()));
                publish_status(&proxy, &cache);
                crate::logging::log("fxsound: apply begin");
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| fxsound::apply_report(&config)))
                    .unwrap_or_else(|_| Err("FxSound apply panicked.".into()));
                crate::logging::log(&format!("fxsound: apply done in {}ms", started.elapsed().as_millis()));
                set_busy(&cache, None);
                match result {
                    Ok(report) => {
                        store_status(&cache, report.status.clone());
                        publish_status(&proxy, &cache);
                        if let Some((code, message, actions)) = apply_recovery(&report) {
                            if quiet {
                                crate::logging::log(&format!("fxsound: startup reconciliation note: {message}"));
                            } else if last_recovery.as_deref() != Some(message.as_str()) {
                                last_recovery = Some(message.clone());
                                let _ = proxy.send_event(AppEvent::AudioRecovery { code, message, actions });
                            }
                        } else {
                            last_recovery = None;
                        }
                    }
                    Err(err) => {
                        crate::logging::log(&format!("fxsound: apply failed: {err}"));
                        if quiet {
                            let status = fxsound::status();
                            store_status(&cache, status);
                            publish_status(&proxy, &cache);
                            continue;
                        }
                        let mut status = fxsound::status();
                        status.message = "FxSound could not apply the new settings.".into();
                        store_status(&cache, status);
                        publish_status(&proxy, &cache);
                        if let Some((message, actions)) = preset_conflict_recovery() {
                            if last_recovery.as_deref() != Some(message.as_str()) {
                                last_recovery = Some(message.clone());
                                let _ = proxy.send_event(AppEvent::AudioRecovery {
                                    code: "audio_preset_conflict".into(),
                                    message,
                                    actions,
                                });
                            }
                        }
                    }
                }
            }
        }
    }
}

fn meter_worker(proxy: EventLoopProxy<AppEvent>, enabled: Arc<AtomicBool>, device: Arc<Mutex<Option<String>>>) {
    let mut meter = wasapi_meter::PeakMeter::new();
    let mut last_refresh = Instant::now();
    let mut announced = false;
    loop {
        thread::sleep(Duration::from_millis(66));
        if !enabled.load(Ordering::Relaxed) {
            continue;
        }
        let name = device.lock().unwrap_or_else(|e| e.into_inner()).clone();
        if last_refresh.elapsed() > Duration::from_secs(5) || meter.needs_refresh() {
            meter.refresh(name.as_deref());
            last_refresh = Instant::now();
        }
        if let Some(peak) = meter.peak() {
            if !announced {
                announced = true;
                crate::logging::log(&format!("wasapi: loopback meter active peak={peak:.4}"));
            }
            let _ = proxy.send_event(AppEvent::SoundMeter { peak });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn within<T: Send + 'static>(label: &str, work: impl FnOnce() -> T + Send + 'static) -> T {
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let _ = tx.send(work());
        });
        rx.recv_timeout(Duration::from_secs(5))
            .unwrap_or_else(|_| panic!("{label} blocked on a locked status cache"))
    }

    #[test]
    fn updater_publish_does_not_deadlock_status_reads() {
        let cache = Arc::new(Mutex::new(StatusCache::default()));
        let publisher = cache.clone();
        let published = within("update publish", move || publish_latest(&publisher, "1.2.99").is_some());
        assert!(published, "the first published version must produce a status");
        let reader = cache.clone();
        let status = within("status read", move || snapshot(&reader));
        assert_eq!(status.latest_version.as_deref(), Some("1.2.99"));
        assert!(publish_latest(&cache, "1.2.99").is_none(), "an unchanged version must not republish");
    }

    #[test]
    fn status_snapshot_is_stable_while_worker_publishes() {
        let cache = Arc::new(Mutex::new(StatusCache::default()));
        let writer = cache.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let writer_stop = stop.clone();
        let handle = thread::spawn(move || {
            let mut index = 0u32;
            while !writer_stop.load(Ordering::Relaxed) {
                let _ = publish_latest(&writer, &format!("1.0.{index}"));
                index = index.wrapping_add(1);
            }
        });
        for _ in 0..250 {
            let status = snapshot(&cache);
            assert!(!status.busy, "no worker task should be reported as busy");
        }
        stop.store(true, Ordering::Relaxed);
        handle.join().expect("status writer panicked");
    }

    #[test]
    fn unstick_discards_pending_apply_and_reports_success() {
        let cache = Mutex::new(StatusCache::default());
        let mut pending = Some((AudioConfig { enabled: true, ..Default::default() }, false));
        let mut events = Vec::new();
        run_unstick(
            &AudioConfig::default(),
            &mut pending,
            &cache,
            &mut |event| events.push(event),
            |_| Ok(FxSoundStatus { running: true, power: true, output: Some("Stub Headphones".into()), ..Default::default() }),
            FxSoundStatus::default,
        );
        assert!(pending.is_none(), "the queued slider apply must be discarded");
        let statuses: Vec<&FxSoundStatus> = events
            .iter()
            .filter_map(|event| match event {
                AppEvent::FxSound(status) => Some(status),
                _ => None,
            })
            .collect();
        assert_eq!(statuses.len(), 2);
        assert!(statuses[0].busy, "the worker must publish a busy state");
        assert_eq!(statuses[0].task.as_deref(), Some("Restarting FxSound…"));
        assert!(!statuses[1].busy, "the busy flag must be cleared");
        assert!(events.iter().any(|event| matches!(
            event,
            AppEvent::AudioNotice(message) if message.contains("Stub Headphones")
        )));
    }

    #[test]
    fn unstick_failure_clears_busy_and_reports_failure() {
        let cache = Mutex::new(StatusCache::default());
        let mut pending = Some((AudioConfig::default(), false));
        let mut events = Vec::new();
        run_unstick(
            &AudioConfig::default(),
            &mut pending,
            &cache,
            &mut |event| events.push(event),
            |_| Err("restart failed".into()),
            FxSoundStatus::default,
        );
        assert!(pending.is_none());
        assert!(!cache.lock().unwrap().snapshot().busy);
        assert!(events.iter().any(|event| matches!(
            event,
            AppEvent::AudioNotice(message) if message.contains("Could not unstick FxSound")
        )));
    }

    #[test]
    fn repair_preset_success_clears_busy_and_emits_recovery_actions() {
        let cache = Mutex::new(StatusCache::default());
        let mut pending = Some((AudioConfig { enabled: true, ..Default::default() }, false));
        let mut last_recovery = None;
        let mut events = Vec::new();
        run_repair_preset(
            &AudioConfig::default(),
            &mut pending,
            &cache,
            &mut |event| events.push(event),
            &mut last_recovery,
            |_| {
                Ok((
                    Some(PathBuf::from("backup.fac")),
                    fxsound::ApplyReport {
                        status: FxSoundStatus { installed: true, running: true, power: true, ..Default::default() },
                        fallback_device: Some("Missing Headset".into()),
                        active_output: Some("Stub Speakers".into()),
                        mismatches: Vec::new(),
                    },
                ))
            },
            FxSoundStatus::default,
            || None,
        );

        assert!(pending.is_none(), "the queued slider apply must be discarded");
        let statuses: Vec<&FxSoundStatus> = events
            .iter()
            .filter_map(|event| match event {
                AppEvent::FxSound(status) => Some(status),
                _ => None,
            })
            .collect();
        assert_eq!(statuses.len(), 2);
        assert!(statuses[0].busy);
        assert_eq!(statuses[0].task.as_deref(), Some("Repairing sound preset…"));
        assert!(!statuses[1].busy, "the busy flag must be cleared after repair");
        assert!(events.iter().any(|event| matches!(
            event,
            AppEvent::AudioNotice(message) if message.contains("Backed up your Crosshair+ preset")
        )));
        let (code, message, actions) = events
            .iter()
            .find_map(|event| match event {
                AppEvent::AudioRecovery { code, message, actions } => Some((code, message, actions)),
                _ => None,
            })
            .expect("the fallback must publish a recovery notice");
        assert_eq!(code, "audio_output_fallback");
        assert!(message.contains("Missing Headset"));
        assert!(actions.iter().any(|action| action.id == "use-default-output" && action.label == "Use Windows default"));
        assert!(actions.iter().any(|action| action.id == "choose-output" && action.label == "Choose device…"));
        assert_eq!(last_recovery.as_deref(), Some(message.as_str()));
    }

    #[test]
    fn repair_preset_failure_clears_busy_and_emits_conflict_recovery() {
        let cache = Mutex::new(StatusCache::default());
        let mut pending = Some((AudioConfig::default(), false));
        let mut last_recovery = None;
        let mut events = Vec::new();
        run_repair_preset(
            &AudioConfig::default(),
            &mut pending,
            &cache,
            &mut |event| events.push(event),
            &mut last_recovery,
            |_| Err("access denied".into()),
            || FxSoundStatus { installed: true, ..Default::default() },
            || {
                Some((
                    "A user-owned Crosshair+ preset is blocking managed sound.".into(),
                    vec![
                        action("repair-managed-preset", "Back up existing & recreate managed preset"),
                        action("open-fxsound-presets", "Open presets folder"),
                    ],
                ))
            },
        );

        assert!(pending.is_none(), "the queued slider apply must be discarded");
        assert!(!cache.lock().unwrap().snapshot().busy);
        assert_eq!(
            cache.lock().unwrap().status.message,
            "FxSound could not repair the managed preset."
        );
        let (code, message, actions) = events
            .iter()
            .find_map(|event| match event {
                AppEvent::AudioRecovery { code, message, actions } => Some((code, message, actions)),
                _ => None,
            })
            .expect("the conflict must publish a recovery notice");
        assert_eq!(code, "audio_preset_conflict");
        assert!(message.contains("user-owned Crosshair+ preset"));
        assert!(actions.iter().any(|action| action.id == "repair-managed-preset" && action.label == "Back up existing & recreate managed preset"));
        assert!(actions.iter().any(|action| action.id == "open-fxsound-presets" && action.label == "Open presets folder"));
        assert_eq!(last_recovery.as_deref(), Some(message.as_str()));
    }

    #[test]
    fn clean_apply_produces_no_recovery_banner() {
        let report = fxsound::ApplyReport {
            status: FxSoundStatus { installed: true, running: true, power: true, ..Default::default() },
            fallback_device: None,
            active_output: Some("Stub Speakers".into()),
            mismatches: Vec::new(),
        };
        assert!(apply_recovery(&report).is_none(), "a clean apply must not notify");
    }

    #[test]
    fn ignored_setting_produces_one_nonblocking_recovery_notice() {
        let report = fxsound::ApplyReport {
            status: FxSoundStatus { installed: true, running: true, power: true, ..Default::default() },
            fallback_device: None,
            active_output: Some("Stub Speakers".into()),
            mismatches: vec![
                "band count (expected 20, got 10)".into(),
                "bass effect".into(),
            ],
        };
        let (code, message, actions) = apply_recovery(&report).expect("one recovery notice");
        assert_eq!(code, "audio_mismatch");
        assert!(message.contains("FxSound ignored: band count"));
        assert!(message.contains("bass effect"));
        assert_eq!(actions.len(), 1, "one user action, no automatic retry loop");
        assert_eq!(actions[0].id, "unstick-audio");
        assert_eq!(actions[0].label, "Restart FxSound");
    }

    #[test]
    fn user_owned_crosshair_preset_offers_backup_and_recreate_actions() {
        let path = PathBuf::from("C:\\Users\\User\\AppData\\Roaming\\FxSound\\Presets\\Crosshair+.fac");
        let (message, actions) = preset_conflict_notice(fxsound::PresetConflict::UserFile(path.clone()));
        assert!(message.contains("user-owned Crosshair+ preset"));
        assert!(message.contains(&path.display().to_string()));
        assert!(actions.iter().any(|action| {
            action.id == "repair-managed-preset"
                && action.label == "Back up existing & recreate managed preset"
        }));
        assert!(actions.iter().any(|action| {
            action.id == "open-fxsound-presets" && action.label == "Open presets folder"
        }));
    }

    fn ten_band_fixture_audio() -> AudioConfig {
        AudioConfig {
            enabled: true,
            device: None,
            bands: [3.5, 4.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0],
            bass: 1.0236220359802251,
            treble: 0.0,
            output_level: -4.0,
            echo: 4.0157480239868164,
            surround: 4.0157480239868164,
            dynamic_boost: 1.968503952026367,
            volume_leveling: 0.0,
            balance: 0.0,
            filter_q: 2.0,
            volume_guard: false,
            ..Default::default()
        }
    }

    fn report_for(audio: &AudioConfig, actual: &serde_json::Value) -> fxsound::ApplyReport {
        fxsound::ApplyReport {
            status: FxSoundStatus { installed: true, running: true, power: true, ..Default::default() },
            fallback_device: None,
            active_output: None,
            mismatches: crate::audio::verify::mismatch_messages(audio, actual),
        }
    }

    #[test]
    fn sound_off_apply_has_no_mismatch_or_banner() {
        let audio = AudioConfig { enabled: false, ..ten_band_fixture_audio() };
        let mut actual: serde_json::Value = serde_json::from_str(include_str!("../testdata/fxsound-status-sound-off.json"))
            .expect("Sound-off fixture must parse");
        actual["power"] = serde_json::json!(false);

        let report = report_for(&audio, &actual);
        assert!(report.mismatches.is_empty(), "Sound-off is a legitimate state: {:?}", report.mismatches);
        assert!(apply_recovery(&report).is_none(), "Sound-off must not show a recovery banner");
    }

    #[test]
    fn missing_output_after_apply_has_no_mismatch_or_banner() {
        let audio = ten_band_fixture_audio();
        let mut actual: serde_json::Value = serde_json::from_str(include_str!("../testdata/fxsound-status.json"))
            .expect("10-band fixture must parse");
        actual.as_object_mut().expect("fixture is an object").remove("selected_output");

        let report = report_for(&audio, &actual);
        assert!(report.mismatches.is_empty(), "a missing output section is legitimate: {:?}", report.mismatches);
        assert!(apply_recovery(&report).is_none(), "a missing output section must not show a recovery banner");
    }

    #[test]
    fn requested_sound_on_with_fxsound_off_is_a_power_mismatch_and_one_banner() {
        let audio = ten_band_fixture_audio();
        let mut actual: serde_json::Value = serde_json::from_str(include_str!("../testdata/fxsound-status.json"))
            .expect("10-band fixture must parse");
        actual["power"] = serde_json::json!(false);
        actual.as_object_mut().expect("fixture is an object").remove("selected_output");

        let report = report_for(&audio, &actual);
        assert_eq!(report.mismatches, vec!["sound on/off".to_string()]);
        let (code, message, actions) = apply_recovery(&report).expect("power mismatch needs one banner");
        assert_eq!(code, "audio_mismatch");
        assert!(message.contains("FxSound ignored: sound on/off."), "{message}");
        assert_eq!(actions.len(), 1, "one user action, not an automatic retry");
        assert_eq!(actions[0].id, "unstick-audio");
    }
}
