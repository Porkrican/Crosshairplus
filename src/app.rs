use std::borrow::Cow;
use std::ffi::c_void;
use std::num::NonZeroIsize;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, TryRecvError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tao::event::{Event, StartCause, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoopBuilder, EventLoopProxy, EventLoopWindowTarget};
use tao::dpi::{PhysicalPosition, PhysicalSize, Size};
use tao::window::{Icon, Window, WindowBuilder};
use tao::platform::windows::{WindowBuilderExtWindows, WindowExtWindows};
use tray_icon::{TrayIcon, TrayIconEvent};
use raw_window_handle::{
    DisplayHandle, HandleError, HasDisplayHandle, HasWindowHandle, RawDisplayHandle, RawWindowHandle,
    Win32WindowHandle, WindowHandle, WindowsDisplayHandle,
};
use windows::core::PCWSTR;
use windows::Win32::Foundation::{HWND, POINT, ERROR_SUCCESS};
use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_APARTMENTTHREADED};
use windows::Win32::Graphics::Dwm::{DwmSetWindowAttribute, DWMWA_CAPTION_COLOR, DWMWA_USE_IMMERSIVE_DARK_MODE};
use windows::Win32::Graphics::Gdi::{GetMonitorInfoW, MonitorFromPoint, MONITORINFO, MONITOR_DEFAULTTONEAREST};
use windows::Win32::System::Registry::{
    RegDeleteKeyValueW, RegGetValueW, RegSetKeyValueW, HKEY, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE,
    RRF_RT_REG_BINARY, RRF_RT_REG_SZ,
};
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::{
    GetForegroundWindow, MessageBoxW, SetForegroundWindow, MB_ICONERROR, MB_OK, SW_SHOWNORMAL,
};
use wry::{DragDropEvent, WebContext, WebView, WebViewBuilder, WebViewBuilderExtWindows};

use crate::audio::{dsp_math, fxsound, AudioDevice, AudioRuntime, FxSoundStatus};
use crate::actions;
use crate::backup;
use crate::config::{self, Config, Settings, Slot, MAX_SLOT_NAME};
use crate::display::runtime::DisplayRuntime;
use crate::display::DisplayEngine;
use crate::hotkeys;
use crate::games;
use crate::ipc::{self, Command, DiagnosticAction, Event as IpcEvent, FullStatePayload};
use crate::monitors::{self, MonitorInfo};
use crate::overlay::{Input as OverlayInput, OverlayCostSnapshot, OverlayEvent, OverlayHandle};
use crate::render::preview::{PreviewCache, PreviewCost, PreviewWorker};
use crate::render::scene;
use crate::safety;
use crate::slots;

pub enum AppEvent {
    Command(Command),
    Overlay(OverlayEvent),
    FxSound(FxSoundStatus),
    AudioNotice(String),
    AudioRecovery { code: String, message: String, actions: Vec<DiagnosticAction> },
    SoundMeter { peak: f32 },
    DropFile(PathBuf),
    Ready,
    /// A duplicate launch asked the running copy to surface its main window.
    ShowMain,
    /// Full monitor probe (DDC/WMI) finished on a worker thread.
    MonitorsRefreshed(Vec<MonitorInfo>),
    /// Render endpoint enumeration finished on a worker thread.
    AudioDevices(Vec<AudioDevice>),
    VerifyReport(String),
    VerifyOverlay(String),
    FxVerifyReport(String),
    StressReport(String),
    PreviewReady { tick: u64, next_ms: Option<u64>, render_ms: u64 },
    RunningApps { apps: Vec<slots::RunningApp>, elapsed_ms: u128 },
    InstalledGames {
        generation: u64,
        explicit: bool,
        /// The cached list after dropping entries that no longer exist; only
        /// filled when the scan itself returned nothing.
        cached_alive: Vec<games::InstalledGame>,
        games: Vec<games::InstalledGame>,
        elapsed_ms: u128,
    },
    DisplayOutcome(crate::display::ApplyOutcome),
    /// Result of a picker that was opened on its own thread, so the event loop
    /// never blocks behind the native file dialog.
    FilePicked { kind: PickKind, slot_id: Option<String>, path: Option<PathBuf> },
}

/// What a native picker is for. Keeping this typed means an unexpected result
/// cannot silently fall through into slot assignment.
#[derive(Clone, Copy)]
pub(crate) enum PickKind {
    Image,
    Eq,
    SlotExe,
    BackupOpen,
    BackupSave,
}

impl PickKind {
    fn label(self) -> &'static str {
        match self {
            Self::Image => "image",
            Self::Eq => "eq",
            Self::SlotExe => "exe",
            Self::BackupOpen => "backup-open",
            Self::BackupSave => "backup-save",
        }
    }

    fn saves_file(self) -> bool {
        matches!(self, Self::BackupSave)
    }
}

/// Raw handles for rfd's dialog owner, stored as integers so the picker thread
/// can own the dialog without borrowing the window.
struct ParentWindow(isize);

impl HasWindowHandle for ParentWindow {
    fn window_handle(&self) -> Result<WindowHandle<'_>, HandleError> {
        let hwnd = NonZeroIsize::new(self.0).ok_or(HandleError::Unavailable)?;
        Ok(unsafe { WindowHandle::borrow_raw(RawWindowHandle::Win32(Win32WindowHandle::new(hwnd))) })
    }
}

impl HasDisplayHandle for ParentWindow {
    fn display_handle(&self) -> Result<DisplayHandle<'_>, HandleError> {
        Ok(unsafe { DisplayHandle::borrow_raw(RawDisplayHandle::Windows(WindowsDisplayHandle::new())) })
    }
}

/// Detects UI-thread stalls without changing the hot path. The event loop
/// beats once per pump; if a command blocks, the stage string identifies where.
pub struct UiWatchdog {
    beat: Arc<AtomicU64>,
    stage: Arc<Mutex<String>>,
    stop: Arc<AtomicBool>,
}

impl UiWatchdog {
    fn start() -> Self {
        let beat = Arc::new(AtomicU64::new(0));
        let stage = Arc::new(Mutex::new("startup".to_string()));
        let stop = Arc::new(AtomicBool::new(false));
        {
            let beat = beat.clone();
            let stage = stage.clone();
            let stop = stop.clone();
            std::thread::spawn(move || {
                let mut last = 0u64;
                let mut last_change = Instant::now();
                while !stop.load(Ordering::Relaxed) {
                    std::thread::sleep(Duration::from_millis(500));
                    let current = beat.load(Ordering::Relaxed);
                    if current == last {
                        if last_change.elapsed() >= Duration::from_millis(1500) {
                            let last_stage = stage.lock().map(|s| s.clone()).unwrap_or_default();
                            crate::logging::log(&format!("ui: stalled {}ms at {last_stage}", last_change.elapsed().as_millis()));
                            last_change = Instant::now();
                        }
                    } else {
                        if last != 0 && last_change.elapsed() >= Duration::from_millis(1500) {
                            crate::logging::log(&format!("ui: resumed after {}ms", last_change.elapsed().as_millis()));
                        }
                        last = current;
                        last_change = Instant::now();
                    }
                }
            });
        }
        Self { beat, stage, stop }
    }
    fn beat(&self) {
        self.beat.fetch_add(1, Ordering::Relaxed);
    }
    fn stage(&self, stage: &str) {
        if let Ok(mut slot) = self.stage.lock() {
            *slot = stage.to_string();
        }
    }
}

impl Drop for UiWatchdog {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

pub fn run() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|arg| arg == "--repair-fxsound") {
        // Explicit, destructive developer probe: real uninstall and reinstall,
        // no window, tray or audio runtime. Writes a JSON report next to the log.
        let result = crate::audio::fxsound::repair_probe();
        let path = crate::config::data_dir().join("fx-repair-result.json");
        if let Err(err) = std::fs::write(&path, serde_json::to_string_pretty(&result).unwrap_or_default()) {
            crate::logging::log(&format!("fxrepair: cannot write {}: {err}", path.display()));
        }
        crate::logging::log(&format!("fxrepair: {result}"));
        let ok = result.get("ok").and_then(|value| value.as_bool()).unwrap_or(false);
        std::process::exit(if ok { 0 } else { 1 });
    }
    let capture_mode = args.iter().any(|arg| arg == "--capture");
    let verify_mode = args.iter().any(|arg| arg == "--verify-preset-delete");
    let fx_verify_mode = args.iter().any(|arg| arg == "--verify-fxsound");
    // Live mode runs the real network update check before driving the Sound tab;
    // the default sandboxed verifier uses the deterministic stub instead.
    let fx_verify_live = fx_verify_mode && args.iter().any(|arg| arg == "--live-updates");
    let interaction_mode = args.iter().any(|arg| arg == "--interaction-test");
    let stress_mode = args.iter().any(|arg| arg == "--stress");
    if args.iter().any(|arg| arg == "--stress-idle") {
        // Diagnostic modifier: keep the app idle with an animated preview so the
        // harness can tell per-tick handle growth apart from per-message work.
        std::env::set_var("CROSSHAIRPLUS_STRESS_IDLE", "1");
    }
    if capture_mode && (verify_mode || fx_verify_mode || interaction_mode || stress_mode) {
        return Err("--capture cannot be combined with verification modes".into());
    }
    if [verify_mode, fx_verify_mode, interaction_mode, stress_mode].into_iter().filter(|mode| *mode).count() > 1 {
        return Err("verification modes are mutually exclusive".into());
    }
    if (interaction_mode || fx_verify_mode || stress_mode) && crate::config::sandbox_root().is_none() {
        return Err("--interaction-test, --stress and --verify-fxsound require CROSSHAIRPLUS_SANDBOX_DIR".into());
    }
    // A second copy would add an overlay, a tray icon and a config writer on
    // top of the first, so a plain launch hands the request to the running
    // window instead. Headless verification runs stay independent.
    let _instance = if capture_mode || verify_mode || fx_verify_mode || interaction_mode || stress_mode {
        None
    } else {
        match crate::instance::acquire() {
            Some(guard) => Some(guard),
            None => {
                if args.iter().any(|arg| arg == "--minimized") {
                    crate::logging::log("instance: another copy is running; staying in the background");
                } else {
                    let focused = crate::instance::focus_existing();
                    crate::logging::log(&format!("instance: another copy is running; focused={focused}"));
                }
                return Ok(());
            }
        }
    };
    // Keep WebView2 in physical-pixel space. On a scaled display the default
    // controller state fails to initialize (0x8007139F), and the OLED layout is
    // specified in exact 1200x780 pixels anyway.
    std::env::set_var("WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS", "--force-device-scale-factor=1 --high-dpi-support=1");
    let mut builder = EventLoopBuilder::<AppEvent>::with_user_event();
    let event_loop = builder.build();
    let proxy = event_loop.create_proxy();
    if let Some(handle) = _instance.as_ref().and_then(|guard| guard.show_event()) {
        // A duplicate launch signals this event instead of searching for a
        // window title, so a flyout with a similar title cannot be picked.
        let proxy = proxy.clone();
        std::thread::spawn(move || loop {
            use windows::Win32::Foundation::HANDLE;
            use windows::Win32::System::Threading::{WaitForSingleObject, INFINITE};
            let wait = unsafe { WaitForSingleObject(HANDLE(handle as *mut c_void), INFINITE) };
            if wait.0 != 0 {
                break;
            }
            let _ = proxy.send_event(AppEvent::ShowMain);
        });
    }
    let mut app = App::new(proxy, capture_mode, verify_mode, fx_verify_mode, interaction_mode, stress_mode, fx_verify_live)?;
    event_loop.run(move |event, target, control_flow| {
        app.handle(event, target, control_flow);
    });
}

/// The one games scan that may be in flight. Its thread cannot be interrupted,
/// so after a timeout the picker is released while the thread keeps running.
/// Each scan gets a generation so a late result can be told apart from the
/// result of the scan that replaced it.
#[derive(Default)]
struct GameScan {
    generation: u64,
    started: Option<Instant>,
    scanned: bool,
}

impl GameScan {
    /// Starts a scan and returns its generation. `None` means a scan is already
    /// being watched, so the request is a no-op.
    fn begin(&mut self) -> Option<u64> {
        if self.started.is_some() {
            return None;
        }
        self.generation += 1;
        self.started = Some(Instant::now());
        self.scanned = true;
        Some(self.generation)
    }

    /// Releases the picker once the watched scan has run past `grace`. The
    /// thread keeps running; its late result is still accepted if nothing
    /// replaced it.
    fn time_out(&mut self, grace: Duration) -> bool {
        match self.started {
            Some(started) if started.elapsed() >= grace => {
                self.started = None;
                true
            }
            _ => false,
        }
    }

    /// Claims a result and ends the watchdog of the scan it belongs to. A stale
    /// generation is refused, so a released scan cannot end its replacement.
    fn finish(&mut self, generation: u64) -> bool {
        if generation != self.generation {
            return false;
        }
        self.started = None;
        true
    }
}

struct App {
    proxy: EventLoopProxy<AppEvent>,
    window: Option<Window>,
    webview: Option<WebView>,
    _web_context: Option<WebContext>,
    tray: Option<TrayIcon>,
    tray_window: Option<Window>,
    tray_webview: Option<WebView>,
    tray_anchor: Option<TrayAnchor>,
    /// Set once the flyout has actually owned the system foreground. Until
    /// then a focus change is the opening handshake, not a click-away.
    tray_owns_foreground: bool,
    tray_height: f64,
    config: Config,
    /// Problems the config file had to be cleaned of on load, shown once the UI
    /// is ready so a vanished curve is explained instead of only logged.
    startup_warnings: Vec<String>,
    monitors: Vec<MonitorInfo>,
    monitors_refresh_due: Option<Instant>,
    monitors_refreshing: bool,
    display: DisplayRuntime,
    audio: AudioRuntime,
    overlay: OverlayHandle,
    overlay_cost: OverlayCostSnapshot,
    overlay_cost_at: Instant,
    process_cpu: u64,
    backlight: crate::display::ddc::BacklightPacer,
    fx: FxSoundStatus,
    audio_devices: Vec<AudioDevice>,
    running_apps: Vec<slots::RunningApp>,
    installed_games: Vec<games::InstalledGame>,
    games_scan: GameScan,
    last_apps_refresh: Instant,
    apps_refreshing: bool,
    preview: PreviewWorker,
    preview_cache: Arc<Mutex<PreviewCache>>,
    preview_tick: u64,
    /// When the next animated frame is due. None means nothing is scheduled,
    /// which is also the state of a still image.
    preview_due: Option<Instant>,
    preview_cost: PreviewCost,
    preview_cost_at: Instant,
    tab: String,
    selected_slot: Option<String>,
    ui_ready: bool,
    started: bool,
    dirty: bool,
    last_save: Instant,
    last_watch: Instant,
    last_fx_refresh: Instant,
    /// Set when launch skipped the audio catch-up because FxSound was not
    /// running yet. The next ready status applies the saved sound settings
    /// once, quietly.
    pending_audio_reconcile: bool,
    last_overlay_log: Option<(bool, String, usize)>,
    /// A manual Cycle must not be undone by the one-second foreground watcher
    /// until the user actually switches to another application.
    auto_suppressed_for: Option<String>,
    /// Consecutive watcher ticks with the active slot's target gone. Two ticks
    /// keep a splash-screen relaunch from reverting the slot.
    slot_absence: slots::SlotAbsence,
    /// True between CompareDisplay(true) and CompareDisplay(false). Suspend and
    /// shutdown clear it so a lost pointer can never leave identity ramps on.
    compare_active: bool,
    /// True between CompareSound(true) and CompareSound(false). Suspend and
    /// shutdown put the saved curve back so a lost pointer never leaves flat EQ.
    audio_compare_active: bool,
    /// Per-monitor HDR/ACM block state, so a return to SDR re-applies the LUT.
    display_blocks: crate::display::recovery::BlockedDisplays,
    stress_toasts: u64,
    stress_toast_at: Instant,
    watchdog: UiWatchdog,
    exit_requested: bool,
    exit_code: i32,
    exited: bool,
    capture_mode: bool,
    verify_mode: bool,
    fx_verify_mode: bool,
    interaction_mode: bool,
    stress_mode: bool,
    stress_started: bool,
    stress_deadline: Option<Instant>,
    stress_baseline: Option<crate::qa::Metrics>,
    stress_done: bool,
    last_image_check: Option<Instant>,
    warned_missing_image: Option<String>,
    verify_started: bool,
    verify_deadline: Option<Instant>,
    verify_done: bool,
    verify_image_seeded: bool,
    verify_image_armed: Arc<AtomicBool>,
    verify_image_poll_at: Option<Instant>,
    verify_image_deleted: bool,
    verify_image_steps: Vec<&'static str>,
    verify_overlay_last_requests: u64,
    verify_overlay_checks: Vec<serde_json::Value>,
    fx_verify_live: bool,
    fx_verify_started: bool,
    fx_verify_deadline: Option<Instant>,
    fx_verify_done: bool,
    fx_verify_report: Option<serde_json::Value>,
    /// The verifier compares FxSound's live status with the real user config,
    /// not the sandbox copy the rest of this run loaded.
    fx_verify_expected: Option<crate::config::AudioConfig>,
    fx_verify_expected_path: Option<PathBuf>,
    fx_verify_expected_warnings: Vec<String>,
    fx_verify_next: Instant,
    selftest_done: bool,
    capture_stage: usize,
    capture_next: Option<Instant>,
    pending_capture: Option<(String, PathBuf, Receiver<Result<Vec<u8>, String>>)>,
    selftest_complete: Arc<AtomicBool>,
    selftest_started_at: Option<Instant>,
    layout_resized: bool,
    layout_probe_done: bool,
    recent_probe_logged: bool,
    picker_open: bool,
    backup_drop_armed: Arc<AtomicBool>,
    pending_backup: Option<PathBuf>,
}

impl App {
    fn new(proxy: EventLoopProxy<AppEvent>, capture_mode: bool, verify_mode: bool, fx_verify_mode: bool, interaction_mode: bool, stress_mode: bool, fx_verify_live: bool) -> Result<Self, Box<dyn std::error::Error>> {
        config::ensure_dirs();
        let modes = RunModes::new(capture_mode, verify_mode, fx_verify_mode, interaction_mode, stress_mode);
        // Capture and harness modes must not touch the real user's config or
        // data directory; only a normal launch performs the one-time backup.
        if !modes.automated() {
            backup_config();
        }
        let (mut config, mut startup_warnings) = if capture_mode {
            (capture_config(), Vec::new())
        } else {
            Config::load_reporting()
        };
        // A loaded config arrives normalized, with its cleanup already reported;
        // only the synthetic capture config still needs the pass.
        if capture_mode {
            config.normalize();
        }
        // The preset verifier also drives one real failed image selection. Seed
        // a live file and persist that state now; the page arms before the
        // harness deletes it, so the verifier can prove the failure cleanup
        // persisted over the original image (not a later flush).
        let verify_image_seeded = if verify_mode {
            let live = config::crosshairs_dir().join("live-probe.png");
            let source = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("assets/icon.png");
            // The page uses this file for the successful use/remove/active-delete
            // paths; it is stored but not live at startup.
            if let Err(err) = std::fs::copy(&source, config::crosshairs_dir().join("use-probe.png")) {
                crate::logging::log(&format!("verify: could not seed the use-probe image: {err}"));
            }
            match std::fs::copy(&source, &live) {
                Ok(_) => {
                    config.crosshair.image = Some(live.to_string_lossy().to_string());
                    config.crosshair.image_name = Some("live-probe.png".into());
                    config.crosshair.gif_playing = false;
                    match config.save() {
                        Ok(()) => true,
                        Err(err) => {
                            crate::logging::log(&format!("verify: could not persist the seeded live image: {err}"));
                            false
                        }
                    }
                }
                Err(err) => {
                    crate::logging::log(&format!("verify: could not seed the live image: {err}"));
                    false
                }
            }
        } else {
            false
        };
        if stress_mode {
            // A real animated asset keeps the preview pump and overlay drawing
            // for the whole run, while the sandbox config keeps it local.
            config.crosshair.enabled = true;
            config.crosshair.gif_playing = true;
            config.crosshair.image = Some(
                PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                    .join("assets/test.webp")
                    .to_string_lossy()
                    .to_string(),
            );
            config.crosshair.image_name = Some("stress-animated.webp".into());
        }
        let slot_recovered = if !capture_mode && !stress_mode { slots::restore_session_baseline(&mut config) } else { false };
        let safety_recovered = if modes.live_system() { safety::recover_pending() } else { false };
        if slot_recovered {
            crate::logging::log("slots: recovered baseline from an interrupted session and cleared the active slot");
        }
        if safety_recovered {
            crate::logging::log("safety: recovered display/FxSound baseline from an interrupted session");
        }
        if slot_recovered && !capture_mode {
            if let Err(err) = config.save() {
                crate::logging::log(&format!("slots: could not persist crash recovery: {err}"));
            }
        }
        // Startup uses the fast enumeration only: DDC/WMI probes run after the
        // first paint on a worker (see `init_window`).
        let monitors = monitors::quick();
        for monitor in &monitors {
            crate::logging::log(&format!(
                "monitor: id={} gdi={} name={} {}x{}+{},{} dpi={} scale={:.2} backlight={}",
                monitor.id,
                monitor.gdi,
                monitor.name,
                monitor.width,
                monitor.height,
                monitor.x,
                monitor.y,
                monitor.dpi,
                monitor.scale,
                if monitor.backlight_supported { "true" } else { "deferred" }
            ));
        }
        let audio = AudioRuntime::start(proxy.clone());
        let fx = audio.status();
        if interaction_mode || stress_mode || (fx_verify_mode && !fx_verify_live) {
            // Exercise the real update-publish path with a deterministic version
            // and no network. Verification modes used to skip this entirely,
            // which is how a status-cache deadlock once shipped.
            audio.check_updates_with(|| Some("1.2.99".into()), Duration::from_secs(1));
        } else if !verify_mode && !capture_mode {
            audio.check_updates();
        }
        if modes.live_system() {
            safety::capture(&monitors, fx.power, fx.preset.clone());
        }
        let display = DisplayRuntime::start(proxy.clone(), monitors.clone(), !modes.live_system());
        let overlay = OverlayHandle::start(proxy.clone());
        let preview_cache = Arc::new(Mutex::new(PreviewCache::default()));
        let preview = PreviewWorker::start(proxy.clone(), preview_cache.clone());
        // Endpoint enumeration blocks on WASAPI; it starts after the window is
        // visible and arrives as `AppEvent::AudioDevices`.
        let audio_devices: Vec<AudioDevice> = Vec::new();
        let selected_slot = config.slots.active.clone().or_else(|| config.slots.list.first().map(|slot| slot.id.clone()));
        if let Some(device) = config.audio.device.clone() {
            if let Some(found) = audio_devices.iter().find(|d| d.id == device || d.name == device) {
                config.audio.device = Some(found.name.clone());
            }
        }
        if let Some(warning) = apply_run_key_checked(&config.settings, modes) {
            startup_warnings.push(warning);
        }
        if interaction_mode {
            crate::logging::log("interaction test: OS/FxSound/registry side effects disabled");
        }
        // The picker opens from the last scan; the walk runs in the background
        // to correct it. Automated runs never read or write the real cache.
        let cached_games = if modes.automated() {
            Vec::new()
        } else {
            match games::load_cache(&config::games_cache_path()) {
                Ok(cached) => {
                    if !cached.is_empty() {
                        crate::logging::log(&format!("slots: loaded {} cached game(s)", cached.len()));
                    }
                    cached
                }
                Err(err) => {
                    crate::logging::log(&format!("slots: {err}; scanning instead"));
                    Vec::new()
                }
            }
        };
        let tray_height = tray_panel_height(config.slots.list.len());
        let app = Self {
            proxy,
            window: None,
            webview: None,
            _web_context: None,
            tray: None,
            tray_window: None,
            tray_webview: None,
            tray_anchor: None,
            tray_owns_foreground: false,
            tray_height,
            config,
            startup_warnings,
            monitors,
            monitors_refresh_due: None,
            monitors_refreshing: false,
            display,
            audio,
            overlay,
            overlay_cost: OverlayCostSnapshot::default(),
            overlay_cost_at: Instant::now(),
            process_cpu: 0,
            backlight: crate::display::ddc::BacklightPacer::start(),
            fx,
            audio_devices,
            running_apps: Vec::new(),
            installed_games: cached_games,
            games_scan: GameScan::default(),
            last_apps_refresh: Instant::now() - Duration::from_secs(60),
            apps_refreshing: false,
            preview,
            preview_cache,
            preview_tick: 0,
            preview_due: None,
            preview_cost: PreviewCost::default(),
            preview_cost_at: Instant::now(),
            tab: "aim".into(),
            selected_slot,
            ui_ready: false,
            started: false,
            dirty: false,
            last_save: Instant::now(),
            last_watch: Instant::now(),
            last_fx_refresh: Instant::now() - Duration::from_secs(30),
            pending_audio_reconcile: false,
            last_overlay_log: None,
            auto_suppressed_for: None,
            slot_absence: slots::SlotAbsence::default(),
            compare_active: false,
            audio_compare_active: false,
            display_blocks: crate::display::recovery::BlockedDisplays::default(),
            stress_toasts: 0,
            stress_toast_at: Instant::now(),
            watchdog: UiWatchdog::start(),
            exit_requested: false,
            exit_code: 0,
            exited: false,
            capture_mode,
            verify_mode,
            fx_verify_mode,
            interaction_mode,
            stress_mode,
            stress_started: false,
            stress_deadline: None,
            stress_baseline: None,
            stress_done: false,
            last_image_check: None,
            warned_missing_image: None,
            fx_verify_live,
            verify_started: false,
            verify_deadline: None,
            verify_done: false,
            verify_image_seeded,
            verify_image_armed: Arc::new(AtomicBool::new(false)),
            verify_image_poll_at: None,
            verify_image_deleted: false,
            verify_image_steps: Vec::new(),
            verify_overlay_last_requests: 0,
            verify_overlay_checks: Vec::new(),
            fx_verify_started: false,
            fx_verify_deadline: None,
            fx_verify_done: false,
            fx_verify_report: None,
            fx_verify_expected: None,
            fx_verify_expected_path: None,
            fx_verify_expected_warnings: Vec::new(),
            fx_verify_next: Instant::now(),
            selftest_done: false,
            capture_stage: 0,
            capture_next: None,
            pending_capture: None,
            selftest_complete: Arc::new(AtomicBool::new(false)),
            selftest_started_at: None,
            layout_resized: false,
            layout_probe_done: false,
            recent_probe_logged: false,
            picker_open: false,
            backup_drop_armed: Arc::new(AtomicBool::new(false)),
            pending_backup: None,
        };
        if stress_mode {
            crate::logging::log("stress: sandbox harness; tray, hotkeys and OS side effects are disabled");
        }
        Ok(app)
    }

    fn handle(&mut self, event: Event<'_, AppEvent>, target: &EventLoopWindowTarget<AppEvent>, control_flow: &mut ControlFlow) {
        match event {
            Event::NewEvents(StartCause::Init) => {
                if let Err(err) = self.init_window(target) {
                    crate::logging::log(&format!("window init failed: {err}"));
                    self.exit_code = 1;
                    show_startup_error(&err.to_string());
                    self.exit_requested = true;
                }
            }
            Event::WindowEvent { event, window_id, .. } => {
                if self.tray_window.as_ref().map(|w| w.id()) == Some(window_id) {
                    // Focus alone cannot dismiss the flyout: wry moves keyboard
                    // focus from this top-level window into its WebView2 child
                    // on activation, and tao reports that handoff as
                    // Focused(false) the moment the flyout opens. poll_tray
                    // dismisses it on a real foreground change instead.
                    if matches!(event, WindowEvent::CloseRequested) {
                        self.hide_tray_panel();
                    }
                    return;
                }
                if self.window.as_ref().map(|w| w.id()) != Some(window_id) {
                    return;
                }
                match event {
                    WindowEvent::CloseRequested => {
                        if self.config.settings.minimize_to_tray && !self.capture_mode {
                            self.hide_window();
                        } else {
                            self.exit_requested = true;
                            self.shutdown();
                            *control_flow = ControlFlow::ExitWithCode(self.exit_code);
                        }
                    }
                    WindowEvent::Destroyed => {
                        self.exit_requested = true;
                        *control_flow = ControlFlow::ExitWithCode(self.exit_code);
                    }
                    _ => {}
                }
            }
            Event::UserEvent(event) => self.on_app_event(event),
            Event::MainEventsCleared => self.pump(control_flow),
            Event::LoopDestroyed => {
                self.shutdown();
            }
            _ => {}
        }
    }

    fn init_window(&mut self, target: &EventLoopWindowTarget<AppEvent>) -> Result<(), Box<dyn std::error::Error>> {
        if self.window.is_some() {
            return Ok(());
        }
        if let Some(err) = webview2_preflight_error() {
            return Err(err.into());
        }
        let icon = scene::icon_rgba(64)
            .ok()
            .and_then(|(rgba, size)| Icon::from_rgba(rgba, size, size).ok());
        let start_hidden = self.config.settings.start_minimized && !self.capture_mode && !self.verify_mode;
        let initial_size = Size::Physical(PhysicalSize::new(1200, 780));
        let min_size = if self.capture_mode {
            Size::Physical(PhysicalSize::new(800, 600))
        } else {
            Size::Physical(PhysicalSize::new(1040, 680))
        };
        let window = WindowBuilder::new()
            .with_title("Crosshair+")
            .with_inner_size(initial_size)
            .with_min_inner_size(min_size)
            .with_resizable(true)
            .with_window_icon(icon)
            .with_visible(!start_hidden)
            .build(target)?;
        if self.capture_mode {
            window.set_inner_size(PhysicalSize::new(1200, 780));
        } else if let Some(monitor) = window.current_monitor() {
            let size = monitor.size();
            let (width, height) = window_dimensions(size.width, size.height);
            window.set_inner_size(PhysicalSize::new(width, height));
        }
        apply_dwm(&window);
        self.update_preview();
        let cache = self.preview_cache.clone();
        let proxy_ipc = self.proxy.clone();
        let proxy_drop = self.proxy.clone();
        let verify_mode = self.verify_mode;
        let fx_verify_mode = self.fx_verify_mode;
        let stress_mode = self.stress_mode;
        let init_script = if self.capture_mode {
            format!("{}\nwindow.__crosshairCapture = true;", ipc::IPC_BOOTSTRAP)
        } else if self.stress_mode {
            let idle = if std::env::var_os("CROSSHAIRPLUS_STRESS_IDLE").is_some() {
                "\nwindow.__crosshairStressIdle = true;"
            } else {
                ""
            };
            let focus = std::env::var("CROSSHAIRPLUS_STRESS_FOCUS")
                .map(|value| format!("\nwindow.__crosshairStressFocus = {value:?};"))
                .unwrap_or_default();
            format!("{}\nwindow.__crosshairStress = true;{idle}{focus}", ipc::IPC_BOOTSTRAP)
        } else if verify_mode {
            format!("{}\n{}", ipc::IPC_BOOTSTRAP, VERIFY_INIT_JS)
        } else {
            ipc::IPC_BOOTSTRAP.to_string()
        };
        let web_context_path = config::webview_dir();
        crate::logging::log(&format!("shell: webview profile {}", web_context_path.display()));
        let mut web_context = WebContext::new(Some(web_context_path));
        let webview = WebViewBuilder::new_with_web_context(&mut web_context)
            .with_https_scheme(true)
            .with_background_color((0, 0, 0, 255))
            .with_custom_protocol("app".into(), move |_id, request| protocol_response(request.uri().path(), &cache))
            .with_initialization_script(&init_script)
            .with_navigation_handler(|url| {
                let allowed = navigation_allowed(&url);
                if !allowed {
                    crate::logging::log(&format!("navigation blocked: {url}"));
                }
                allowed
            })
            .with_ipc_handler(move |request| {
                let body = request.body();
                if verify_mode && ipc::is_verify_report(body) {
                    let _ = proxy_ipc.send_event(AppEvent::VerifyReport(body.to_string()));
                    return;
                }
                if verify_mode && ipc::is_verify_overlay(body) {
                    let _ = proxy_ipc.send_event(AppEvent::VerifyOverlay(body.to_string()));
                    return;
                }
                if fx_verify_mode && ipc::is_fx_verify_report(body) {
                    let _ = proxy_ipc.send_event(AppEvent::FxVerifyReport(body.to_string()));
                    return;
                }
                if stress_mode && ipc::is_stress_report(body) {
                    let _ = proxy_ipc.send_event(AppEvent::StressReport(body.to_string()));
                    return;
                }
                if let Some(command) = ipc::parse(body) {
                    let _ = proxy_ipc.send_event(AppEvent::Command(command));
                }
            })
            .with_drag_drop_handler(move |event| {
                if let DragDropEvent::Drop { paths, .. } = event {
                    if let Some(path) = paths.into_iter().next() {
                        let _ = proxy_drop.send_event(AppEvent::DropFile(path));
                    }
                }
                false
            })
            .with_devtools(cfg!(debug_assertions))
            .with_url("https://app.localhost/")
            .build(&window)?;
        let proxy_tray = self.proxy.clone();
        // The flyout is a second WebView2 on the same profile, so it needs its
        // own registration of the virtual origin. Without it WebView2 sends the
        // request to the network and shows its "can't reach this page" error.
        let tray_cache = self.preview_cache.clone();
        // WebView2 is forced to device scale 1, so the tray's CSS pixels are
        // physical pixels; size the window in the same units as the page.
        let tray_window = WindowBuilder::new()
            .with_title("Crosshair+ tray")
            .with_inner_size(PhysicalSize::new(
                TRAY_PANEL_WIDTH as u32,
                tray_panel_height(self.config.slots.list.len()) as u32,
            ))
            .with_visible(false)
            .with_decorations(false)
            .with_resizable(false)
            .with_always_on_top(true)
            .with_skip_taskbar(true)
            .build(target)?;
        let tray_webview = WebViewBuilder::new_with_web_context(&mut web_context)
            .with_https_scheme(true)
            .with_background_color((0, 0, 0, 255))
            .with_custom_protocol("app".into(), move |_id, request| protocol_response(request.uri().path(), &tray_cache))
            .with_initialization_script(ipc::IPC_BOOTSTRAP)
            .with_navigation_handler(|url| navigation_allowed(&url))
            .with_ipc_handler(move |request| {
                if let Some(command) = ipc::parse(request.body()) {
                    let _ = proxy_tray.send_event(AppEvent::Command(command));
                }
            })
            .with_devtools(cfg!(debug_assertions))
            .with_url("https://app.localhost/tray.html")
            .build(&tray_window)?;
        let _ = window.set_focus();
        if !self.modes().live_system() {
            crate::logging::log("sandbox: tray skipped; no real shell icon");
        } else {
            match crate::tray::create(&self.config) {
                Ok(handles) => {
                    self.tray = Some(handles.tray);
                    crate::logging::log("tray: icon created");
                }
                Err(err) => crate::logging::log(&format!("tray: creation failed: {err}")),
            }
        }
        self.window = Some(window);
        self.webview = Some(webview);
        self.tray_window = Some(tray_window);
        self.tray_webview = Some(tray_webview);
        self._web_context = Some(web_context);
        self.sync_overlay();
        self.start_startup_probes();
        Ok(())
    }

    /// Runs the expensive probes after the window exists, so neither the first
    /// paint nor the event loop waits on a display driver or WASAPI.
    fn start_startup_probes(&mut self) {
        let proxy = self.proxy.clone();
        let probes = self.backlight.handle();
        std::thread::spawn(move || {
            // WMI (the laptop fallback) needs COM on this thread. DDC reads are
            // requested from the BacklightPacer worker, which owns that thread.
            let com = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) }.is_ok();
            let monitors = monitors::refresh(&probes);
            if com {
                unsafe { CoUninitialize() };
            }
            let _ = proxy.send_event(AppEvent::MonitorsRefreshed(monitors));
        });
        let proxy = self.proxy.clone();
        std::thread::spawn(move || {
            let devices = AudioRuntime::devices();
            let _ = proxy.send_event(AppEvent::AudioDevices(devices));
        });
        if self.modes().live_system() {
            // `--status` can take seconds; the audio worker owns it now.
            self.audio.refresh(true);
        }
    }

    fn pump(&mut self, control_flow: &mut ControlFlow) {
        if self.exit_requested {
            self.shutdown();
            *control_flow = ControlFlow::ExitWithCode(self.exit_code);
            return;
        }
        self.watchdog.beat();
        self.watchdog.stage("pump");
        self.poll_tray();
        self.poll_watchers();
        self.poll_monitor_refresh();
        self.poll_backlight_probes();
        self.poll_preview();
        self.poll_save();
        self.poll_capture();
        self.poll_stress();
        self.poll_image_state();
        self.poll_verify();
        self.poll_fx_verify();
        self.poll_fx_status();
        self.poll_game_scan();
        self.poll_overlay_cost();
        self.poll_preview_cost();
        self.watchdog.stage("idle");
        // An animated preview wakes on its own frame deadline; everything else
        // keeps the cheap 50 ms pump.
        let mut wait = Duration::from_millis(50);
        if self.tab == "aim" && self.preview_animated() {
            if let Some(due) = self.preview_due {
                wait = wait.min(due.saturating_duration_since(Instant::now()));
            }
        }
        *control_flow = ControlFlow::WaitUntil(Instant::now() + wait);
    }

    fn on_app_event(&mut self, event: AppEvent) {
        self.watchdog.beat();
        match event {
            AppEvent::ShowMain => self.show_window(),
            AppEvent::MonitorsRefreshed(monitors) => {
                self.monitors_refreshing = false;
                self.apply_monitor_refresh(monitors);
            }
            AppEvent::AudioDevices(devices) => {
                self.audio_devices = devices;
                crate::logging::log(&format!(
                    "audio: {} render device(s): {}",
                    self.audio_devices.len(),
                    self.audio_devices.iter().map(|device| device.name.as_str()).collect::<Vec<_>>().join(", ")
                ));
                if self.ui_ready {
                    self.emit_full_state();
                }
            }
            AppEvent::Command(command) => {
                let stage = format!("command:{}", command.name());
                self.watchdog.stage(&stage);
                let started = Instant::now();
                self.handle_command(command);
                let elapsed = started.elapsed();
                if elapsed >= Duration::from_millis(250) {
                    crate::logging::log(&format!("ui: command slow {}ms", elapsed.as_millis()));
                }
                self.watchdog.stage("idle");
            }
            AppEvent::FilePicked { kind, slot_id, path } => {
                self.picker_open = false;
                let Some(path) = path else { return };
                match kind {
                    PickKind::Image => self.import_image_path(&path),
                    PickKind::Eq => self.import_eq(Some(path.to_string_lossy().to_string()), None, None),
                    PickKind::SlotExe => self.assign_slot_exe(slot_id.as_deref(), &path),
                    PickKind::BackupOpen => self.inspect_backup_path(&path),
                    PickKind::BackupSave => self.create_backup_path(&path),
                }
            }
            AppEvent::Overlay(event) => {
                self.watchdog.stage("overlay-event");
                self.handle_overlay(event);
                self.watchdog.stage("idle");
            }
            AppEvent::PreviewReady { tick, next_ms, render_ms } => {
                self.preview_tick = tick;
                self.emit_preview_tick();
                self.preview_due = next_ms.map(|ms| Instant::now() + Duration::from_millis(ms));
                if render_ms >= 250 {
                    crate::logging::log(&format!("preview: slow frame render {render_ms}ms"));
                }
            }
            AppEvent::RunningApps { apps, elapsed_ms } => {
                self.running_apps = apps;
                self.apps_refreshing = false;
                self.last_apps_refresh = Instant::now();
                crate::logging::log(&format!("slots: refreshed {} running app(s) in {}ms", self.running_apps.len(), elapsed_ms));
                if self.tab == "slots" {
                    self.emit_full_state();
                }
            }
            AppEvent::InstalledGames { generation, explicit, cached_alive, games, elapsed_ms } => {
                if !self.games_scan.finish(generation) {
                    crate::logging::log(&format!(
                        "slots: ignoring stale game scan result (scan {generation}, current {})",
                        self.games_scan.generation
                    ));
                    return;
                }
                if games.is_empty() {
                    let cached = self.installed_games.len();
                    let kept = cached_alive.len();
                    match empty_scan_outcome(explicit, cached, kept) {
                        EmptyScan::Apply => {}
                        EmptyScan::Keep => {
                            crate::logging::log(&format!(
                                "slots: scan found no game(s); kept {kept} cached game(s), pruned {}",
                                cached - kept
                            ));
                            if kept != cached {
                                if self.modes().live_system() {
                                    games::save_cache(&config::games_cache_path(), &cached_alive);
                                }
                                self.installed_games = cached_alive;
                                self.emit(IpcEvent::InstalledGames { games: self.installed_games.clone(), timed_out: false });
                            }
                            return;
                        }
                        EmptyScan::Clear => {
                            crate::logging::log(&format!(
                                "slots: scan found no game(s); pruned all {cached} cached game(s)"
                            ));
                            self.installed_games.clear();
                            if self.modes().live_system() {
                                games::save_cache(&config::games_cache_path(), &[]);
                            }
                            self.emit(IpcEvent::InstalledGames { games: Vec::new(), timed_out: false });
                            return;
                        }
                    }
                }
                self.installed_games = games;
                crate::logging::log(&format!("slots: found {} installed game(s) in {}ms", self.installed_games.len(), elapsed_ms));
                // One outcome channel for the picker. A FullState also carries
                // the list but cannot say whether the scan timed out, so the
                // UI could not tell a late result from a fresh failure.
                self.emit(IpcEvent::InstalledGames { games: self.installed_games.clone(), timed_out: false });
                // Persist for the next launch. Capture and sandbox runs must
                // never replace the real cache with their synthetic lists.
                if self.modes().live_system() {
                    let path = config::games_cache_path();
                    let cached = self.installed_games.clone();
                    std::thread::spawn(move || games::save_cache(&path, &cached));
                }
            }
            AppEvent::DisplayOutcome(outcome) => {
                self.watchdog.stage("display-outcome");
                let warning = outcome.warning.clone();
                self.emit(IpcEvent::DisplayLut {
                    r: outcome.luts.r.to_vec(),
                    g: outcome.luts.g.to_vec(),
                    b: outcome.luts.b.to_vec(),
                    applied: outcome.applied,
                    warning: warning.clone(),
                });
                if self.config.display.enabled && !outcome.applied && warning.is_none() {
                    self.emit(IpcEvent::DiagnosticBanner {
                        code: "display".into(),
                        message: "Windows rejected the gamma ramp for the selected display.".into(),
                        recoverable: true,
                    });
                }
                self.watchdog.stage("idle");
            }
            AppEvent::FxVerifyReport(report) => self.accept_fx_verify_report(&report),
            AppEvent::StressReport(report) => self.accept_stress_report(&report),
            AppEvent::FxSound(status) => {
                self.fx = status.clone();
                self.last_fx_refresh = Instant::now();
                let catch_up = deferred_audio_reconcile_due(
                    &mut self.pending_audio_reconcile,
                    self.config.audio.enabled,
                    &status,
                );
                self.emit(IpcEvent::FxSoundStatusEvent { status });
                if catch_up {
                    crate::logging::log("audio: FxSound became ready; applying the deferred startup reconciliation");
                    self.audio.reconcile(self.config.audio.clone());
                }
            }
            AppEvent::AudioNotice(message) => {
                self.emit(IpcEvent::DiagnosticBanner { code: "audio_notice".into(), message, recoverable: true });
            }
            AppEvent::AudioRecovery { code, message, actions } => {
                self.emit(IpcEvent::AudioRecovery { code, message, actions });
            }
            AppEvent::SoundMeter { peak } => {
                let visible = self.window.as_ref().map(|w| w.is_visible()).unwrap_or(false);
                if self.tab == "audio" && visible {
                    let headroom = dsp_math::headroom_db(peak, &self.config.audio);
                    self.emit(IpcEvent::SoundMeter { peak, headroom_db: headroom });
                }
            }
            AppEvent::DropFile(path) => {
                if self.backup_drop_armed.load(Ordering::Relaxed) {
                    if path.extension().and_then(|ext| ext.to_str()).is_some_and(|ext| ext.eq_ignore_ascii_case("zip")) {
                        self.inspect_backup_path(&path);
                    } else {
                        self.emit(IpcEvent::DiagnosticBanner {
                            code: "backup".into(),
                            message: "Drop a Crosshair+ .zip backup here.".into(),
                            recoverable: true,
                        });
                    }
                } else {
                    self.import_image_path(&path);
                }
            }
            AppEvent::VerifyReport(report) => self.finish_verify(&report),
            AppEvent::VerifyOverlay(report) => self.accept_verify_overlay(&report),
            AppEvent::Ready => {
                if !self.ui_ready {
                    self.ui_ready = true;
                    if self.running_apps.is_empty() {
                        self.refresh_running_apps();
                    }
                    self.update_preview();
                    self.send_full_state();
                    if let Some(event) = cleanup_warning_banner(&self.startup_warnings) {
                        self.emit(event);
                    }
                    // The cleanup is a one-time report: nothing may replay it.
                    self.startup_warnings.clear();
                    crate::logging::log("ui: Ready -> FullState");
                    if self.verify_mode {
                        self.start_verify();
                    }
                    if self.fx_verify_mode {
                        self.start_fx_verify();
                    }
                    if !self.started {
                        self.started = true;
                        if !self.verify_mode && !self.fx_verify_mode {
                            self.apply_display();
                            if !self.capture_mode {
                                if self.config.audio.enabled {
                                    self.reconcile_audio_on_startup();
                                }
                                self.sync_overlay();
                            }
                            self.update_meter_state();
                        }
                    }
                    if self.capture_mode {
                        self.capture_next = Some(Instant::now() + Duration::from_millis(900));
                    }
                }
            }
        }
    }

    fn handle_overlay(&mut self, event: OverlayEvent) {
        match event {
            OverlayEvent::Hotkey { id } => {
                crate::logging::log(&format!("overlay: hotkey {id}"));
                if id == hotkeys::RESTORE {
                    self.restore_display_from_hotkey();
                } else if id == hotkeys::AIM {
                    let before = self.config.crosshair.enabled;
                    self.toggle_module("crosshair");
                    if self.config.crosshair.enabled != before {
                        self.toast(if self.config.crosshair.enabled { "👁️ Aim On" } else { "👁️ Aim Off" });
                    }
                } else if id == hotkeys::DISPLAY {
                    let before = self.config.display.enabled;
                    self.toggle_module("display");
                    if self.config.display.enabled != before {
                        self.toast(if self.config.display.enabled { "🖥️ Screen On" } else { "🖥️ Screen Off" });
                    }
                } else if id == hotkeys::AUDIO {
                    let before = self.config.audio.enabled;
                    self.toggle_module("audio");
                    if self.config.audio.enabled != before {
                        self.toast(if self.config.audio.enabled { "🔊 Sound On" } else { "🔊 Sound Off" });
                    }
                } else if id == hotkeys::RESET {
                    self.config.crosshair.offset_x = 0;
                    self.config.crosshair.offset_y = 0;
                    self.config.crosshair.rotation = 0.0;
                    self.mark_dirty();
                    self.sync_overlay();
                    self.emit_full_state();
                    self.toast("↺ Reset · Centered");
                } else if id == hotkeys::CYCLE {
                    self.cycle_slot();
                } else if let Some(slot_id) = hotkeys::slot_for_hotkey_id(&self.config, id) {
                    self.switch_slot(Some(&slot_id), None);
                    if self.config.slots.active.as_deref() == Some(slot_id.as_str()) {
                        let text = self.slot_toast_text(&slot_id);
                        self.toast(text);
                    }
                }
            }
            OverlayEvent::HotkeyConflict { slot_id, key, owner } => {
                self.emit(IpcEvent::HotkeyConflict { slot_id, key, owner });
            }
            OverlayEvent::HotkeyResolved { slot_id, key } => {
                self.emit(IpcEvent::HotkeyResolved { slot_id, key });
            }
            OverlayEvent::DisplayChanged => self.queue_monitor_refresh(),
            OverlayEvent::ForegroundChanged => {
                // A foreground change is also a cheap chance to retry hotkeys
                // that another application briefly owned.
                self.sync_overlay();
                self.check_slots();
            }
            OverlayEvent::PowerSuspend => {
                self.compare_active = false;
                if !self.sandbox_blocked("display restore") {
                    self.display.restore_blocking();
                }
                if self.audio_compare_active {
                    self.audio_compare_active = false;
                    if !self.sandbox_blocked("audio compare restore") {
                        self.audio.apply(self.config.audio.clone());
                    }
                }
            }
            OverlayEvent::PowerResume | OverlayEvent::SessionUnlock => {
                self.queue_monitor_refresh();
                self.apply_display();
                self.sync_overlay();
            }
            OverlayEvent::EndSession => {
                if !self.sandbox_blocked("display restore") {
                    safety::restore_for_shutdown();
                }
                self.exit_requested = true;
            }
            OverlayEvent::Diagnostic { code, message } => {
                self.emit(IpcEvent::DiagnosticBanner { code, message, recoverable: true });
            }
        }
    }

    fn handle_command(&mut self, command: Command) {
        match command {
            Command::Ready => self.on_app_event(AppEvent::Ready),
            Command::SetTab { tab } => {
                if !actions::valid_tab(&tab) {
                    self.emit(IpcEvent::DiagnosticBanner { code: "tab".into(), message: format!("Unknown tab '{tab}'."), recoverable: true });
                    return;
                }
                let previous = self.tab.clone();
                self.tab = tab;
                self.update_meter_state();
                if self.tab == "aim" {
                    self.update_preview();
                }
                if self.tab == "audio" {
                    if self.modes().live_system() {
                        self.audio.refresh(true);
                    } else {
                        // `refresh(true)` runs FxSound with `--status`, which
                        // rewrites %APPDATA%\FxSound\status.json. Capture and
                        // every harness keep the status read at startup and
                        // never ask the real app to write anything.
                        crate::logging::log("sandbox: FxSound refresh skipped; real status untouched");
                    }
                    self.last_fx_refresh = Instant::now();
                }
                if self.tab == "slots" && previous != "slots" {
                    if self.last_apps_refresh.elapsed() > Duration::from_millis(500) {
                        self.refresh_running_apps();
                    }
                    if !self.games_scan.scanned {
                        self.scan_installed_games(false);
                    }
                    self.emit_full_state();
                }
            }
            Command::ToggleMaster { module } => self.toggle_module(&module),
            Command::UpdateCrosshair { mut crosshair } => {
                // Honor the preset the page applied; a manual edit arrives with no
                // preset id and clears it exactly like display and audio do.
                crosshair.normalize();
                crosshair.active_preset = crosshair
                    .active_preset
                    .as_deref()
                    .and_then(|id| self.config.find_crosshair_preset(id))
                    .map(|preset| preset.id.clone());
                // An image-mode preset whose file vanished must not replace the
                // live image with a dead path; keep the current image block and
                // tell the page what was actually kept.
                let preset = crosshair
                    .active_preset
                    .as_deref()
                    .and_then(|id| self.config.find_crosshair_preset(id));
                let missing_image =
                    config::guard_preset_image(&self.config.crosshair, &mut crosshair, preset.as_ref());
                self.config.crosshair = crosshair;
                self.mark_dirty();
                self.sync_overlay();
                self.update_preview();
                if let Some(name) = missing_image {
                    self.emit_full_state();
                    self.emit(IpcEvent::DiagnosticBanner {
                        code: "image".into(),
                        message: config::missing_preset_image_message(&name),
                        recoverable: true,
                    });
                }
            }
            Command::UpdateDisplay { mut display } => {
                display.normalize();
                self.config.display = display;
                self.mark_dirty();
                self.apply_display();
            }
            Command::CompareDisplay { active } => {
                // This is a temporary live desktop effect, never part of a
                // capture/verify run or a sandboxed harness.
                if !self.modes().live_system() {
                    crate::logging::log("sandbox: display compare skipped; real gamma ramps untouched");
                } else if active {
                    if !self.config.display.enabled {
                        crate::logging::log("display: compare ignored while Display is off");
                    } else if !self.compare_active {
                        self.compare_active = true;
                        self.display.preview_neutral(&self.monitors, &self.config.display);
                    }
                } else {
                    // Release always reapplies the current calibration. The UI
                    // only sends this for a held press, but an out-of-order or
                    // duplicate release still leaves the real ramps in sync.
                    self.compare_active = false;
                    self.apply_display();
                }
            }
            Command::CompareSound { active } => {
                // Temporary live effect, like CompareDisplay: capture and every
                // sandboxed harness leave FxSound completely untouched.
                if !self.modes().live_system() {
                    crate::logging::log("sandbox: sound compare skipped; FxSound untouched");
                } else if active {
                    if !self.config.audio.enabled {
                        crate::logging::log("audio: compare ignored while Sound is off");
                    } else if !self.audio_compare_active {
                        self.audio_compare_active = true;
                        self.audio.apply(dsp_math::flat_compare(&self.config.audio));
                        crate::logging::log("audio: compare applied a flat curve with power kept on");
                    }
                } else {
                    self.audio_compare_active = false;
                    self.apply_audio();
                }
            }
            Command::UpdateSound { mut audio } => {
                audio.normalize();
                self.config.audio = audio;
                self.mark_dirty();
                self.apply_audio();
            }
            Command::SavePreset { module, name, image_mode } => self.save_preset(&module, &name, image_mode),
            Command::DeletePreset { module, id } => self.delete_preset(&module, &id),
            Command::AssignHotkey { slot_id, hotkey } => {
                let clean = hotkey.trim();
                let Some(index) = self.config.slots.list.iter().position(|s| s.id == slot_id) else {
                    self.emit(IpcEvent::DiagnosticBanner { code: "hotkey".into(), message: format!("Unknown slot '{slot_id}'."), recoverable: true });
                    return;
                };
                if !clean.is_empty() && slots::parse_hotkey(clean).is_none() {
                    self.emit(IpcEvent::DiagnosticBanner { code: "hotkey".into(), message: "That key could not be registered.".into(), recoverable: true });
                    self.emit_full_state();
                    return;
                }
                let mut candidate = self.config.slots.list[index].clone();
                candidate.hotkey = if clean.is_empty() {
                    None
                } else {
                    Some(slots::canonical_hotkey(clean).unwrap_or_else(|| clean.to_string()))
                };
                if let Err(err) = slots::validate_slot(&self.config, &candidate, false) {
                    self.emit(IpcEvent::DiagnosticBanner { code: "hotkey".into(), message: err, recoverable: true });
                    self.emit_full_state();
                } else {
                    self.config.slots.list[index] = candidate;
                    self.mark_dirty();
                    self.sync_overlay();
                    self.emit_full_state();
                }
            }
            Command::AssignModuleHotkey { module, hotkey } => {
                let clean = hotkey.trim();
                if !matches!(module.as_str(), "aim" | "display" | "audio" | "restore" | "cycle") {
                    self.emit(IpcEvent::DiagnosticBanner { code: "hotkey".into(), message: format!("Unknown module '{module}'."), recoverable: true });
                    self.emit_full_state();
                    return;
                }
                if clean.is_empty() {
                    self.clear_module_hotkey(&module);
                    return;
                }
                let Some(canonical) = slots::canonical_hotkey(clean) else {
                    self.emit(IpcEvent::DiagnosticBanner { code: "hotkey".into(), message: "That key could not be registered.".into(), recoverable: true });
                    self.emit_full_state();
                    return;
                };
                if let Err(err) = slots::validate_hotkey(&self.config, &canonical, slots::HotkeyOwner::Module(&module)) {
                    self.emit(IpcEvent::DiagnosticBanner { code: "hotkey".into(), message: err, recoverable: true });
                    self.emit_full_state();
                    return;
                }
                match module.as_str() {
                    "aim" => self.config.settings.aim_hotkey = canonical,
                    "display" => self.config.settings.display_hotkey = canonical,
                    "audio" => self.config.settings.audio_hotkey = canonical,
                    "restore" => self.config.settings.restore_hotkey = canonical,
                    "cycle" => self.config.settings.cycle_hotkey = canonical,
                    _ => unreachable!(),
                }
                self.finish_hotkey_change();
            }
            Command::ClearModuleHotkey { module } => {
                if !matches!(module.as_str(), "aim" | "display" | "audio" | "restore" | "cycle") {
                    self.emit(IpcEvent::DiagnosticBanner { code: "hotkey".into(), message: format!("Unknown module '{module}'."), recoverable: true });
                    return;
                }
                self.clear_module_hotkey(&module);
            }
            Command::ClearHotkey { slot_id } => {
                let Some(slot) = self.config.slots.list.iter_mut().find(|s| s.id == slot_id) else {
                    self.emit(IpcEvent::DiagnosticBanner { code: "hotkey".into(), message: format!("Unknown slot '{slot_id}'."), recoverable: true });
                    return;
                };
                slot.hotkey = None;
                self.mark_dirty();
                self.sync_overlay();
                self.emit_full_state();
            }
            Command::BackupModalOpen { open } => {
                self.backup_drop_armed.store(open, Ordering::Relaxed);
                if !open {
                    self.pending_backup = None;
                }
            }
            Command::CreateBackup => self.spawn_pick(PickKind::BackupSave, None),
            Command::BrowseBackup => {
                self.pending_backup = None;
                self.spawn_pick(PickKind::BackupOpen, None);
            }
            Command::RestoreBackup => self.restore_pending_backup(),
            Command::UndoRestore => self.undo_last_restore(),
            Command::ImportImage { path } => {
                if let Some(path) = path {
                    self.import_image_path(Path::new(&path));
                } else {
                    self.browse_image();
                }
            }
            Command::RefreshRecentImages => {
                self.emit(IpcEvent::RecentImages { images: config::recent_crosshair_images() });
            }
            Command::UseStoredImage { name } => {
                match config::apply_stored_image(&mut self.config.crosshair, &name) {
                    Ok(()) => {
                        self.image_changed();
                        self.save_now();
                        self.emit_full_state();
                        self.emit(IpcEvent::StoredImageResult { name, ok: true });
                    }
                    Err(message) => {
                        // A failed pick must not leave its dead path live. The
                        // refresh below also recomputes the Recent list so the
                        // vanished file stops being selectable.
                        self.drop_vanished_live_image(&name);
                        self.emit_full_state();
                        self.emit(IpcEvent::StoredImageResult { name, ok: false });
                        self.emit(IpcEvent::DiagnosticBanner {
                            code: "image".into(),
                            message,
                            recoverable: true,
                        });
                    }
                }
            }
            Command::DeleteStoredImage { name } => {
                match config::delete_stored_image(&mut self.config, &name) {
                    Ok(was_active) => {
                        if was_active {
                            self.image_changed();
                        } else {
                            self.mark_dirty();
                        }
                        self.save_now();
                        self.emit_full_state();
                    }
                    Err(message) => {
                        // Deleting a file that already vanished outside the app
                        // must refresh the Recent list and drop the live image
                        // when it was that missing file.
                        self.drop_vanished_live_image(&name);
                        self.emit_full_state();
                        self.emit(IpcEvent::DiagnosticBanner {
                            code: "image".into(),
                            message,
                            recoverable: true,
                        });
                    }
                }
            }
            Command::RemoveImage => {
                self.config.crosshair.image = None;
                self.config.crosshair.image_name = None;
                self.image_changed();
                self.emit_full_state();
            }
            Command::ImportEq { path, text, name } => self.import_eq(path, text, name),
            Command::FlattenEqImport => {
                if let Some(bands) = dsp_math::flatten_import(&self.config.audio) {
                    self.config.audio.bands = bands;
                    self.config.audio.imported = None;
                    self.config.audio.active_preset = None;
                    self.mark_dirty();
                    self.save_now();
                    self.apply_audio();
                    self.emit_full_state();
                } else {
                    self.emit(IpcEvent::DiagnosticBanner {
                        code: "eq".into(),
                        message: "There is no imported EQ profile to apply.".into(),
                        recoverable: true,
                    });
                }
            }
            Command::ClearEqImport => {
                self.config.audio.imported = None;
                self.mark_dirty();
                self.apply_audio();
                self.emit_full_state();
            }
            Command::SelectSlot { id } => {
                let Some(id) = id else {
                    self.emit(IpcEvent::DiagnosticBanner { code: "slot".into(), message: "No slot selected.".into(), recoverable: true });
                    return;
                };
                if self.config.slots.list.iter().any(|s| s.id == id) {
                    self.selected_slot = Some(id);
                    self.emit_full_state();
                } else {
                    self.emit(IpcEvent::DiagnosticBanner { code: "slot".into(), message: format!("Unknown slot '{id}'."), recoverable: true });
                }
            }
            Command::SaveSlot { mut slot } => {
                slot.name = slot.name.chars().take(MAX_SLOT_NAME).collect();
                match slots::save_slot(&mut self.config, slot) {
                    Ok(()) => {
                        self.mark_dirty();
                        self.save_now();
                        self.sync_overlay();
                        self.emit_full_state();
                    }
                    Err(err) => {
                        self.emit(IpcEvent::DiagnosticBanner { code: "slot".into(), message: err, recoverable: true });
                        self.emit_full_state();
                    }
                }
            }
            Command::ActivateSlot { id, slot } => {
                if let Some(id) = id.as_deref() {
                    if !self.config.slots.list.iter().any(|slot| slot.id == id) {
                        self.emit(IpcEvent::DiagnosticBanner { code: "slot".into(), message: format!("Unknown slot '{id}'."), recoverable: true });
                        return;
                    }
                }
                self.switch_slot(id.as_deref(), slot);
            }
            Command::DeleteSlot { id } => {
                if self.config.slots.active.as_deref() == Some(id.as_str()) {
                    slots::deactivate_slot(&mut self.config);
                }
                self.config.slots.list.retain(|s| s.id != id);
                self.config.normalize();
                if self.selected_slot.as_deref() == Some(id.as_str()) || self.selected_slot.is_none() {
                    self.selected_slot = self.config.slots.active.clone().or_else(|| self.config.slots.list.first().map(|slot| slot.id.clone()));
                }
                self.mark_dirty();
                self.save_now();
                self.sync_overlay();
                self.apply_audio();
                self.apply_display();
                self.emit_full_state();
            }
            Command::BrowseSlotExe { slot_id } => {
                if !self.config.slots.list.iter().any(|s| s.id == slot_id) {
                    self.emit(IpcEvent::DiagnosticBanner { code: "slot".into(), message: format!("Unknown slot '{slot_id}'."), recoverable: true });
                    return;
                }
                self.spawn_pick(PickKind::SlotExe, Some(slot_id));
            }
            Command::ScanGames => self.scan_installed_games(true),
            Command::SettingsChanged { settings } => {
                if self.capture_mode {
                    self.config.settings = settings;
                } else if let Err(err) = actions::set_settings(&mut self.config, settings, &config::config_path()) {
                    self.emit(IpcEvent::DiagnosticBanner { code: "settings".into(), message: err, recoverable: true });
                    return;
                }
                if let Some(warning) = apply_run_key_checked(&self.config.settings, self.modes()) {
                    self.emit(IpcEvent::DiagnosticBanner { code: "settings".into(), message: warning, recoverable: true });
                }
                self.mark_dirty();
                self.sync_overlay();
                self.update_meter_state();
                self.emit_full_state();
            }
            Command::SetupAudio => {
                if self.sandbox_blocked("audio setup") {
                    return;
                }
                self.audio.setup();
                if self.config.audio.enabled {
                    self.audio.apply(self.config.audio.clone());
                }
            }
            Command::RepairAudio => {
                if self.sandbox_blocked("audio repair") {
                    return;
                }
                self.audio.repair();
                if self.config.audio.enabled {
                    self.audio.apply(self.config.audio.clone());
                }
            }
            Command::UnstickAudio => {
                if self.sandbox_blocked("audio unstick") {
                    return;
                }
                if self.fx.busy {
                    self.emit(IpcEvent::DiagnosticBanner {
                        code: "audio".into(),
                        message: "FxSound is busy; wait for the current task to finish.".into(),
                        recoverable: true,
                    });
                } else {
                    crate::logging::log("fxsound: unstick requested from Settings");
                    self.audio.unstick(self.config.audio.clone());
                }
            }
            Command::RepairManagedPreset => {
                if self.sandbox_blocked("repairing the managed sound preset") {
                    return;
                }
                if self.fx.busy {
                    self.emit(IpcEvent::DiagnosticBanner {
                        code: "audio".into(),
                        message: "FxSound is busy; wait for the current task to finish.".into(),
                        recoverable: true,
                    });
                } else {
                    crate::logging::log("fxsound: managed preset repair requested");
                    self.audio.repair_preset(self.config.audio.clone());
                }
            }
            Command::OpenFxSoundPresets => {
                if self.sandbox_blocked("opening the FxSound presets folder") {
                    return;
                }
                match crate::audio::fxsound::presets_folder() {
                    Ok(path) => open_folder(&path),
                    Err(err) => self.emit(IpcEvent::DiagnosticBanner {
                        code: "audio".into(),
                        message: err,
                        recoverable: true,
                    }),
                }
            }
            Command::OpenFxSound => {
                if self.sandbox_blocked("opening FxSound") {
                    return;
                }
                self.audio.open();
            }
            Command::OpenFxSoundDownload => {
                if self.sandbox_blocked("opening the FxSound download page") {
                    return;
                }
                crate::audio::fxsound::open_url(crate::audio::fxsound::DOWNLOAD_URL);
            }
            Command::OpenWindowsDisplaySettings => {
                if self.sandbox_blocked("opening Windows display settings") {
                    return;
                }
                open_windows_display();
            }
            Command::OpenDataFolder => {
                if self.sandbox_blocked("opening the data folder") {
                    return;
                }
                match actions::execute(&Command::OpenDataFolder, &mut self.config, &config::config_path(), &config::data_dir()) {
                    Ok(_) => open_folder(&config::data_dir()),
                    Err(err) => self.emit(IpcEvent::DiagnosticBanner { code: "data".into(), message: err, recoverable: true }),
                }
            }
            Command::ResetDisplay => {
                if !self.sandbox_blocked("display restore") {
                    self.display.restore_blocking();
                }
                if self.capture_mode {
                    self.config.display = crate::config::DisplayConfig::default();
                } else {
                    if let Err(err) = actions::execute(&Command::ResetDisplay, &mut self.config, &config::config_path(), &config::data_dir()) {
                        self.emit(IpcEvent::DiagnosticBanner { code: "display".into(), message: err, recoverable: true });
                        return;
                    }
                }
                self.mark_dirty();
                self.emit_neutral_lut();
                self.emit_full_state();
            }
            Command::FactoryReset => {
                if !self.sandbox_blocked("factory reset system restore") {
                    // Factory reset is the user-facing escape hatch: restore both
                    // the OS gamma ramps and the saved FxSound power state before
                    // the settings that describe them are wiped.
                    safety::restore_all_blocking();
                    safety::reset_guards();
                    self.display.restore_blocking();
                }
                let (removed_config, removed_images) = if self.capture_mode {
                    self.config = Config::default();
                    (false, 0)
                } else {
                    match actions::execute(&Command::FactoryReset, &mut self.config, &config::config_path(), &config::data_dir()) {
                        Ok(actions::Effect::FactoryReset { removed_config, removed_images }) => (removed_config, removed_images),
                        Ok(_) => (false, 0),
                        Err(err) => {
                            self.emit(IpcEvent::DiagnosticBanner { code: "settings".into(), message: err, recoverable: true });
                            return;
                        }
                    }
                };
                crate::logging::log(&format!(
                    "factory reset: config_removed={removed_config} images_removed={removed_images}"
                ));
                self.selected_slot = None;
                apply_run_key_checked(&self.config.settings, self.modes());
                // Drop the last FxSound snapshot so the Sound tab reports the
                // fresh install instead of the reset session's cached status.
                if self.modes().live_system() {
                    self.audio.refresh(true);
                }
                self.fx = fxsound::status();
                self.mark_dirty();
                self.sync_overlay();
                self.emit_neutral_lut();
                self.emit_full_state();
            }
            Command::OpenLog => {
                if self.sandbox_blocked("opening the log") {
                    return;
                }
                let path = config::log_path();
                if path.exists() {
                    open_path(&path);
                } else {
                    self.emit(IpcEvent::DiagnosticBanner { code: "log".into(), message: "No log has been written yet.".into(), recoverable: true });
                }
            }
            Command::OpenMain => self.show_window(),
            Command::OpenSettings => {
                self.show_window();
                if let Err(err) = self.evaluate("document.querySelector('[data-action=settings]').click();") {
                    crate::logging::log(&format!("tray: could not open settings: {err}"));
                }
            }
            Command::ExitApp => {
                self.hide_tray_panel();
                self.exit_requested = true;
            }
            Command::HideTray => self.hide_tray_panel(),
        }
    }

    fn finish_hotkey_change(&mut self) {
        self.mark_dirty();
        self.save_now();
        self.sync_overlay();
        self.emit_full_state();
    }

    fn clear_module_hotkey(&mut self, module: &str) {
        match module {
            "aim" => self.config.settings.aim_hotkey.clear(),
            "display" => self.config.settings.display_hotkey.clear(),
            "audio" => self.config.settings.audio_hotkey.clear(),
            "restore" => self.config.settings.restore_hotkey.clear(),
            "cycle" => self.config.settings.cycle_hotkey.clear(),
            _ => {}
        }
        self.finish_hotkey_change();
    }

    /// Escape hatch for a calibration that hides the screen: stop applying the
    /// display config and hand the OS ramps back without opening the window.
    fn restore_display_from_hotkey(&mut self) {
        if !self.modes().live_system() {
            crate::logging::log("sandbox: display restore hotkey skipped; real gamma ramps untouched");
            return;
        }
        self.config.display.enabled = false;
        self.mark_dirty();
        self.save_now();
        self.display.restore_blocking();
        self.emit_neutral_lut();
        self.emit_full_state();
        crate::logging::log("display: panic restore via hotkey; module disabled");
    }

    fn toggle_module(&mut self, module: &str) {
        let effect = if self.capture_mode {
            actions::toggle_module_in_memory(&mut self.config, module)
        } else {
            actions::toggle_module(&mut self.config, module, &config::config_path())
        };
        let effect = match effect {
            Ok(effect) => effect,
            Err(err) => {
                self.emit(IpcEvent::DiagnosticBanner { code: "module".into(), message: err, recoverable: true });
                return;
            }
        };
        self.apply_module_effect(effect);
    }

    fn apply_module_effect(&mut self, effect: actions::ToggleEffect) {
        self.mark_dirty();
        if effect.applies_display() {
            self.apply_display();
        } else if effect.applies_audio() {
            self.apply_audio();
        }
        self.sync_overlay();
        self.update_preview();
        self.emit_full_state();
    }

    /// One slot hotkey: first slot -> next -> … -> Off, reusing the same
    /// snapshot-preserving switch path as the UI and the tray.
    fn cycle_slot(&mut self) {
        let Some(target) = slots::cycle_target(&self.config) else { return };
        match target {
            slots::CycleTarget::Slot(id) => {
                self.switch_slot(Some(&id), None);
                if self.config.slots.active.as_deref() == Some(id.as_str()) {
                    self.auto_suppressed_for = slots::current_foreground_app().map(|app| app.exe);
                    let text = self.slot_toast_text(&id);
                    self.toast(text);
                }
            }
            slots::CycleTarget::Off => {
                if self.config.slots.active.is_none() {
                    return;
                }
                self.switch_slot(None, None);
                if self.config.slots.active.is_none() {
                    self.auto_suppressed_for = slots::current_foreground_app().map(|app| app.exe);
                    self.toast("Slots off");
                }
            }
        }
    }

    fn slot_toast_text(&self, id: &str) -> String {
        let Some(slot) = self.config.slots.list.iter().find(|slot| slot.id == id) else {
            return "Slot switched".into();
        };
        let mut modules = Vec::new();
        if slot.crosshair.on {
            modules.push("Aim");
        }
        if slot.display.on {
            modules.push("Screen");
        }
        if slot.audio.on {
            modules.push("Sound");
        }
        let suffix = if modules.is_empty() { "Off".to_string() } else { modules.join(" + ") };
        format!("{} {} · {suffix}", slot.emoji, slot.name)
    }

    /// The one gate for the in-game pill. Capture/verify runs never create a
    /// topmost window; a normal sandbox run does not either. The stress harness
    /// forces toasts so the OSD window and timer are included in its leak check.
    fn toast(&mut self, text: impl Into<String>) {
        if !self.stress_mode && (!self.modes().live_system() || !self.config.settings.osd_toast) {
            return;
        }
        self.overlay.toast(text.into());
    }

    /// Save the selected card's edits and switch activation in one transaction.
    /// Only one full state is emitted, and the cache of running apps is reused
    /// so a switch never walks every window and extracts icons.
    fn switch_slot(&mut self, id: Option<&str>, save: Option<Slot>) {
        let total_started = Instant::now();
        let target = id.map(str::to_string);
        crate::logging::log(&format!("slots: switch begin target={} saved={}", target.as_deref().unwrap_or("none"), save.is_some()));
        self.watchdog.stage("slot-save");
        let was_active = target.as_deref()
            .map(|id| self.config.slots.active.as_deref() == Some(id))
            .unwrap_or(false);

        let save_started = Instant::now();
        let saved = if let Some(mut slot) = save {
            slot.name = slot.name.chars().take(MAX_SLOT_NAME).collect();
            if let Err(err) = slots::save_slot(&mut self.config, slot) {
                self.emit(IpcEvent::DiagnosticBanner { code: "slot".into(), message: err, recoverable: true });
                self.emit_full_state();
                return;
            }
            true
        } else {
            false
        };
        let save_ms = save_started.elapsed().as_millis();
        self.watchdog.stage("slot-activate");

        if let Some(id) = &target {
            self.selected_slot = Some(id.clone());
        }
        let toggle = if was_active { None } else { target.as_deref() };
        let (changed, missing_image) = match slots::switch_slot(&mut self.config, toggle, None) {
            Ok(result) => result,
            Err(err) => {
                self.emit(IpcEvent::DiagnosticBanner { code: "slot".into(), message: err, recoverable: true });
                return;
            }
        };
        if self.selected_slot.is_none() {
            self.selected_slot = self.config.slots.active.clone()
                .or_else(|| self.config.slots.list.first().map(|slot| slot.id.clone()));
        }

        if changed || saved {
            self.mark_dirty();
            self.watchdog.stage("slot-persist");
            self.save_now();
        }
        self.watchdog.stage("slot-runtime");
        let display_started = Instant::now();
        if changed {
            self.apply_display();
            self.apply_audio();
        }
        let display_ms = display_started.elapsed().as_millis();
        self.watchdog.stage("slot-overlay");
        let overlay_started = Instant::now();
        self.sync_overlay();
        let overlay_ms = overlay_started.elapsed().as_millis();
        self.watchdog.stage("slot-preview");
        let preview_started = Instant::now();
        if self.tab == "aim" {
            self.update_preview();
        }
        let preview_ms = preview_started.elapsed().as_millis();
        self.watchdog.stage("slot-state");
        let state_started = Instant::now();
        self.emit_full_state();
        let state_ms = state_started.elapsed().as_millis();
        if let Some(name) = missing_image {
            self.emit(IpcEvent::DiagnosticBanner {
                code: "image".into(),
                message: config::missing_preset_image_message(&name),
                recoverable: true,
            });
        }
        self.watchdog.stage("idle");
        crate::logging::log(&format!(
            "slots: switch target={} active={:?} changed={} saved={} save={}ms display+audio={}ms overlay={}ms preview={}ms state={}ms total={}ms",
            target.as_deref().unwrap_or("none"),
            self.config.slots.active,
            changed,
            saved,
            save_ms,
            display_ms,
            overlay_ms,
            preview_ms,
            state_ms,
            total_started.elapsed().as_millis()
        ));
    }

    /// Discover games installed by Steam, Epic and Xbox so the Auto-launch
    /// picker is not limited to processes that are already running. The scan
    /// reads known folders on a worker thread and is cached for the session and
    /// on disk, so the next launch opens the picker without walking them again.
    fn scan_installed_games(&mut self, explicit: bool) {
        let Some(generation) = self.games_scan.begin() else { return; };
        let proxy = self.proxy.clone();
        if !self.modes().live_system() {
            // Automated runs stay deterministic and must not walk the disk.
            let _ = proxy.send_event(AppEvent::InstalledGames {
                generation,
                explicit,
                cached_alive: Vec::new(),
                games: Vec::new(),
                elapsed_ms: 0,
            });
            return;
        }
        // The worker prunes the cached list, so no filesystem probe lands on
        // the UI thread.
        let cached = self.installed_games.clone();
        std::thread::spawn(move || {
            let started = Instant::now();
            let games = games::scan();
            // A real result replaces the list wholesale; only an empty scan
            // needs the cached entries checked.
            let cached_alive = if games.is_empty() { games::retain_existing(cached) } else { Vec::new() };
            let _ = proxy.send_event(AppEvent::InstalledGames {
                generation,
                explicit,
                cached_alive,
                games,
                elapsed_ms: started.elapsed().as_millis(),
            });
        });
    }

    /// The scan thread cannot be interrupted, so a background refresh is
    /// released after a grace period instead of sitting on the Scanning state
    /// forever when a library folder (for example a network share) never
    /// answers. The cached list stays on screen, and a late result still
    /// replaces it when the thread finally reports.
    fn poll_game_scan(&mut self) {
        const GAME_SCAN_GRACE: Duration = Duration::from_secs(30);
        if !self.games_scan.time_out(GAME_SCAN_GRACE) {
            return;
        }
        crate::logging::log(&format!("slots: background game scan exceeded {}s; keeping the current list", GAME_SCAN_GRACE.as_secs()));
        self.emit(IpcEvent::InstalledGames { games: self.installed_games.clone(), timed_out: true });
    }

    /// One line per window so a session's log answers what the crosshair costs
    /// while visible, next to windows where Aim was off.
    fn poll_overlay_cost(&mut self) {
        const COST_WINDOW: Duration = Duration::from_secs(30);
        if !self.modes().live_system() && !self.stress_mode {
            return;
        }
        let elapsed = self.overlay_cost_at.elapsed();
        if elapsed < COST_WINDOW {
            return;
        }
        self.overlay_cost_at = Instant::now();
        let previous = self.overlay_cost;
        self.overlay_cost = self.overlay.cost_snapshot();
        let process = process_cpu_100ns();
        // "visible" describes the window, not the instant of reporting, so a
        // window that straddles an F8 toggle is not called hidden while it
        // holds hundreds of paints.
        let visible = self.overlay_cost.paints > previous.paints || self.overlay.is_visible();
        crate::logging::log(&overlay_cost_line(
            elapsed,
            &previous,
            &self.overlay_cost,
            self.process_cpu,
            process,
            visible,
        ));
        self.process_cpu = process;
    }

    fn refresh_running_apps(&mut self) {
        if self.apps_refreshing {
            return;
        }
        self.apps_refreshing = true;
        self.last_apps_refresh = Instant::now();
        let proxy = self.proxy.clone();
        std::thread::spawn(move || {
            let started = Instant::now();
            let apps = slots::visible_apps();
            let _ = proxy.send_event(AppEvent::RunningApps {
                apps,
                elapsed_ms: started.elapsed().as_millis(),
            });
        });
    }

    /// True while a sandboxed launch must not touch the real machine. Every
    /// launcher and restore path calls this first so an automated run cannot
    /// open desktop windows or rewrite gamma ramps outside the sandbox.
    fn sandbox_blocked(&self, what: &str) -> bool {
        if !self.modes().live_system() {
            crate::logging::log(&format!("sandbox: {what} skipped; real system untouched"));
            return true;
        }
        false
    }

    fn modes(&self) -> RunModes {
        RunModes::new(
            self.capture_mode,
            self.verify_mode,
            self.fx_verify_mode,
            self.interaction_mode,
            self.stress_mode,
        )
    }

    fn apply_display(&mut self) {
        match display_path(self.modes()) {
            DisplayPath::Preview => {
                let config = self.config.display.clone();
                let selected = monitors::find(&self.monitors, config.selected_monitor.as_deref()).clone();
                let luts = DisplayEngine::preview(&config, &selected);
                let warning = crate::display::warning::for_monitor(&selected);
                crate::logging::log("capture: display preview only; OS gamma ramp untouched");
                self.emit(IpcEvent::DisplayLut {
                    r: luts.r.to_vec(),
                    g: luts.g.to_vec(),
                    b: luts.b.to_vec(),
                    applied: true,
                    warning,
                });
                return;
            }
            DisplayPath::Skip => {
                crate::logging::log("sandbox: display apply skipped; real gamma ramps untouched");
                return;
            }
            DisplayPath::Live => {}
        }
        self.watchdog.stage("display-apply");
        let config = self.config.display.clone();
        // Apply on the display worker. The UI must never wait for a display
        // driver or DDC read-back; the outcome arrives as an event.
        self.display.apply(&self.monitors, &config);
        self.apply_backlight();
        self.watchdog.stage("idle");
    }

    fn apply_audio(&mut self) {
        if audio_path(self.modes()) == AudioPath::Skip {
            crate::logging::log("sandbox: audio apply skipped; FxSound untouched");
            return;
        }
        self.audio.apply(self.config.audio.clone());
    }


    /// Launch is not a user edit. Reconcile only with an already-running, ready
    /// FxSound; never launch or rewrite an untouched session just to report a
    /// failed apply as a red toast.
    fn reconcile_audio_on_startup(&mut self) {
        if audio_path(self.modes()) == AudioPath::Skip {
            crate::logging::log("sandbox: startup audio reconciliation skipped; FxSound untouched");
            return;
        }
        let fx = self.audio.status();
        if !startup_audio_eligible(&fx) {
            self.pending_audio_reconcile = startup_audio_deferred(self.config.audio.enabled, &fx);
            crate::logging::log(&format!(
                "audio: startup reconciliation skipped; FxSound installed={} running={} cli_ready={} deferred={}",
                fx.installed, fx.running, fx.cli_ready, self.pending_audio_reconcile
            ));
            return;
        }
        self.pending_audio_reconcile = false;
        self.audio.reconcile(self.config.audio.clone());
    }

    fn emit_neutral_lut(&mut self) {
        let luts = crate::display::curves::Luts::neutral();
        self.emit(IpcEvent::DisplayLut {
            r: luts.r.to_vec(),
            g: luts.g.to_vec(),
            b: luts.b.to_vec(),
            applied: false,
            warning: None,
        });
    }

    fn sync_overlay(&mut self) {
        if !self.modes().live_system() && !self.interaction_mode && !self.stress_mode && !self.verify_mode {
            return;
        }
        let monitor = monitors::find(&self.monitors, self.config.crosshair.monitor.as_deref());
        let hotkeys = active_hotkeys(self.modes(), &self.config);
        let signature = (
            self.config.crosshair.enabled,
            monitor.name.clone(),
            hotkeys.len(),
        );
        if self.last_overlay_log.as_ref() != Some(&signature) {
            self.last_overlay_log = Some(signature);
            crate::logging::log(&format!(
                "overlay: sync enabled={} monitor={}x{}+{},{} hotkeys={}",
                self.config.crosshair.enabled,
                monitor.width,
                monitor.height,
                monitor.x,
                monitor.y,
                hotkeys.len()
            ));
        }
        // A verify run is sandboxed and must not create a desktop overlay, but
        // its image step still needs to prove the update reached this handle.
        let rect = if self.verify_mode {
            None
        } else {
            Some((monitor.x, monitor.y, monitor.width, monitor.height))
        };
        let input = overlay_input(&self.config.crosshair, rect, hotkeys, monitor.name.clone());
        self.overlay.update(input);
    }

    /// Queues a render of the current crosshair. The worker answers with
    /// `PreviewReady`, which carries the next animation deadline, so this is
    /// also the only place a frame change is announced to the page.
    fn update_preview(&mut self) {
        if self.tab != "aim" {
            return;
        }
        self.preview.render(&self.config.crosshair);
        // Safety net: if the worker skips this request because the frame is
        // already published, poll again soon instead of stalling.
        self.preview_due = Some(Instant::now() + Duration::from_millis(33));
    }

    fn emit_preview_tick(&mut self) {
        if self.tab != "aim" {
            return;
        }
        self.emit(IpcEvent::PreviewTick { v: self.preview_tick });
    }

    fn preview_animated(&self) -> bool {
        let path = self.config.crosshair.image.as_deref().unwrap_or("").to_ascii_lowercase();
        (path.ends_with(".gif") || path.ends_with(".webp")) && self.config.crosshair.gif_playing
    }

    fn poll_preview(&mut self) {
        if self.tab != "aim" || !self.preview_animated() {
            return;
        }
        let Some(due) = self.preview_due else { return };
        if Instant::now() < due {
            return;
        }
        // The real cadence comes from the frame delays; this only keeps the
        // animation alive if a frame or its event ever goes missing.
        self.preview_due = Some(Instant::now() + Duration::from_millis(33));
        self.preview.render(&self.config.crosshair);
    }

    /// One line per window while an animated preview is on screen, so a
    /// session's log says whether the box keeps up with the source instead of
    /// leaving it to the eye.
    fn poll_preview_cost(&mut self) {
        const COST_WINDOW: Duration = Duration::from_secs(10);
        if !self.modes().live_system() && !self.stress_mode {
            return;
        }
        let Ok(current) = self.preview_cache.lock().map(|cache| cache.cost()) else { return };
        if self.tab != "aim" || !self.preview_animated() {
            // Keep the baseline moving so a reporting window only ever covers
            // time the preview was actually animating.
            self.preview_cost_at = Instant::now();
            self.preview_cost = current;
            return;
        }
        let elapsed = self.preview_cost_at.elapsed();
        if elapsed < COST_WINDOW {
            return;
        }
        self.preview_cost_at = Instant::now();
        let previous = self.preview_cost;
        self.preview_cost = current;
        crate::logging::log(&preview_cost_line(elapsed, &previous, &current));
    }

    /// Debounce display-change bursts and probe on a worker. DDC/WMI reads can
    /// take seconds and must never run on the UI thread.
    fn queue_monitor_refresh(&mut self) {
        self.monitors_refresh_due = Some(Instant::now() + Duration::from_millis(250));
    }

    fn poll_monitor_refresh(&mut self) {
        if self.monitors_refreshing {
            return;
        }
        let Some(due) = self.monitors_refresh_due else { return };
        if Instant::now() < due {
            return;
        }
        self.monitors_refresh_due = None;
        self.monitors_refreshing = true;
        let proxy = self.proxy.clone();
        let probes = self.backlight.handle();
        std::thread::spawn(move || {
            let com = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) }.is_ok();
            let monitors = monitors::refresh(&probes);
            if com {
                unsafe { CoUninitialize() };
            }
            let _ = proxy.send_event(AppEvent::MonitorsRefreshed(monitors));
        });
    }

    fn apply_backlight(&mut self) {
        let backlight = self.config.display.backlight.clone();
        for (id, percent) in backlight {
            if let Some(monitor) = self.monitors.iter().find(|m| m.id == id && m.backlight_supported) {
                self.backlight.set(monitor.gdi.clone(), percent);
            }
        }
    }

    /// Probe replies arrive from the pacer thread; drain them without blocking
    /// the UI and publish the monitor state once per batch. The first update
    /// after a display change can therefore say `false` until the reply lands.
    fn poll_backlight_probes(&mut self) {
        let mut received = false;
        while let Some((gdi, value)) = self.backlight.poll() {
            received = true;
            crate::logging::log(&format!(
                "monitors: backlight {}={}",
                gdi,
                value.map(|v| format!("{v}%")).unwrap_or_else(|| "unsupported".to_string())
            ));
            if let Some(monitor) = self.monitors.iter_mut().find(|m| m.gdi.eq_ignore_ascii_case(&gdi)) {
                monitor.backlight_supported = value.is_some();
                monitor.backlight = value;
            }
        }
        if !received {
            return;
        }
        let backlight = self
            .monitors
            .iter()
            .map(|monitor| format!("{}={}", monitor.gdi, monitor.backlight_supported))
            .collect::<Vec<_>>()
            .join(",");
        crate::logging::log(&format!("monitors: updated count={} backlight={backlight}", self.monitors.len()));
        self.emit(IpcEvent::MonitorsUpdated { monitors: self.monitors.clone() });
        self.apply_backlight();
    }

    fn apply_monitor_refresh(&mut self, monitors: Vec<MonitorInfo>) {
        self.monitors = monitors;
        // A probe reply can land between the worker's cache read and this
        // event. Re-check the cache so that result is never overwritten by a
        // stale `false` from the refresh it raced.
        let probes = self.backlight.handle();
        for monitor in &mut self.monitors {
            if !monitor.backlight_supported {
                if let Some(Some(value)) = probes.probe(&monitor.gdi) {
                    monitor.backlight_supported = true;
                    monitor.backlight = Some(value);
                }
            }
        }
        self.display.ensure_baselines(&self.monitors);
        let released = self.display_blocks.observe(&self.monitors);
        if crate::display::recovery::should_reapply(
            self.config.display.enabled,
            self.compare_active,
            &released,
        ) {
            crate::logging::log(&format!(
                "display: HDR/ACM cleared on {}; re-applying the active LUT",
                released.join(", ")
            ));
        }
        let backlight = self
            .monitors
            .iter()
            .map(|monitor| {
                format!(
                    "{}={}",
                    monitor.gdi,
                    if monitor.backlight_supported { "true" } else { "pending" }
                )
            })
            .collect::<Vec<_>>()
            .join(",");
        crate::logging::log(&format!("monitors: updated count={} backlight={backlight}", self.monitors.len()));
        self.emit(IpcEvent::MonitorsUpdated { monitors: self.monitors.clone() });
        if self.compare_active {
            // Hold to Compare owns the ramps; release re-applies them.
            crate::logging::log("display: monitor refresh during compare; keeping identity ramps until release");
        } else {
            self.apply_display();
        }
        self.sync_overlay();
    }

    fn poll_fx_status(&mut self) {
        if self.fx.busy || !self.modes().live_system() {
            return;
        }
        if self.pending_audio_reconcile && !self.config.audio.enabled {
            self.pending_audio_reconcile = false;
            crate::logging::log("audio: deferred startup reconciliation cancelled; Sound is off");
        }
        // The Audio tab polls while visible; a deferred launch catch-up also
        // needs the cheap file read while the window sits on another tab.
        let catching_up = self.pending_audio_reconcile && self.config.audio.enabled;
        if self.tab != "audio" && !catching_up {
            return;
        }
        if self.last_fx_refresh.elapsed() < Duration::from_secs(3) {
            return;
        }
        self.last_fx_refresh = Instant::now();
        self.audio.refresh(false);
    }

    fn poll_watchers(&mut self) {
        if self.last_watch.elapsed() < Duration::from_secs(1) {
            return;
        }
        self.last_watch = Instant::now();
        self.check_slots();
    }

    fn check_slots(&mut self) {
        if !self.modes().live_system() {
            return;
        }
        let foreground = slots::current_foreground_app();
        // A Cycle is a deliberate manual override. Keep the watcher out until
        // the user moves to a different foreground application, so the slot the
        // user just picked is not replaced a second later.
        if let Some(marker) = self.auto_suppressed_for.clone() {
            if foreground.as_ref().map(|app| app.exe.as_str()) == Some(marker.as_str()) {
                return;
            }
            self.auto_suppressed_for = None;
        }
        // Reverting on focus loss/game exit is the only behaviour the setting
        // controls; a targeted slot always activates when its game is focused.
        if self.config.settings.auto_revert_slots && slots::active_target_stopped(&self.config) {
            if self.slot_absence.observe(self.config.slots.active.as_deref()) {
                if slots::deactivate_slot(&mut self.config) {
                    self.mark_dirty();
                    self.save_now();
                    self.apply_display();
                    self.apply_audio();
                    self.sync_overlay();
                    self.update_preview();
                    self.emit_full_state();
                }
                self.slot_absence.reset();
            }
            return;
        }
        self.slot_absence.reset();
        if let Some(app) = foreground {
            if let Some(id) = slots::auto_activate_for_app(&self.config, &app) {
                self.switch_slot(Some(&id), None);
                if self.config.slots.active.as_deref() == Some(id.as_str()) {
                    let text = self.slot_toast_text(&id);
                    self.toast(text);
                }
            }
        }
    }

    fn poll_tray(&mut self) {
        while let Ok(event) = TrayIconEvent::receiver().try_recv() {
            if let TrayIconEvent::Click { button, button_state, position, rect, .. } = event {
                if button_state != tray_icon::MouseButtonState::Up {
                    continue;
                }
                match button {
                    tray_icon::MouseButton::Left => self.show_window(),
                    tray_icon::MouseButton::Right => self.toggle_tray_panel(position, rect),
                    tray_icon::MouseButton::Middle => {}
                }
            }
        }
        self.poll_tray_foreground();
    }

    /// Hide the flyout when another window takes the foreground.
    ///
    /// Window focus is not usable here: the flyout's WebView2 child holds
    /// keyboard focus, so the top-level window stops receiving focus events
    /// after the page is clicked. The system foreground window still names the
    /// flyout while its child is focused, which makes this check work for both
    /// the opening handshake and later click-aways.
    fn poll_tray_foreground(&mut self) {
        let Some(window) = self.tray_window.as_ref() else { return; };
        if !window.is_visible() {
            self.tray_owns_foreground = false;
            return;
        }
        let panel = HWND(window.hwnd() as *mut c_void);
        let foreground = unsafe { GetForegroundWindow() };
        if foreground.0 == panel.0 {
            self.tray_owns_foreground = true;
            return;
        }
        // Windows can briefly report no foreground window during an
        // activation change; only a real other window dismisses the flyout.
        if self.tray_owns_foreground && !foreground.0.is_null() {
            self.hide_tray_panel();
        }
    }

    fn poll_save(&mut self) {
        if self.dirty && self.last_save.elapsed() > Duration::from_millis(450) {
            self.save_now();
        }
    }

    /// Seed the sandbox crosshairs folder for the Recent-menu probe. Only one
    /// image is written; the page also injects a stale name that is absent from
    /// disk, simulating an external delete the cached list cannot know about.
    fn seed_recent_probe_images(&self) {
        let dir = config::crosshairs_dir();
        if let Err(err) = std::fs::create_dir_all(&dir) {
            crate::logging::log(&format!("capture recent: cannot create {}: {err}", dir.display()));
            return;
        }
        // A previous probe run may have left the stale name behind.
        let _ = std::fs::remove_file(dir.join("recent-dead.webp"));
        let source = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("assets/test.webp");
        match std::fs::copy(&source, dir.join("recent-alive.webp")) {
            Ok(_) => crate::logging::log("capture recent: seeded recent-alive.webp"),
            Err(err) => crate::logging::log(&format!("capture recent: cannot seed recent-alive.webp: {err}")),
        }
    }

    fn poll_capture(&mut self) {
        if !self.capture_mode || !self.ui_ready {
            return;
        }
        let Some(next) = self.capture_next else { return };
        if Instant::now() < next {
            return;
        }
        let names = ["crosshair", "display", "audio", "slots"];
        if self.capture_stage >= names.len() {
            if !self.selftest_done {
                self.selftest_done = true;
                self.seed_recent_probe_images();
                self.selftest_started_at = Some(Instant::now());
                let complete = self.selftest_complete.clone();
                if let Some(webview) = &self.webview {
                    if let Err(err) = webview.evaluate_script_with_callback(SELFTEST_JS, move |result| {
                        crate::logging::log(&format!("selftest: {result}"));
                        complete.store(true, Ordering::Relaxed);
                    }) {
                        crate::logging::log(&format!("selftest: could not run: {err}"));
                    }
                }
                self.capture_next = Some(Instant::now() + Duration::from_millis(250));
            } else if self.selftest_started_at.is_some_and(|started| started.elapsed() < Duration::from_secs(6))
                || (!self.selftest_complete.load(Ordering::Relaxed)
                    && self.selftest_started_at.is_some_and(|started| started.elapsed() < Duration::from_secs(30)))
            {
                // The self-test drives real controls and awaits state changes;
                // do not resize under it while it is still running.
                self.capture_next = Some(Instant::now() + Duration::from_millis(250));
            } else if !self.recent_probe_logged {
                self.recent_probe_logged = true;
                if let Some(webview) = &self.webview {
                    if let Err(err) = webview.evaluate_script_with_callback(
                        "JSON.stringify(window.__crosshairRecentProbe || null)",
                        |result| crate::logging::log(&format!("recent-menu: {result}")),
                    ) {
                        crate::logging::log(&format!("recent-menu: could not read: {err}"));
                    }
                }
                self.capture_next = Some(Instant::now() + Duration::from_millis(100));
            } else if !self.layout_resized {
                self.layout_resized = true;
                if let Some(window) = &self.window {
                    window.set_inner_size(PhysicalSize::new(1040, 680));
                }
                if let Err(err) = self.evaluate("window.__crosshair_setTab('display');") {
                    crate::logging::log(&format!("capture layout: could not switch to display: {err}"));
                }
                self.capture_next = Some(Instant::now() + Duration::from_millis(700));
            } else if !self.layout_probe_done {
                self.layout_probe_done = true;
                if let Some(webview) = &self.webview {
                    if let Err(err) = webview.evaluate_script_with_callback(LAYOUT_JS, |result| {
                        crate::logging::log(&format!("layout-1040x680: {result}"));
                    }) {
                        crate::logging::log(&format!("layout-1040x680: could not run: {err}"));
                    }
                }
                self.capture_next = Some(Instant::now() + Duration::from_millis(600));
            } else {
                self.exit_requested = true;
            }
            return;
        }
        if let Some((name, path, receiver)) = self.pending_capture.take() {
            match receiver.try_recv() {
                Ok(Ok(bytes)) => {
                    match crate::screenshot::save_captured(bytes, &path, 1200, 780) {
                        Ok(()) => {
                            if let Ok(saved) = std::fs::read(&path) {
                                if let Some((w, h)) = crate::screenshot::png_size(&saved) {
                                    crate::logging::log(&format!("capture {name}: {w}x{h}"));
                                }
                            }
                        }
                        Err(err) => crate::logging::log(&format!("capture {name} save failed: {err}")),
                    }
                    self.capture_stage += 1;
                    self.schedule_next_capture(&names);
                }
                Ok(Err(err)) => {
                    crate::logging::log(&format!("capture {name} failed: {err}"));
                    self.capture_stage += 1;
                    self.schedule_next_capture(&names);
                }
                Err(TryRecvError::Empty) => {
                    self.pending_capture = Some((name, path, receiver));
                    self.capture_next = Some(Instant::now() + Duration::from_millis(40));
                }
                Err(TryRecvError::Disconnected) => {
                    crate::logging::log(&format!("capture {name} failed: WebView2 disconnected"));
                    self.capture_stage += 1;
                    self.schedule_next_capture(&names);
                }
            }
            return;
        }
        let name = names[self.capture_stage].to_string();
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("screenshots").join(format!("{name}.png"));
        let Some(webview) = &self.webview else {
            self.exit_requested = true;
            return;
        };
        match crate::screenshot::capture_async(webview) {
            Ok(receiver) => {
                self.pending_capture = Some((name, path, receiver));
                self.capture_next = Some(Instant::now() + Duration::from_millis(40));
            }
            Err(err) => {
                crate::logging::log(&format!("capture {name} start failed: {err}"));
                self.capture_stage += 1;
                self.schedule_next_capture(&names);
            }
        }
    }

    /// The renderer silently falls back to the vector crosshair when the
    /// configured image is gone. Warn once per missing path so a deleted or
    /// moved file does not look like a broken crosshair.
    fn poll_image_state(&mut self) {
        if self.last_image_check.is_some_and(|at| at.elapsed() < Duration::from_secs(1)) {
            return;
        }
        self.last_image_check = Some(Instant::now());
        let missing = self
            .config
            .crosshair
            .image
            .clone()
            .filter(|path| !Path::new(path.as_str()).exists());
        if missing == self.warned_missing_image {
            return;
        }
        self.warned_missing_image = missing.clone();
        if let Some(path) = missing {
            crate::logging::log(&format!("image: configured image is missing; vector fallback shown ({path})"));
            let name = Path::new(&path)
                .file_name()
                .and_then(|value| value.to_str())
                .unwrap_or("The crosshair image")
                .to_string();
            self.emit(IpcEvent::DiagnosticBanner {
                code: "image".into(),
                message: format!("{name} is no longer available, so the vector crosshair is shown instead. Re-import the image or pick a new one."),
                recoverable: true,
            });
        }
    }

    fn poll_stress(&mut self) {
        if !self.stress_mode || self.stress_done || !self.ui_ready {
            return;
        }
        if !self.stress_started {
            self.stress_started = true;
            let baseline = crate::qa::Metrics::sample();
            self.stress_baseline = Some(baseline);
            crate::logging::log(&format!("stress: baseline {}", crate::qa::describe(baseline)));
            self.stress_deadline = Some(Instant::now() + Duration::from_secs(150));
            if let Err(err) = self.evaluate(STRESS_JS) {
                self.finish_stress(
                    false,
                    vec![format!("could not evaluate the stress driver: {err}")],
                    serde_json::Value::Null,
                    serde_json::Value::Null,
                );
            }
            return;
        }
        // Exercise the OSD window and its independent timer repeatedly, so the
        // handle/working-set verdict covers the pill and not just the crosshair.
        if self.stress_toast_at.elapsed() >= Duration::from_secs(2) {
            self.stress_toast_at = Instant::now();
            self.stress_toasts += 1;
            self.toast(format!("Stress toast {}", self.stress_toasts));
        }
        if self.stress_deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            self.finish_stress(
                false,
                vec!["timed out waiting for the stress report".into()],
                serde_json::Value::Null,
                serde_json::Value::Null,
            );
        }
    }

    fn accept_stress_report(&mut self, report_text: &str) {
        if self.stress_done {
            return;
        }
        let parsed: serde_json::Value = serde_json::from_str(report_text).unwrap_or_else(|err| {
            serde_json::json!({ "payload": { "ok": false, "errors": [format!("unreadable stress report: {err}")] } })
        });
        let payload = parsed.get("payload").cloned().unwrap_or(serde_json::Value::Null);
        let mut errors: Vec<String> = payload
            .get("errors")
            .and_then(|value| value.as_array())
            .map(|list| list.iter().filter_map(|value| value.as_str().map(str::to_string)).collect())
            .unwrap_or_default();
        let phase = payload.get("phase").and_then(|value| value.as_str()).unwrap_or("final");
        if phase == "baseline" {
            // Warm baseline: one-time allocations from the first cycle (surfaces,
            // caches, WebView handles) must not read as a leak in the second.
            let warm = crate::qa::Metrics::sample();
            self.stress_baseline = Some(warm);
            self.stress_deadline = Some(Instant::now() + Duration::from_secs(150));
            crate::logging::log(&format!(
                "stress: warm baseline {} (page errors so far: {})",
                crate::qa::describe(warm),
                errors.len()
            ));
            return;
        }
        if payload.get("ok").and_then(|value| value.as_bool()) != Some(true) && errors.is_empty() {
            errors.push("the page driver reported failure without an error".into());
        }
        self.finish_stress(
            errors.is_empty(),
            errors,
            payload.get("checks").cloned().unwrap_or(serde_json::Value::Null),
            payload.get("debug").cloned().unwrap_or(serde_json::Value::Null),
        );
    }

    fn finish_stress(&mut self, page_ok: bool, errors: Vec<String>, checks: serde_json::Value, debug: serde_json::Value) {
        if self.stress_done {
            return;
        }
        self.stress_done = true;
        let baseline = self.stress_baseline.unwrap_or_default();
        let final_sample = crate::qa::Metrics::sample();
        let growth = final_sample.growth_over(baseline);
        let leak_ok = crate::qa::growth_within_budget(growth);
        let ok = page_ok && leak_ok;
        let report = serde_json::json!({
            "ok": ok,
            "pageOk": page_ok,
            "leakOk": leak_ok,
            "errors": errors,
            "checks": checks,
            "debug": debug,
            "toastRequests": self.stress_toasts,
            "baseline": baseline.as_json(),
            "finalSample": final_sample.as_json(),
            "growth": growth.as_json(),
        });
        let path = crate::config::sandbox_root()
            .unwrap_or_else(crate::config::data_dir)
            .join("stress-result.json");
        if let Err(err) = std::fs::write(&path, serde_json::to_string_pretty(&report).unwrap_or_default()) {
            crate::logging::log(&format!("stress: cannot write {}: {err}", path.display()));
        }
        crate::logging::log(&format!("stress: {report}"));
        crate::logging::log(&format!("stress: growth {}", crate::qa::describe(growth)));
        if !ok {
            crate::qa::mark_failed();
        }
        self.exit_requested = true;
    }

    fn start_fx_verify(&mut self) {
        if !self.fx_verify_mode || self.fx_verify_started {
            return;
        }
        self.fx_verify_started = true;
        let budget = if self.fx_verify_live { 120 } else { 75 };
        self.fx_verify_deadline = Some(Instant::now() + Duration::from_secs(budget));
        let live_path = crate::config::live_config_path();
        match fx_verify_expected_audio(&live_path) {
            Ok((audio, warnings)) => {
                self.fx_verify_expected = Some(audio);
                self.fx_verify_expected_path = Some(live_path.clone());
                self.fx_verify_expected_warnings = warnings;
                crate::logging::log(&format!(
                    "fxverify: comparing the live audio config at {} with the existing status.json; no launch or CLI",
                    live_path.display()
                ));
            }
            Err(err) => {
                self.fail_fx_verify(&format!("could not read the live audio config {}: {err}", live_path.display()));
                return;
            }
        }
        if let Err(err) = self.evaluate(FX_VERIFY_JS) {
            self.fail_fx_verify(&format!("could not evaluate FxSound verifier: {err}"));
        }
    }

    fn accept_fx_verify_report(&mut self, report_text: &str) {
        if self.fx_verify_done || self.fx_verify_report.is_some() {
            return;
        }
        let report: serde_json::Value = serde_json::from_str(report_text).unwrap_or_else(|err| serde_json::json!({ "parseError": err.to_string() }));
        let payload = report.get("payload").cloned().unwrap_or(serde_json::Value::Null);
        if payload.get("ok").and_then(|v| v.as_bool()) != Some(true) {
            self.fail_fx_verify(payload.get("error").and_then(|v| v.as_str()).unwrap_or("the page verifier failed"));
            return;
        }
        self.fx_verify_report = Some(payload);
        self.fx_verify_deadline = Some(Instant::now() + Duration::from_secs(15));
        self.fx_verify_next = Instant::now();
        crate::logging::log("fxverify: page report received; reading the existing status.json");
    }

    fn poll_fx_verify(&mut self) {
        if !self.fx_verify_mode || self.fx_verify_done {
            return;
        }
        if self.fx_verify_report.is_none() {
            if self.fx_verify_deadline.map(|d| Instant::now() >= d).unwrap_or(false) {
                self.fail_fx_verify("timed out waiting for the FxSound page verifier");
            }
            return;
        }
        if Instant::now() < self.fx_verify_next {
            return;
        }
        self.fx_verify_next = Instant::now() + Duration::from_millis(450);
        let Some(expected) = self.fx_verify_expected.clone() else {
            self.fail_fx_verify("the live audio config was not loaded");
            return;
        };
        let actual = match crate::audio::fxsound::read_status_json() {
            Ok(value) => value,
            Err(err) => {
                if self.fx_verify_deadline.map(|d| Instant::now() >= d).unwrap_or(false) {
                    self.fail_fx_verify(&format!("could not read FxSound status.json: {err}"));
                }
                return;
            }
        };
        // Sound off still has one honest expectation: FxSound must be powered
        // off. The comparison skips the retained user curve, effects and output
        // for that state, but it must not skip the power check itself.
        let comparison = compare_fx_status(&expected, &actual);
        let preset_report = crate::audio::fxsound::managed_preset_report();
        if comparison.get("ok").and_then(|v| v.as_bool()) == Some(true) {
            self.fx_verify_done = true;
            let output = serde_json::json!({
                "ok": true,
                "mode": "--verify-fxsound",
                "readOnly": true,
                "dataDir": config::data_dir().to_string_lossy(),
                "expectedConfig": self.fx_verify_expected_path.as_ref().map(|path| path.to_string_lossy().to_string()),
                "expectedWarnings": self.fx_verify_expected_warnings.clone(),
                "checks": comparison,
                "preset": preset_report,
                "steps": self.fx_verify_report.as_ref().and_then(|p| p.get("steps")).cloned(),
            });
            self.write_fx_verify_result(&output);
            crate::logging::log("fxverify: PASS - every expected FxSound field matched");
            self.exit_requested = true;
        } else {
            self.fx_verify_done = true;
            let output = serde_json::json!({
                "ok": false,
                "mode": "--verify-fxsound",
                "readOnly": true,
                "error": "existing status.json does not match the live audio config",
                "expectedConfig": self.fx_verify_expected_path.as_ref().map(|path| path.to_string_lossy().to_string()),
                "expectedWarnings": self.fx_verify_expected_warnings.clone(),
                "checks": comparison,
                "preset": preset_report,
                "steps": self.fx_verify_report.as_ref().and_then(|p| p.get("steps")).cloned(),
            });
            self.write_fx_verify_result(&output);
            crate::logging::log("fxverify: FAIL - existing status.json did not match the loaded audio config");
            self.exit_requested = true;
        }
    }

    fn fail_fx_verify(&mut self, reason: &str) {
        if self.fx_verify_done {
            return;
        }
        self.fx_verify_done = true;
        let output = serde_json::json!({
            "ok": false,
            "mode": "--verify-fxsound",
            "readOnly": true,
            "error": reason,
        });
        self.write_fx_verify_result(&output);
        crate::logging::log(&format!("fxverify: failed: {reason}"));
        self.exit_requested = true;
    }

    fn write_fx_verify_result(&mut self, output: &serde_json::Value) {
        if output.get("ok").and_then(|value| value.as_bool()) != Some(true) {
            self.exit_code = 1;
        }
        let path = config::sandbox_root().unwrap_or_else(config::data_dir).join("fx-verify-result.json");
        if let Err(err) = std::fs::write(&path, serde_json::to_string_pretty(output).unwrap_or_else(|_| "{}".into())) {
            crate::logging::log(&format!("fxverify: cannot write {}: {err}", path.display()));
        }
    }

    fn start_verify(&mut self) {
        if !self.verify_mode || self.verify_started {
            return;
        }
        self.verify_started = true;
        self.verify_deadline = Some(Instant::now() + Duration::from_secs(45));
        crate::logging::log("verify: driving custom-preset create/delete in the live WebView");
        if let Err(err) = self.evaluate(VERIFY_JS) {
            self.fail_verify(&format!("could not evaluate verification script: {err}"));
        }
    }

    fn poll_verify(&mut self) {
        if !self.verify_mode || self.verify_done || !self.verify_started {
            return;
        }
        if self.verify_deadline.map(|deadline| Instant::now() >= deadline).unwrap_or(false) {
            self.fail_verify("timed out waiting for the page verification report");
            return;
        }
        if self.verify_image_seeded && !self.verify_image_deleted {
            if self.verify_image_armed.load(Ordering::Relaxed) {
                self.verify_overlay_last_requests = self.overlay.state().render_requests;
                let live = config::crosshairs_dir().join("live-probe.png");
                match std::fs::remove_file(&live) {
                    Ok(()) => crate::logging::log("verify: deleted the live image after the page armed"),
                    Err(err) => crate::logging::log(&format!("verify: could not delete the live image: {err}")),
                }
                self.verify_image_deleted = true;
                if let Some(webview) = &self.webview {
                    if let Err(err) = webview.evaluate_script_with_callback(
                        "window.__crosshairLiveGone = true;",
                        |_| {},
                    ) {
                        crate::logging::log(&format!("verify: could not signal the live-image deletion: {err}"));
                    }
                }
            } else if self.verify_image_poll_at.map(|at| Instant::now() >= at).unwrap_or(true) {
                self.verify_image_poll_at = Some(Instant::now() + Duration::from_millis(100));
                let armed = self.verify_image_armed.clone();
                if let Some(webview) = &self.webview {
                    if let Err(err) = webview.evaluate_script_with_callback(
                        "!!window.__crosshairLiveArmed",
                        move |result| armed.store(result.contains("true"), Ordering::Relaxed),
                    ) {
                        crate::logging::log(&format!("verify: could not poll the live-image arming flag: {err}"));
                    }
                }
            }
        }
    }

    /// Compare the overlay's actual last input with the image operation the
    /// page just performed. The request counter alone only proves a wake-up;
    /// this checks the payload that the renderer would use.
    fn accept_verify_overlay(&mut self, report_text: &str) {
        if !self.verify_mode || self.verify_done {
            return;
        }
        let value: serde_json::Value = match serde_json::from_str(report_text) {
            Ok(value) => value,
            Err(err) => {
                crate::logging::log(&format!("verify: malformed overlay check: {err}"));
                return;
            }
        };
        let path = value
            .get("payload")
            .and_then(|payload| payload.get("path"))
            .and_then(|path| path.as_str())
            .unwrap_or("");
        let use_probe = config::crosshairs_dir().join("use-probe.png");
        let (expected_image, expected_name) = match path {
            "use" => (
                Some(use_probe.to_string_lossy().to_string()),
                Some("use-probe.png".to_string()),
            ),
            "remove" | "delete" | "vanished" => (None, None),
            other => {
                crate::logging::log(&format!("verify: unknown overlay-check path {other:?}"));
                return;
            }
        };
        let applied = self.overlay.applied();
        let actual_image = applied.as_ref().and_then(|input| input.config.image.clone());
        let actual_name = applied.as_ref().and_then(|input| input.config.image_name.clone());
        let requests = self.overlay.state().render_requests;
        let counted = requests > self.verify_overlay_last_requests;
        self.verify_overlay_last_requests = requests;
        let state_ok = applied.is_some()
            && actual_image == expected_image
            && actual_name == expected_name;
        let ok = counted && state_ok;
        self.verify_overlay_checks.push(serde_json::json!({
            "path": path,
            "ok": ok,
            "counted": counted,
            "requests": requests,
            "applied": applied.is_some(),
            "expectedImage": expected_image,
            "expectedName": expected_name,
            "actualImage": actual_image,
            "actualName": actual_name,
        }));
        if let Some(webview) = &self.webview {
            if let Err(err) = webview.evaluate_script_with_callback(
                "window.__crosshairOverlayChecked = (window.__crosshairOverlayChecked || 0) + 1;",
                |_| {},
            ) {
                crate::logging::log(&format!("verify: could not acknowledge the overlay check: {err}"));
            }
        }
        crate::logging::log(&format!(
            "verify: overlay path={path} ok={ok} counted={counted} applied={} image={actual_image:?} name={actual_name:?}",
            applied.is_some()
        ));
    }

    fn finish_verify(&mut self, report_text: &str) {
        if self.verify_done {
            return;
        }
        self.verify_done = true;
        // The failed pick's own save must have cleared the image before the
        // page saw FullState. Read before this final flush so a missing save
        // cannot be hidden by the verifier's own persistence.
        let persisted_before_flush = Config::load_from(&config::config_path());
        let image_cleared_persisted = persisted_before_flush.crosshair.image.is_none()
            && persisted_before_flush.crosshair.image_name.is_none();
        self.save_now();
        let report: serde_json::Value = match serde_json::from_str(report_text) {
            Ok(value) => value,
            Err(err) => serde_json::json!({ "parseError": err.to_string() }),
        };
        let payload = report.get("payload").cloned().unwrap_or(serde_json::Value::Null);
        let persisted = Config::load_from(&config::config_path());
        let persisted_ids = serde_json::json!({
            "crosshair": persisted.custom.crosshair.iter().map(|preset| preset.id.clone()).collect::<Vec<_>>(),
            "display": persisted.custom.display.iter().map(|preset| preset.id.clone()).collect::<Vec<_>>(),
            "audio": persisted.custom.audio.iter().map(|preset| preset.id.clone()).collect::<Vec<_>>(),
        });
        let persisted_empty = persisted.custom.crosshair.is_empty()
            && persisted.custom.display.is_empty()
            && persisted.custom.audio.is_empty();
        let steps = payload.get("steps").and_then(|value| value.as_array()).cloned().unwrap_or_default();
        let modules = ["crosshair", "display", "audio"];
        let steps_ok = modules.iter().all(|module| {
            steps.iter().any(|step| {
                step.get("module").and_then(|value| value.as_str()) == Some(*module)
                    && step.get("ok").and_then(|value| value.as_bool()) == Some(true)
            })
        });
        let page_ok = payload.get("ok").and_then(|value| value.as_bool()).unwrap_or(false);
        let empty_array = |name: &str| {
            payload
                .get(name)
                .and_then(|value| value.as_array())
                .map(|value| value.is_empty())
                .unwrap_or(false)
        };
        let errors_empty = empty_array("errors");
        let console_empty = empty_array("console");
        let setup_empty = empty_array("setup");
        let image_modules = ["image-vanished", "image-use", "image-remove", "image-delete"];
        let image_step_ok = image_modules.iter().all(|module| {
            steps.iter().any(|step| {
                step.get("module").and_then(|value| value.as_str()) == Some(*module)
                    && step.get("ok").and_then(|value| value.as_bool()) == Some(true)
            })
        });
        let image_order_ok = self.verify_image_steps.len() == 3
            && self.verify_image_steps[0] == "cleared"
            && self.verify_image_steps[1] == "repainted"
            && self.verify_image_steps[2] == "persisted";
        let overlay_paths = ["vanished", "use", "remove", "delete"];
        let overlay_checks_ok = self.verify_overlay_checks.len() == overlay_paths.len()
            && overlay_paths.iter().all(|path| {
                self.verify_overlay_checks.iter().any(|check| {
                    check.get("path").and_then(|value| value.as_str()) == Some(*path)
                        && check.get("ok").and_then(|value| value.as_bool()) == Some(true)
                })
            });
        let overlay_requested = !self.verify_overlay_checks.is_empty()
            && self.verify_overlay_checks.iter().all(|check| {
                check.get("counted").and_then(|value| value.as_bool()) == Some(true)
            });
        let ok = page_ok
            && steps_ok
            && image_step_ok
            && image_order_ok
            && overlay_requested
            && overlay_checks_ok
            && image_cleared_persisted
            && self.verify_image_seeded
            && errors_empty
            && console_empty
            && setup_empty
            && persisted_empty;
        let output = serde_json::json!({
            "ok": ok,
            "mode": "--verify-preset-delete",
            "dataDir": config::data_dir().to_string_lossy(),
            "configPath": config::config_path().to_string_lossy(),
            "persistedCustomPresetIds": persisted_ids,
            "imageSeeded": self.verify_image_seeded,
            "imageClearedPersisted": image_cleared_persisted,
            "imageOrder": self.verify_image_steps,
            "imageStep": image_step_ok,
            "overlayRequested": overlay_requested,
            "overlayChecksOk": overlay_checks_ok,
            "overlayChecks": self.verify_overlay_checks.clone(),
            "report": report,
        });
        self.write_verify_result(&output);
        crate::logging::log(&format!(
            "verify: page_ok={page_ok} steps_ok={steps_ok} image_step_ok={image_step_ok} image_order_ok={image_order_ok} image_cleared_persisted={image_cleared_persisted} overlay_requested={overlay_requested} overlay_checks_ok={overlay_checks_ok} image_seeded={} errors_empty={errors_empty} console_empty={console_empty} setup_empty={setup_empty} persisted_empty={persisted_empty}",
            self.verify_image_seeded
        ));
        self.exit_requested = true;
    }

    fn fail_verify(&mut self, reason: &str) {
        if self.verify_done {
            return;
        }
        self.verify_done = true;
        let output = serde_json::json!({
            "ok": false,
            "mode": "--verify-preset-delete",
            "dataDir": config::data_dir().to_string_lossy(),
            "configPath": config::config_path().to_string_lossy(),
            "error": reason,
        });
        self.write_verify_result(&output);
        crate::logging::log(&format!("verify: failed: {reason}"));
        self.exit_requested = true;
    }

    fn write_verify_result(&self, output: &serde_json::Value) {
        let path = config::sandbox_root().unwrap_or_else(config::data_dir).join("verify-result.json");
        let text = match serde_json::to_string_pretty(output) {
            Ok(text) => text,
            Err(err) => {
                crate::logging::log(&format!("verify: cannot serialize result: {err}"));
                String::from("{}")
            }
        };
        if let Err(err) = std::fs::write(&path, text) {
            crate::logging::log(&format!("verify: cannot write {}: {err}", path.display()));
        }
    }

    fn schedule_next_capture(&mut self, names: &[&str; 4]) {
        if self.capture_stage < names.len() {
            let tab = names[self.capture_stage];
            if let Err(err) = self.evaluate(&format!("window.__crosshair_setTab('{tab}');")) {
                crate::logging::log(&format!("capture: could not switch to {tab}: {err}"));
            }
            self.capture_next = Some(Instant::now() + Duration::from_millis(450));
        } else {
            self.capture_next = Some(Instant::now());
        }
    }

    fn save_now(&mut self) {
        if self.capture_mode {
            self.dirty = false;
            self.last_save = Instant::now();
            return;
        }
        if let Err(err) = self.config.save() {
            crate::logging::log(&format!("config save failed: {err}"));
            self.emit(IpcEvent::DiagnosticBanner {
                code: "config".into(),
                message: "Couldn't save your settings.".into(),
                recoverable: true,
            });
        }
        self.dirty = false;
        self.last_save = Instant::now();
    }

    fn mark_dirty(&mut self) {
        self.dirty = true;
    }

    /// A live image change must reach the overlay before the preview and the
    /// save, or the desktop crosshair keeps the previous image until an
    /// unrelated sync.
    fn image_changed(&mut self) {
        self.mark_dirty();
        self.sync_overlay();
        self.update_preview();
    }

    /// A failed pick or delete can leave the live image pointing at a file that
    /// no longer exists. Drop it, repaint the Aim preview, and persist before
    /// the caller publishes the refreshed state. Still-present files (for
    /// example a locked delete target) are deliberately left alone.
    fn drop_vanished_live_image(&mut self, name: &str) {
        if !config::clear_vanished_live_image(&mut self.config.crosshair, name) {
            return;
        }
        if self.verify_mode {
            self.verify_image_steps.push("cleared");
        }
        self.image_changed();
        if self.verify_mode {
            self.verify_image_steps.push("repainted");
        }
        self.save_now();
        if self.verify_mode {
            self.verify_image_steps.push("persisted");
        }
    }

    fn save_preset(&mut self, module: &str, name: &str, image_mode: bool) {
        let name = name.trim().chars().take(40).collect::<String>();
        if let Err(err) = actions::validate_preset_name(module, &name) {
            self.emit(IpcEvent::DiagnosticBanner { code: "preset".into(), message: err, recoverable: true });
            return;
        }
        let id = format!("{module}_{}", std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0));
        match module {
            "crosshair" => {
                let mut preset = config::CrosshairPreset { id: id.clone(), name, builtin: false, config: self.config.crosshair.clone() };
                preset.config.enabled = false;
                if !image_mode {
                    // A Shape-mode save owns only the shape. The user's current
                    // image stays independent, exactly like a built-in preset.
                    preset.config.image = None;
                    preset.config.image_name = None;
                }
                self.config.custom.crosshair.push(preset);
                self.config.crosshair.active_preset = Some(id);
            }
            "display" => {
                self.config.custom.display.push(config::DisplayPreset {
                    id: id.clone(),
                    name,
                    builtin: false,
                    values: self.config.display.global.clone(),
                });
                self.config.display.active_preset = Some(id);
            }
            "audio" => {
                self.config.custom.audio.push(config::AudioPreset {
                    id: id.clone(),
                    name,
                    builtin: false,
                    bands: self.config.audio.bands,
                    bass: self.config.audio.bass,
                    treble: self.config.audio.treble,
                    output_level: self.config.audio.output_level,
                    echo: self.config.audio.echo,
                    surround: self.config.audio.surround,
                    dynamic_boost: self.config.audio.dynamic_boost,
                    volume_leveling: self.config.audio.volume_leveling,
                    balance: self.config.audio.balance,
                    filter_q: self.config.audio.filter_q,
                    volume_guard: self.config.audio.volume_guard,
                });
                self.config.audio.active_preset = Some(id);
            }
            _ => {
                self.emit(IpcEvent::DiagnosticBanner { code: "preset".into(), message: "Unknown preset module.".into(), recoverable: true });
                return;
            }
        }
        self.mark_dirty();
        self.save_now();
        self.emit_full_state();
    }

    fn delete_preset(&mut self, module: &str, id: &str) {
        if let Err(err) = actions::validate_custom_preset(&self.config, module, id) {
            self.emit(IpcEvent::DiagnosticBanner { code: "preset".into(), message: err, recoverable: true });
            return;
        }
        if !self.config.remove_custom_preset(module, id) {
            self.emit(IpcEvent::DiagnosticBanner { code: "preset".into(), message: "Unknown custom preset.".into(), recoverable: true });
            return;
        }
        self.mark_dirty();
        self.save_now();
        self.emit_full_state();
    }

    /// Open a native picker on its own STA thread. The dialog is owned by the
    /// app window, so it sits above the always-on-top UI, and the event loop
    /// keeps running (hotkeys, tray, tabs) while it is open instead of blocking
    /// in the dialog's modal loop.
    fn spawn_pick(&mut self, kind: PickKind, slot_id: Option<String>) {
        if !self.modes().live_system() {
            crate::logging::log(&format!("sandbox: {} picker skipped; real dialog untouched", kind.label()));
            return;
        }
        if self.picker_open {
            crate::logging::log(&format!("picker: ignored duplicate request kind={}", kind.label()));
            return;
        }
        let Some(window) = &self.window else {
            self.emit(IpcEvent::DiagnosticBanner { code: "dialog".into(), message: "The window is not ready for a file picker yet.".into(), recoverable: true });
            return;
        };
        let hwnd = window.hwnd();
        self.picker_open = true;
        let proxy = self.proxy.clone();
        crate::logging::log(&format!("picker: open kind={} parent=0x{hwnd:X}", kind.label()));
        std::thread::spawn(move || {
            unsafe {
                let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
            }
            let parent = ParentWindow(hwnd);
            let dialog = rfd::FileDialog::new().set_parent(&parent);
            let dialog = match kind {
                PickKind::Image => dialog
                    .set_title("Choose a crosshair image")
                    .add_filter("Images", &["png", "jpg", "jpeg", "webp", "gif", "svg", "bmp", "ico"]),
                PickKind::Eq => dialog
                    .set_title("Import an EQ profile")
                    .add_filter("EQ profiles", &["txt", "eq", "cfg", "conf"]),
                PickKind::SlotExe => dialog.set_title("Choose a game executable").add_filter("Executable", &["exe"]),
                PickKind::BackupOpen => dialog
                    .set_title("Choose a Crosshair+ backup")
                    .add_filter("Crosshair+ backup", &["zip"]),
                PickKind::BackupSave => dialog
                    .set_title("Save a Crosshair+ backup")
                    .add_filter("Crosshair+ backup", &["zip"])
                    .set_file_name("CrosshairPlus-backup.zip"),
            };
            // A panicking dialog must still report back, or the app would keep
            // ignoring every later browse request. A dialog that never returns
            // is different: the picker stays marked open, the event loop stays
            // responsive, and a second dialog is deliberately not stacked.
            let picked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                if kind.saves_file() { dialog.save_file() } else { dialog.pick_file() }
            }));
            let path = match picked {
                Ok(path) => path,
                Err(_) => {
                    crate::logging::log(&format!("picker: dialog failed kind={}", kind.label()));
                    None
                }
            };
            unsafe { CoUninitialize() };
            match &path {
                Some(path) => crate::logging::log(&format!("picker: selected kind={} path={}", kind.label(), path.display())),
                None => crate::logging::log(&format!("picker: cancelled kind={}", kind.label())),
            }
            let _ = proxy.send_event(AppEvent::FilePicked { kind, slot_id, path });
        });
    }

    fn assign_slot_exe(&mut self, slot_id: Option<&str>, path: &Path) {
        let Some(slot_id) = slot_id else { return };
        if let Some(slot) = self.config.slots.list.iter_mut().find(|s| s.id == slot_id) {
            slot.target_exe = Some(path.to_string_lossy().to_string());
            self.mark_dirty();
            self.save_now();
            self.refresh_running_apps();
            self.emit_full_state();
        }
    }

    fn browse_image(&mut self) {
        self.spawn_pick(PickKind::Image, None);
    }

    fn import_image_path(&mut self, path: &Path) {
        if !self.modes().live_system() {
            crate::logging::log("sandbox: image import skipped; real files untouched");
            return;
        }
        if !image_extension_supported(path) {
            self.emit(IpcEvent::DiagnosticBanner {
                code: "image".into(),
                message: "Unsupported file. Use PNG, JPG, WebP, GIF, SVG or BMP.".into(),
                recoverable: true,
            });
            return;
        }
        match std::fs::metadata(path) {
            Ok(meta) if !import_size_allowed(meta.len()) => {
                crate::logging::log(&format!(
                    "image import: {} is {:.1} MB; the limit is {} MB",
                    path.display(),
                    meta.len() as f64 / (1024.0 * 1024.0),
                    config::MAX_IMPORT_BYTES / (1024 * 1024)
                ));
                self.emit(IpcEvent::DiagnosticBanner {
                    code: "image".into(),
                    message: format!("Image is too large. The limit is {} MB.", config::MAX_IMPORT_BYTES / (1024 * 1024)),
                    recoverable: true,
                });
                return;
            }
            Ok(_) => {}
            Err(err) => {
                crate::logging::log(&format!("image import: could not read {}: {err}", path.display()));
                self.emit(IpcEvent::DiagnosticBanner {
                    code: "image".into(),
                    message: "Couldn't open that image.".into(),
                    recoverable: true,
                });
                return;
            }
        }
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("crosshair").to_string();
        let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0);
        let dest = config::crosshairs_dir().join(format!("{stamp}-{}", name.replace(['/', '\\'], "_")));
        match std::fs::copy(path, &dest) {
            Ok(_) => {
                self.config.crosshair.image = Some(dest.to_string_lossy().to_string());
                self.config.crosshair.image_name = Some(name);
                self.image_changed();
                self.save_now();
                self.emit_full_state();
            }
            Err(err) => {
                crate::logging::log(&format!("image import: could not copy to {}: {err}", dest.display()));
                self.emit(IpcEvent::DiagnosticBanner {
                    code: "image".into(),
                    message: "Couldn't import that image.".into(),
                    recoverable: true,
                });
            }
        }
    }

    fn browse_eq(&mut self) {
        self.spawn_pick(PickKind::Eq, None);
    }

    fn create_backup_path(&mut self, path: &Path) {
        if !self.modes().live_system() {
            crate::logging::log("sandbox: backup create skipped; real files untouched");
            return;
        }
        let path = if path.extension().and_then(|ext| ext.to_str()).is_some_and(|ext| ext.eq_ignore_ascii_case("zip")) {
            path.to_path_buf()
        } else {
            path.with_extension("zip")
        };
        match backup::create_backup(&path, &self.config, &config::data_dir()) {
            Ok(preview) => {
                self.emit(IpcEvent::BackupResult {
                    ok: true,
                    message: format!("Backup saved: {} ({} images)", path.display(), preview.images),
                });
            }
            Err(err) => self.emit(IpcEvent::BackupResult { ok: false, message: err }),
        }
    }

    fn inspect_backup_path(&mut self, path: &Path) {
        match backup::inspect_backup(path) {
            Ok(preview) => {
                self.pending_backup = Some(path.to_path_buf());
                self.emit(IpcEvent::BackupPreview { preview });
            }
            Err(err) => self.emit(IpcEvent::BackupResult { ok: false, message: err }),
        }
    }

    fn restore_pending_backup(&mut self) {
        if !self.modes().live_system() {
            crate::logging::log("sandbox: backup restore skipped; real files untouched");
            return;
        }
        let Some(path) = self.pending_backup.clone() else {
            self.emit(IpcEvent::BackupResult { ok: false, message: "Choose a backup first.".into() });
            return;
        };
        match backup::restore_backup(&path, &mut self.config, &config::config_path(), &config::data_dir()) {
            Ok(report) => {
                self.pending_backup = None;
                self.selected_slot = self.config.slots.list.first().map(|slot| slot.id.clone());
                self.mark_dirty();
                self.save_now();
                self.apply_display();
                self.apply_audio();
                self.sync_overlay();
                self.update_preview();
                self.emit_full_state();
                let message = if report.warnings.is_empty() {
                    format!(
                        "Backup restored ({} custom presets, {} images).",
                        report.preview.crosshair_presets + report.preview.display_presets + report.preview.audio_presets,
                        report.preview.images
                    )
                } else {
                    format!("Backup restored with {} warning(s): {}", report.warnings.len(), report.warnings[0])
                };
                self.emit(IpcEvent::BackupResult { ok: true, message });
            }
            Err(err) => self.emit(IpcEvent::BackupResult { ok: false, message: err }),
        }
    }

    fn undo_last_restore(&mut self) {
        if !self.modes().live_system() {
            crate::logging::log("sandbox: backup undo skipped; real files untouched");
            return;
        }
        match backup::undo_last_restore(&config::config_path(), &config::data_dir()) {
            Ok((reverted, warnings)) => {
                self.config = reverted;
                self.selected_slot = self.config.slots.list.first().map(|slot| slot.id.clone());
                self.mark_dirty();
                self.save_now();
                self.apply_display();
                self.apply_audio();
                self.sync_overlay();
                self.update_preview();
                self.emit_full_state();
                self.emit(IpcEvent::BackupResult {
                    ok: true,
                    message: "Restore undone; the settings from before the restore are back.".into(),
                });
                // Emitted last so the single toast slot shows the cleanup report,
                // exactly as after a config load; the undo confirmation still
                // lands in the backup status line and closes the modal.
                if let Some(event) = cleanup_warning_banner(&warnings) {
                    self.emit(event);
                }
            }
            Err(err) => self.emit(IpcEvent::BackupResult { ok: false, message: err }),
        }
    }

    fn import_eq(&mut self, path: Option<String>, text: Option<String>, name: Option<String>) {
        let (content, label) = if let Some(text) = text {
            (text, name.unwrap_or_else(|| "Pasted profile".into()))
        } else if let Some(path) = path {
            let p = PathBuf::from(&path);
            match std::fs::read_to_string(&p) {
                Ok(content) => {
                    let label = name.unwrap_or_else(|| p.file_stem().and_then(|s| s.to_str()).unwrap_or("Imported profile").to_string());
                    (content, label)
                }
                Err(err) => {
                    self.emit(IpcEvent::DiagnosticBanner { code: "eq".into(), message: format!("Could not read that profile: {err}"), recoverable: true });
                    return;
                }
            }
        } else {
            self.browse_eq();
            return;
        };
        match crate::audio::apo_import::parse(&content, &label) {
            Ok(profile) => {
                self.config.audio.imported = Some(profile);
                self.mark_dirty();
                self.save_now();
                self.apply_audio();
                self.emit_full_state();
            }
            Err(err) => self.emit(IpcEvent::DiagnosticBanner { code: "eq".into(), message: err, recoverable: true }),
        }
    }

    fn update_meter_state(&mut self) {
        let visible = self.window.as_ref().map(|w| w.is_visible()).unwrap_or(false);
        self.audio.set_meter_enabled(self.tab == "audio" && visible);
    }

    fn show_window(&mut self) {
        self.hide_tray_panel();
        if let Some(window) = &self.window {
            let _ = window.set_visible(true);
            let _ = window.set_focus();
        }
        if let Some(webview) = &self.webview {
            let _ = webview.set_visible(true);
        }
        self.update_meter_state();
    }

    fn hide_window(&mut self) {
        if let Some(window) = &self.window {
            let _ = window.set_visible(false);
        }
        self.audio.set_meter_enabled(false);
    }

    fn toggle_tray_panel(&mut self, position: PhysicalPosition<f64>, rect: tray_icon::Rect) {
        let open = self.tray_window.as_ref().and_then(|window| window.is_visible().then_some(())).is_some();
        if open {
            self.hide_tray_panel();
        } else {
            self.show_tray_panel(position, rect);
        }
    }

    fn show_tray_panel(&mut self, position: PhysicalPosition<f64>, rect: tray_icon::Rect) {
        self.tray_owns_foreground = false;
        let Some(window) = &self.tray_window else { return };
        let height = tray_panel_height(self.config.slots.list.len());
        self.tray_height = height;
        let anchor = TrayAnchor {
            x: position.x,
            y: position.y,
            rect_x: rect.position.x as f64,
            rect_y: rect.position.y as f64,
            rect_w: rect.size.width as f64,
            rect_h: rect.size.height as f64,
        };
        self.tray_anchor = Some(anchor);
        // Position first: moving onto the target monitor can make Windows apply
        // a DPI-suggested size, which must not win over the exact page height.
        position_tray_panel(window, anchor, TRAY_PANEL_WIDTH, height);
        let _ = window.set_inner_size(PhysicalSize::new(TRAY_PANEL_WIDTH as u32, height as u32));
        let _ = window.set_visible(true);
        let _ = window.set_focus();
        if let Some(webview) = &self.tray_webview {
            let _ = webview.set_visible(true);
        }
        unsafe {
            let _ = SetForegroundWindow(HWND(window.hwnd() as *mut c_void));
        }
        self.send_tray_state();
    }

    fn hide_tray_panel(&mut self) {
        if let Some(window) = &self.tray_window {
            let _ = window.set_visible(false);
        }
        if let Some(webview) = &self.tray_webview {
            let _ = webview.set_visible(false);
        }
        self.tray_anchor = None;
        self.tray_owns_foreground = false;
    }

    /// Keep the flyout exactly as tall as its rows when slots are added or
    /// removed, and re-clamp it against the tray edge if it is already open.
    fn sync_tray_geometry(&mut self) {
        let height = tray_panel_height(self.config.slots.list.len());
        if (height - self.tray_height).abs() < 0.5 {
            return;
        }
        self.tray_height = height;
        let Some(window) = &self.tray_window else { return };
        if window.is_visible() {
            if let Some(anchor) = self.tray_anchor {
                position_tray_panel(window, anchor, TRAY_PANEL_WIDTH, height);
            }
        }
        let _ = window.set_inner_size(PhysicalSize::new(TRAY_PANEL_WIDTH as u32, height as u32));
    }

    fn send_tray_state(&self) {
        let Some(webview) = &self.tray_webview else { return };
        let modules = crate::tray::module_entries(&self.config);
        let slots = crate::tray::slot_entries(&self.config);
        let state = serde_json::json!({
            "modules": {
                "crosshair": modules[0].enabled,
                "display": modules[1].enabled,
                "audio": modules[2].enabled,
            },
            "slots": slots,
            "selectedSlot": self.config.slots.active,
        });
        if let Ok(text) = serde_json::to_string(&state) {
            let _ = webview.evaluate_script(&format!("window.__crosshairTrayState({text});"));
        }
    }

    fn evaluate(&self, script: &str) -> Result<(), wry::Error> {
        if let Some(webview) = &self.webview {
            webview.evaluate_script(script)
        } else {
            Ok(())
        }
    }

    fn emit(&self, event: IpcEvent) {
        if let Err(err) = self.evaluate(&ipc::to_script(event)) {
            crate::logging::log(&format!("ipc: UI event delivery failed: {err}"));
        }
    }

    fn emit_full_state(&mut self) {
        self.send_full_state();
    }

    fn send_full_state(&mut self) {
        self.fx = self.audio.status();
        let effective = dsp_math::effective_bands(&self.config.audio);
        let curve = dsp_math::curve_db(&effective, 48000.0, 160);
        let payload = FullStatePayload {
            config: self.config.clone(),
            monitors: self.monitors.clone(),
            fx_sound: self.fx.clone(),
            audio_devices: self.audio_devices.clone(),
            crosshair_presets: self.config.all_crosshair_presets(),
            display_presets: self.config.all_display_presets(),
            audio_presets: self.config.all_audio_presets(),
            app_version: config::APP_VERSION.into(),
            selected_slot: self.selected_slot.clone().or_else(|| self.config.slots.active.clone()),
            data_dir: config::data_dir().to_string_lossy().to_string(),
            running_apps: self.running_apps.clone(),
            installed_games: self.installed_games.clone(),
            audio_curve: curve,
            master_gain: dsp_math::master_gain(&self.config.audio),
            launch_at_login: if self.modes().live_system() {
                startup_enabled_live()
            } else {
                self.config.settings.start_with_windows
            },
            can_undo_restore: backup::restore_undo_available(&config::data_dir()),
            crosshair_images: config::recent_crosshair_images(),
        };
        self.emit(IpcEvent::FullState(Box::new(payload)));
        self.sync_tray_geometry();
        self.send_tray_state();
    }

    fn shutdown(&mut self) {
        if self.exited {
            return;
        }
        self.exited = true;
        if slots::deactivate_slot(&mut self.config) {
            self.mark_dirty();
            crate::logging::log("slots: cleared active slot on clean shutdown");
        }
        self.save_now();
        if self.modes().live_system() {
            safety::restore_for_exit(self.config.settings.restore_display_on_exit);
        }
        self.compare_active = false;
        if self.audio_compare_active {
            self.audio_compare_active = false;
            if self.modes().live_system() {
                self.audio.apply(self.config.audio.clone());
            }
        }
        self.audio.shutdown();
        self.preview.shutdown();
        self.display.shutdown();
        self.overlay.shutdown();
        if self.verify_mode && !self.verify_done {
            self.fail_verify("app exited before the page verification report arrived");
        }
        if self.fx_verify_mode && !self.fx_verify_done {
            self.fail_fx_verify("app exited before the FxSound verification completed");
        }
        crate::logging::log("=== Crosshair+ stopped ===");
    }
}

/// The verifier runs against a sandbox, so its expectation must come from the
/// real `%APPDATA%\CrosshairPlus\config.json`. A strict load keeps this
/// read-only and refuses to silently fall back to defaults when the file is
/// missing or unreadable.
fn fx_verify_expected_audio(path: &Path) -> Result<(crate::config::AudioConfig, Vec<String>), String> {
    crate::config::Config::load_strict(path).map(|(config, warnings)| (config.audio, warnings))
}

pub(crate) fn compare_fx_status(expected: &crate::config::AudioConfig, actual: &serde_json::Value) -> serde_json::Value {
    crate::audio::verify::compare_fx_status(expected, actual)
}

fn capture_config() -> Config {
    let mut config = Config::default();
    config.slots.active = Some("slot-fps".into());
    config.crosshair.enabled = true;
    config.crosshair.style = "plus".into();
    config.crosshair.size = 17.0;
    config.crosshair.thickness = 2.0;
    config.crosshair.gap = 3.0;
    config.crosshair.dot = false;
    config.crosshair.dot_size = 2.0;
    config.crosshair.outline = true;
    config.crosshair.outline_width = 1.0;
    config.crosshair.opacity = 100.0;
    config.crosshair.color = "#00FF66".into();
    config.crosshair.active_preset = Some("style_plus".into());
    config.display.enabled = true;
    config.display.mode = "all".into();
    config.display.global.panel = "oled".into();
    config.display.active_preset = Some("reference_neutral".into());
    // Keep the fixture internally consistent with the active Competitive slot,
    // which includes the Sound module.
    config.audio.enabled = true;
    config.audio.active_preset = Some("fps_footsteps".into());
    config.audio.bands = [-4.0, -3.5, -2.5, -1.0, 0.0, 1.5, 3.5, 4.5, 3.0, 1.0];
    config.audio.bass = 0.0;
    config.audio.treble = 3.0;
    config.audio.filter_q = 1.5;
    config.settings.preview_day = true;
    config.settings.start_minimized = false;
    config
}

fn backup_config() {
    let path = config::config_path();
    if path.exists() {
        let backup = config::data_dir().join("config.pre-crosshair-plus.json");
        if !backup.exists() {
            if let Err(err) = std::fs::copy(&path, &backup) {
                crate::logging::log(&format!("config backup failed: {err}"));
            } else {
                crate::logging::log(&format!("config: backed up prior state to {}", backup.display()));
            }
        }
    }
}

/// The evergreen runtime registers its version under one of these keys. A
/// missing runtime is the most common reason a window never appears, so the
/// check runs before the builder and produces a real message instead of a
/// silent exit with code 0.
fn webview2_preflight_error() -> Option<String> {
    const CLIENT: &str = "Microsoft\\EdgeUpdate\\Clients\\{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}";
    let machine = format!("SOFTWARE\\WOW6432Node\\{CLIENT}");
    let user = format!("Software\\{CLIENT}");
    let installed = read_reg_string(HKEY_LOCAL_MACHINE, &machine, "pv")
        .or_else(|| read_reg_string(HKEY_LOCAL_MACHINE, CLIENT, "pv"))
        .or_else(|| read_reg_string(HKEY_CURRENT_USER, &user, "pv"))
        .is_some_and(|version| !version.trim().is_empty());
    if installed {
        None
    } else {
        Some("Microsoft Edge WebView2 Runtime was not found on this computer.".into())
    }
}

fn show_startup_error(detail: &str) {
    if crate::config::sandbox_root().is_some() {
        return;
    }
    let message = format!(
        "Crosshair+ could not create its window.\n\n{detail}\n\nWebView2 Runtime may be missing or broken. Install it from https://go.microsoft.com/fwlink/p/?LinkId=2124703 and start Crosshair+ again.\n\nLog: {}",
        crate::config::log_path().display()
    );
    let message_wide: Vec<u16> = message.encode_utf16().chain(Some(0)).collect();
    let title_wide = wide("Crosshair+");
    unsafe {
        let _ = MessageBoxW(
            None,
            PCWSTR(message_wide.as_ptr()),
            PCWSTR(title_wide.as_ptr()),
            MB_ICONERROR | MB_OK,
        );
    }
}

fn apply_dwm(window: &Window) {
    let hwnd = HWND(window.hwnd() as *mut c_void);
    let dark: i32 = 1;
    let caption: u32 = 0x000000;
    let (dark_ok, caption_ok) = unsafe {
        let dark_ok = DwmSetWindowAttribute(
            hwnd,
            DWMWA_USE_IMMERSIVE_DARK_MODE,
            &dark as *const _ as *const c_void,
            std::mem::size_of::<i32>() as u32,
        )
        .is_ok();
        let caption_ok = DwmSetWindowAttribute(
            hwnd,
            DWMWA_CAPTION_COLOR,
            &caption as *const _ as *const c_void,
            std::mem::size_of::<u32>() as u32,
        )
        .is_ok();
        (dark_ok, caption_ok)
    };
    crate::logging::log(&format!("dwm: dark={dark_ok} caption={caption_ok}"));
}

const TRAY_PANEL_WIDTH: f64 = 272.0;
const TRAY_PANEL_PADDING: f64 = 12.0;
const TRAY_PANEL_BORDER: f64 = 1.0;
const TRAY_HEADER_HEIGHT: f64 = 28.0;
const TRAY_HEADER_GAP: f64 = 8.0;
const TRAY_MODULE_HEIGHT: f64 = 38.0;
const TRAY_MODULE_GAP: f64 = 4.0;
const TRAY_MODULES_PAD: f64 = 10.0;
const TRAY_DIVIDER: f64 = 1.0;
const TRAY_SLOTS_MARGIN: f64 = 10.0;
const TRAY_SLOT_HEADING: f64 = 16.0;
const TRAY_SLOT_HEAD_GAP: f64 = 6.0;
const TRAY_SLOT_HEIGHT: f64 = 34.0;
const TRAY_SLOT_GAP: f64 = 4.0;
const TRAY_EMPTY_HEIGHT: f64 = 28.0;
const TRAY_FOOTER_MARGIN: f64 = 10.0;
const TRAY_FOOTER_PAD: f64 = 8.0;
const TRAY_ACTION_HEIGHT: f64 = 34.0;
const TRAY_ACTION_GAP: f64 = 2.0;

#[derive(Clone, Copy)]
pub(crate) struct TrayAnchor {
    x: f64,
    y: f64,
    rect_x: f64,
    rect_y: f64,
    rect_w: f64,
    rect_h: f64,
}

/// The flyout uses fixed row heights, so its window height is a pure function
/// of the slot count. Keep these constants in step with ui/tray.css.
fn tray_panel_height(slots: usize) -> f64 {
    let slots = slots.min(crate::config::MAX_SLOTS);
    let rows = if slots == 0 {
        TRAY_EMPTY_HEIGHT
    } else {
        TRAY_SLOT_HEIGHT * slots as f64 + TRAY_SLOT_GAP * (slots - 1) as f64
    };
    (TRAY_PANEL_PADDING + TRAY_PANEL_BORDER) * 2.0
        + TRAY_HEADER_HEIGHT
        + TRAY_HEADER_GAP
        + TRAY_MODULE_HEIGHT * 3.0
        + TRAY_MODULE_GAP * 2.0
        + TRAY_MODULES_PAD
        + TRAY_DIVIDER
        + TRAY_SLOTS_MARGIN
        + TRAY_SLOT_HEADING
        + TRAY_SLOT_HEAD_GAP
        + rows
        + TRAY_FOOTER_MARGIN
        + TRAY_DIVIDER
        + TRAY_FOOTER_PAD
        + TRAY_ACTION_HEIGHT * 3.0
        + TRAY_ACTION_GAP * 2.0
}

fn position_tray_panel(window: &Window, anchor: TrayAnchor, panel_w: f64, panel_h: f64) {
    let mut x = if anchor.rect_w > 0.0 {
        anchor.rect_x + anchor.rect_w - panel_w
    } else {
        anchor.x + 8.0
    };
    let mut y = if anchor.rect_h > 0.0 {
        anchor.rect_y - panel_h - 6.0
    } else {
        anchor.y - panel_h
    };
    let (left, top, right, bottom) = work_area_for_point(PhysicalPosition::new(anchor.x, anchor.y));
    x = x.clamp(left, (right - panel_w).max(left));
    y = y.clamp(top, (bottom - panel_h).max(top));
    let _ = window.set_outer_position(PhysicalPosition::new(x.round() as i32, y.round() as i32));
}

fn work_area_for_point(position: PhysicalPosition<f64>) -> (f64, f64, f64, f64) {
    let point = POINT { x: position.x.round() as i32, y: position.y.round() as i32 };
    let monitor = unsafe { MonitorFromPoint(point, MONITOR_DEFAULTTONEAREST) };
    let mut info = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    if unsafe { GetMonitorInfoW(monitor, &mut info as *mut _ as *mut MONITORINFO).as_bool() } {
        (
            info.rcWork.left as f64,
            info.rcWork.top as f64,
            info.rcWork.right as f64,
            info.rcWork.bottom as f64,
        )
    } else {
        (0.0, 0.0, 1920.0, 1080.0)
    }
}

fn window_dimensions(monitor_width: u32, monitor_height: u32) -> (u32, u32) {
    let max_w = monitor_width as f64 * 0.9;
    let max_h = monitor_height as f64 * 0.9;
    (1200.0f64.min(max_w).max(1040.0) as u32, 780.0f64.min(max_h).max(680.0) as u32)
}

fn run_key_value(exe: &Path) -> String {
    format!("\"{}\" --minimized", exe.display())
}

const RUN_KEY_SUBKEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Run";

fn run_key_subkey() -> &'static str {
    RUN_KEY_SUBKEY
}

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(Some(0)).collect()
}

const STARTUP_APPROVED_SUBKEY: &str =
    "Software\\Microsoft\\Windows\\CurrentVersion\\Explorer\\StartupApproved\\Run";

fn read_reg_string(root: HKEY, subkey: &str, name: &str) -> Option<String> {
    let subkey = wide(subkey);
    let name = wide(name);
    let mut size = 0u32;
    let status = unsafe {
        RegGetValueW(root, PCWSTR(subkey.as_ptr()), PCWSTR(name.as_ptr()), RRF_RT_REG_SZ, None, None, Some(&mut size))
    };
    if status != ERROR_SUCCESS || size == 0 {
        return None;
    }
    let mut data = vec![0u16; (size as usize / 2).max(1)];
    let status = unsafe {
        RegGetValueW(
            root,
            PCWSTR(subkey.as_ptr()),
            PCWSTR(name.as_ptr()),
            RRF_RT_REG_SZ,
            None,
            Some(data.as_mut_ptr() as *mut c_void),
            Some(&mut size),
        )
    };
    if status != ERROR_SUCCESS {
        return None;
    }
    let len = data.iter().position(|value| *value == 0).unwrap_or(data.len());
    Some(String::from_utf16_lossy(&data[..len]))
}

fn read_reg_binary(root: HKEY, subkey: &str, name: &str) -> Option<Vec<u8>> {
    let subkey = wide(subkey);
    let name = wide(name);
    let mut size = 0u32;
    let status = unsafe {
        RegGetValueW(root, PCWSTR(subkey.as_ptr()), PCWSTR(name.as_ptr()), RRF_RT_REG_BINARY, None, None, Some(&mut size))
    };
    if status != ERROR_SUCCESS || size == 0 {
        return None;
    }
    let mut data = vec![0u8; size as usize];
    let status = unsafe {
        RegGetValueW(
            root,
            PCWSTR(subkey.as_ptr()),
            PCWSTR(name.as_ptr()),
            RRF_RT_REG_BINARY,
            None,
            Some(data.as_mut_ptr() as *mut c_void),
            Some(&mut size),
        )
    };
    if status != ERROR_SUCCESS {
        return None;
    }
    Some(data)
}

/// Task Manager writes a 12-byte record per startup item; a first byte of 3
/// means the user disabled it without removing the Run value.
fn startup_approved_disabled() -> bool {
    read_reg_binary(HKEY_CURRENT_USER, STARTUP_APPROVED_SUBKEY, "CrosshairPlus")
        .and_then(|data| data.first().copied())
        .is_some_and(|state| state == 0x03)
}

fn apply_run_key(settings: &Settings) -> Result<(), String> {
    if crate::config::sandbox_root().is_some() {
        crate::logging::log("sandbox: Windows startup entry left untouched");
        return Ok(());
    }
    apply_run_key_at(settings, run_key_subkey())?;
    if settings.start_with_windows {
        // Re-enabling must clear a Task-Manager disable, or Windows keeps
        // suppressing the entry even though the Run value is correct.
        let approved = wide(STARTUP_APPROVED_SUBKEY);
        let name = wide("CrosshairPlus");
        unsafe {
            let _ = RegDeleteKeyValueW(HKEY_CURRENT_USER, PCWSTR(approved.as_ptr()), PCWSTR(name.as_ptr()));
        }
    }
    Ok(())
}

fn apply_run_key_at(settings: &Settings, subkey: &str) -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|err| format!("cannot resolve executable: {err}"))?;
    let value = run_key_value(&exe);
    let key = wide(subkey);
    let name = wide("CrosshairPlus");
    if settings.start_with_windows {
        let data = wide(&value);
        let result = unsafe {
            RegSetKeyValueW(
                HKEY_CURRENT_USER,
                PCWSTR(key.as_ptr()),
                PCWSTR(name.as_ptr()),
                1,
                Some(data.as_ptr() as *const c_void),
                (data.len() * 2) as u32,
            )
        };
        if result.is_err() {
            return Err(format!("RegSetKeyValueW failed: {result:?}"));
        }
        // Read the value back instead of trusting the write: a policy or a
        // redirected hive can accept the call and still not persist it.
        let stored = read_reg_string(HKEY_CURRENT_USER, subkey, "CrosshairPlus")
            .ok_or_else(|| "the Run value could not be read back".to_string())?;
        if !stored.eq_ignore_ascii_case(&value) {
            return Err(format!("the Run value read back as '{stored}'"));
        }
    } else {
        unsafe {
            let _ = RegDeleteKeyValueW(HKEY_CURRENT_USER, PCWSTR(key.as_ptr()), PCWSTR(name.as_ptr()));
        }
    }
    Ok(())
}

/// The launch-at-login state the user actually has, not the checkbox they set.
fn startup_enabled_live() -> bool {
    let Ok(exe) = std::env::current_exe() else { return false };
    let expected = run_key_value(&exe);
    let stored = read_reg_string(HKEY_CURRENT_USER, run_key_subkey(), "CrosshairPlus");
    stored.as_deref().is_some_and(|value| value.eq_ignore_ascii_case(&expected))
        && !startup_approved_disabled()
}

#[cfg(test)]
use windows::Win32::System::Registry::RegDeleteKeyW;

#[cfg(test)]
fn read_run_key_at(subkey: &str) -> Result<Option<String>, String> {
    let subkey = wide(subkey);
    let name = wide("CrosshairPlus");
    let mut size = 0u32;
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            PCWSTR(subkey.as_ptr()),
            PCWSTR(name.as_ptr()),
            RRF_RT_REG_SZ,
            None,
            None,
            Some(&mut size),
        )
    };
    if status != ERROR_SUCCESS {
        return Ok(None);
    }
    let mut data = vec![0u16; (size as usize / 2).max(1)];
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            PCWSTR(subkey.as_ptr()),
            PCWSTR(name.as_ptr()),
            RRF_RT_REG_SZ,
            None,
            Some(data.as_mut_ptr() as *mut c_void),
            Some(&mut size),
        )
    };
    if status != ERROR_SUCCESS {
        return Err(format!("RegGetValueW failed: {status:?}"));
    }
    let len = data.iter().position(|value| *value == 0).unwrap_or(data.len());
    Ok(Some(String::from_utf16_lossy(&data[..len])))
}

#[cfg(test)]
fn delete_test_run_key_at(subkey: &str) {
    let subkey = wide(subkey);
    unsafe {
        let _ = RegDeleteKeyW(HKEY_CURRENT_USER, PCWSTR(subkey.as_ptr()));
    }
}

/// The custom app protocol is a single exact origin. A prefix check would
/// accept lookalike hosts such as https://app.localhost.example.com.
fn navigation_allowed(url: &str) -> bool {
    let lower = url.to_ascii_lowercase();
    lower == "https://app.localhost"
        || lower.starts_with("https://app.localhost/")
        || lower.starts_with("https://app.localhost?")
        || lower.starts_with("https://app.localhost#")
}

fn open_folder(path: &Path) {
    match std::process::Command::new("explorer.exe").arg(path).spawn() {
        Ok(_) => crate::logging::log(&format!("data folder opened: {}", path.display())),
        Err(err) => crate::logging::log(&format!("data folder open failed: {err}")),
    }
}

fn open_path(path: &Path) {
    let target: Vec<u16> = path.to_string_lossy().encode_utf16().chain(Some(0)).collect();
    let verb: Vec<u16> = "open".encode_utf16().chain(Some(0)).collect();
    let result = unsafe {
        ShellExecuteW(
            None,
            PCWSTR(verb.as_ptr()),
            PCWSTR(target.as_ptr()),
            PCWSTR::null(),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        )
    };
    if result.0 as isize <= 32 {
        crate::logging::log(&format!("log open failed: ShellExecuteW returned {} for {}", result.0 as isize, path.display()));
    } else {
        crate::logging::log(&format!("log opened: {}", path.display()));
    }
}

/// The mode flags that decide whether a command may touch the real machine.
/// Capture is deliberately in this set: its display preview is generated in
/// memory, but there is no capture equivalent for FxSound, external launches or
/// the HKCU startup entry.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct RunModes {
    capture: bool,
    verify: bool,
    fx_verify: bool,
    interaction: bool,
    stress: bool,
    sandboxed: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DisplayPath {
    Preview,
    Skip,
    Live,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AudioPath {
    Skip,
    Live,
}

impl RunModes {
    fn new(capture: bool, verify: bool, fx_verify: bool, interaction: bool, stress: bool) -> Self {
        Self {
            capture,
            verify,
            fx_verify,
            interaction,
            stress,
            sandboxed: crate::config::sandbox_root().is_some(),
        }
    }

    fn automated(self) -> bool {
        self.capture || self.verify || self.fx_verify || self.interaction || self.stress
    }

    /// The single gate for every real-system side effect: display ramps,
    /// FxSound, tray, hotkeys, HKCU, process restarts and external launches.
    /// Harnesses and any sandboxed launch are excluded by construction.
    fn live_system(self) -> bool {
        !self.automated() && !self.sandboxed
    }
}

fn display_path(modes: RunModes) -> DisplayPath {
    if modes.capture {
        DisplayPath::Preview
    } else if modes.live_system() {
        DisplayPath::Live
    } else {
        DisplayPath::Skip
    }
}

fn audio_path(modes: RunModes) -> AudioPath {
    if modes.live_system() { AudioPath::Live } else { AudioPath::Skip }
}

fn startup_audio_eligible(fx: &FxSoundStatus) -> bool {
    fx.installed && fx.cli_ready && fx.running
}

/// A launch can beat FxSound's own startup. Only Sound being on, an installed
/// and automatable instance, and that instance merely not running yet are worth
/// waiting for.
fn startup_audio_deferred(enabled: bool, fx: &FxSoundStatus) -> bool {
    enabled && fx.installed && fx.cli_ready && !fx.running
}

/// Consume the deferred launch catch-up. Returns true at most once, when a
/// fresh status proves FxSound is ready; turning Sound off drops the flag.
fn deferred_audio_reconcile_due(pending: &mut bool, enabled: bool, fx: &FxSoundStatus) -> bool {
    if !*pending {
        return false;
    }
    if !enabled {
        *pending = false;
        return false;
    }
    if !startup_audio_eligible(fx) {
        return false;
    }
    *pending = false;
    true
}

fn apply_run_key_checked(settings: &Settings, modes: RunModes) -> Option<String> {
    if !modes.live_system() {
        crate::logging::log("sandbox: Windows startup entry left untouched");
        return None;
    }
    match apply_run_key(settings) {
        Ok(()) => None,
        Err(err) => {
            crate::logging::log(&format!("run key update failed: {err}"));
            Some("Windows startup could not be updated. Open Settings and toggle Launch with Windows again.".into())
        }
    }
}

/// Global hotkeys are registered on the real desktop, so only a live launch
/// may publish any. Stress and interaction still get the overlay and OSD, but
/// with an empty binding list.
/// Build the exact payload handed to the overlay thread. Kept separate so the
/// verify payload-boundary test covers the same construction as sync_overlay.
fn overlay_input(
    crosshair: &crate::config::CrosshairConfig,
    rect: Option<(i32, i32, u32, u32)>,
    hotkeys: Vec<crate::overlay::HotkeyBinding>,
    monitor_label: String,
) -> OverlayInput {
    OverlayInput { config: crosshair.clone(), rect, hotkeys, monitor_label }
}

fn active_hotkeys(modes: RunModes, config: &Config) -> Vec<crate::overlay::HotkeyBinding> {
    if modes.live_system() { hotkeys::bindings(config) } else { Vec::new() }
}

fn open_windows_display() {
    let target: Vec<u16> = "ms-settings:display".encode_utf16().chain(Some(0)).collect();
    let verb: Vec<u16> = "open".encode_utf16().chain(Some(0)).collect();
    let result = unsafe {
        ShellExecuteW(
            None,
            PCWSTR(verb.as_ptr()),
            PCWSTR(target.as_ptr()),
            PCWSTR::null(),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        )
    };
    if result.0 as isize <= 32 {
        crate::logging::log(&format!("windows display settings launch failed: ShellExecuteW returned {}", result.0 as isize));
    }
}

const FX_VERIFY_JS: &str = include_str!("../ui/harness/fx_verify.js");

const VERIFY_INIT_JS: &str = include_str!("../ui/harness/verify_init.js");

const VERIFY_JS: &str = include_str!("../ui/harness/verify.js");

/// Adversarial driver for `--stress`. It drives the real DOM handlers and the
/// real IPC bridge twice, drains the command queue with a FullState barrier,
/// then reports a warm baseline so leak growth is measured per cycle instead of
/// against cold-start allocations.
const STRESS_JS: &str = include_str!("../ui/harness/stress.js");
const SELFTEST_JS: &str = include_str!("../ui/harness/selftest.js");
const LAYOUT_JS: &str = r#"(() => {
  const errors = [];
  let settingsMetrics = null;
  const check = (name, ok) => { if (!ok) errors.push(name); };
  try {
    check('viewport', innerWidth === 1040 && innerHeight === 680);
    check('no-horizontal-overflow', document.documentElement.scrollWidth <= innerWidth + 1);
    const footer = document.querySelector('.statusbar')?.getBoundingClientRect();
    check('footer-bottom', !!footer && Math.abs(footer.bottom - innerHeight) <= 1);
    const tabs = document.querySelector('.tabs')?.getBoundingClientRect();
    check('tabs-centered', !!tabs && Math.abs((tabs.left + tabs.right) / 2 - innerWidth / 2) <= 1);
    check('display-visible', document.querySelector('#view-display')?.classList.contains('active'));
    const card = document.querySelector('#view-display .content .card')?.getBoundingClientRect();
    check('display-card-fits', !!card && card.left >= 0 && card.right <= innerWidth + 1);
    check('blue-light', !!document.querySelector('#blueLightToggle'));
    check('compare-display', !!document.querySelector('#compareDisplay'));
    document.querySelector('[data-action="settings"]')?.click();
    const modal = document.querySelector('#settingsModal .modal-card');
    settingsMetrics = modal ? { scroll: modal.scrollHeight, client: modal.clientHeight } : null;
    check('settings-open', !!modal && !document.querySelector('#settingsModal')?.classList.contains('hidden'));
    const box = modal?.getBoundingClientRect();
    check('settings-fits', !!box && box.left >= 0 && box.right <= innerWidth + 1 && box.bottom <= innerHeight + 1);
    check('settings-complete', !!modal && modal.scrollHeight <= modal.clientHeight + 1);
    const grid = document.querySelector('#settingsModal .maintenance-grid');
    const gridButtons = Array.from(grid?.querySelectorAll('.button') || []);
    const labels = gridButtons.map((button) => button.textContent.trim());
    const tops = gridButtons.map((button) => Math.round(button.getBoundingClientRect().top));
    check('settings-grid-3x2', !!grid && tops.filter((top) => top === tops[0]).length === 3 && new Set(tops).size === 2);
    check('settings-grid-labels', JSON.stringify(labels) === JSON.stringify(['Open FxSound', 'Fix audio', 'Reinstall FxSound', 'App Folder', 'Backups', 'Reset app']));
    check('settings-footer-link', document.querySelector('#settingsModal .modal-foot [data-action="open-log"]')?.previousElementSibling?.id === 'versionLabel');
    check('settings-save', document.querySelector('#settingsModal [data-action="save-settings"]')?.textContent.trim() === 'Save');
    check('settings-cycle-hotkey', !!document.querySelector('#setCycleHotkey'));
    check('settings-osd', !!document.querySelector('#setOsdToast'));
    check('settings-unstick', !!document.querySelector('#settingsModal [data-action="unstick-audio"]'));
    document.querySelector('#settingsModal [data-action="close-settings"]')?.click();
    const contentFits = (tab) => {
      window.__crosshair_setTab(tab);
      const content = document.querySelector('#view-' + tab + ' .content');
      return !!content && content.scrollHeight <= content.clientHeight + 1;
    };
    // A loaded image shows the option stack; a GIF plus tint/solid shows the
    // tallest variant. Force every conditional block visible for the measure so
    // an empty dropzone can never hide an overflow again.
    const imageWorstCaseFits = () => {
      const forced = ['#imageOptions', '#solidColorRow', '#gifOptions'].map((selector) => document.querySelector(selector));
      const classes = forced.map((el) => (el ? el.className : null));
      forced.forEach((el) => el?.classList.remove('hidden'));
      const fits = contentFits('aim');
      forced.forEach((el, index) => { if (el) el.className = classes[index]; });
      return fits;
    };
    window.__crosshair_setTab('aim');
    document.querySelector('[data-action="aim-mode"][data-value="shape"]')?.click();
    document.querySelector('[data-action="aim-visibility"][data-value="hide"]')?.click();
    const aimView = document.querySelector('#view-aim');
    const aimRail = aimView?.querySelector('.rail');
    const aimBehavior = document.querySelector('#aimVisibility')?.closest('.card');
    const aimRow = document.querySelector('#aimButtonRow');
    const aimSelect = document.querySelector('#aimButton');
    const aimSelectWrap = document.querySelector('#aimButtonSelect');
    const aimMenu = document.querySelector('#aimButtonMenu');
    const aimTrack = document.querySelector('#aimVisibility');
    const aimViewBox = aimView?.getBoundingClientRect();
    const aimRailBox = aimRail?.getBoundingClientRect();
    const aimBehaviorBox = aimBehavior?.getBoundingClientRect();
    const aimRowBox = aimRow?.getBoundingClientRect();
    const aimTrackBox = aimTrack?.getBoundingClientRect();
    check('aim-button-row-visible-1040', !!aimRow && !aimRow.classList.contains('hidden') && !!aimRowBox && aimRowBox.height > 0);
    check('aim-button-row-inside', !!aimRowBox && !!aimBehaviorBox && aimRowBox.left >= aimBehaviorBox.left - 1 && aimRowBox.right <= aimBehaviorBox.right + 1 && aimRowBox.bottom <= aimBehaviorBox.bottom + 1);
    check('aim-button-select-fits-1040', !!aimSelect && !!aimSelectWrap && !!aimBehaviorBox
      && aimSelectWrap.scrollWidth <= aimSelectWrap.clientWidth + 1
      && aimSelect.getBoundingClientRect().right <= aimBehaviorBox.right + 1
      && aimSelect.getBoundingClientRect().right <= aimSelectWrap.getBoundingClientRect().right + 1);
    aimSelect?.click();
    const aimMenuBox = aimMenu?.getBoundingClientRect();
    check('aim-button-menu-fits-1040', !!aimMenu && !aimMenu.classList.contains('hidden') && !!aimMenuBox
      && aimMenuBox.left >= -1 && aimMenuBox.top >= -1
      && aimMenuBox.right <= innerWidth + 1 && aimMenuBox.bottom <= innerHeight + 1);
    document.dispatchEvent(new KeyboardEvent('keydown', { key: 'Escape', bubbles: true, cancelable: true }));
    check('aim-button-menu-closed-1040', !!aimMenu && aimMenu.classList.contains('hidden') && aimSelect?.getAttribute('aria-expanded') === 'false');
    check('aim-visibility-fits-1040', !!aimTrack && !!aimTrackBox && !!aimBehaviorBox && aimTrackBox.right <= aimBehaviorBox.right + 1
      && aimTrack.scrollWidth <= aimTrack.clientWidth + 1
      && Array.from(aimTrack.querySelectorAll('.seg')).every((el) => el.scrollWidth <= el.clientWidth + 1 && el.getBoundingClientRect().bottom <= aimTrackBox.bottom + 1));
    check('aim-behavior-fits-1040', !!aimBehaviorBox && !!aimRailBox && aimBehaviorBox.bottom <= aimRailBox.bottom + 1);
    check('aim-rail-no-scroll', !!aimRail && aimRail.scrollHeight <= aimRail.clientHeight + 1);
    check('no-scroll-aim-shape', contentFits('aim'));
    document.querySelector('[data-action="aim-mode"][data-value="image"]')?.click();
    check('no-scroll-aim-image', contentFits('aim'));
    check('no-scroll-aim-image-loaded', imageWorstCaseFits());
    document.querySelector('[data-action="aim-mode"][data-value="shape"]')?.click();
    document.querySelector('[data-action="aim-visibility"][data-value="always"]')?.click();
    check('aim-button-row-hidden-restored', !!aimRow && aimRow.classList.contains('hidden'));
    window.__crosshair_setTab('display');
    const displayMore = document.querySelector('#displayMore');
    if (displayMore) displayMore.open = false;
    check('no-scroll-display-collapsed', contentFits('display'));
    if (displayMore) displayMore.open = true;
    check('no-scroll-display-expanded', contentFits('display'));
    if (displayMore) displayMore.open = false;
    window.__crosshair_setTab('audio');
    const audioMore = document.querySelector('#audioMore');
    if (audioMore) audioMore.open = false;
    check('no-scroll-audio-collapsed', contentFits('audio'));
    if (audioMore) audioMore.open = true;
    check('no-scroll-audio-expanded', contentFits('audio'));
    if (audioMore) audioMore.open = false;
    check('no-scroll-slots', contentFits('slots'));
  } catch (error) {
    errors.push('exception:' + String(error));
  }
  return JSON.stringify({ ok: errors.length === 0, errors, width: innerWidth, height: innerHeight, settings: settingsMetrics });
})()"#;

fn protocol_response(path: &str, cache: &Arc<Mutex<PreviewCache>>) -> wry::http::Response<Cow<'static, [u8]>> {
    let (status, content_type, body): (u16, &str, Cow<'static, [u8]>) = match path {
        "/" | "/index.html" => (200, "text/html; charset=utf-8", Cow::Borrowed(include_str!("../ui/index.html").as_bytes())),
        "/styles.css" => (200, "text/css; charset=utf-8", Cow::Borrowed(include_str!("../ui/styles.css").as_bytes())),
        "/app.js" => (200, "text/javascript; charset=utf-8", Cow::Borrowed(include_str!("../ui/app.js").as_bytes())),
        "/tray.html" => (200, "text/html; charset=utf-8", Cow::Borrowed(include_str!("../ui/tray.html").as_bytes())),
        "/tray.css" => (200, "text/css; charset=utf-8", Cow::Borrowed(include_str!("../ui/tray.css").as_bytes())),
        "/tray.js" => (200, "text/javascript; charset=utf-8", Cow::Borrowed(include_str!("../ui/tray.js").as_bytes())),
        "/favicon.ico" => (200, "image/x-icon", Cow::Borrowed(scene::ICON_ICO)),
        "/assets/scene_day.png" => (200, "image/png", Cow::Borrowed(scene::SCENE_DAY)),
        "/assets/scene_night.png" => (200, "image/png", Cow::Borrowed(scene::SCENE_NIGHT)),
        "/assets/icon.png" => (200, "image/png", Cow::Borrowed(scene::ICON_PNG)),
        "/preview" => {
            let png = cache.lock().map(|mut guard| guard.serve()).unwrap_or_default();
            if png.is_empty() {
                (503, "text/plain", Cow::Borrowed(b"preview unavailable".as_slice()))
            } else {
                (200, "image/png", Cow::Owned(png))
            }
        }
        other if other.starts_with("/crosshair-image/") => {
            match stored_image_request(other.trim_start_matches("/crosshair-image/")) {
                Some((content_type, bytes)) => (200, content_type, Cow::Owned(bytes)),
                None => (404, "text/plain", Cow::Borrowed(b"not found".as_slice())),
            }
        }
        other if other.starts_with("/assets/presets/") => {
            let style = other.trim_start_matches("/assets/presets/").trim_end_matches(".png");
            if ["plus", "dot", "ring", "circle_dot", "tsight", "triangle", "peak"].contains(&style) {
                (200, "image/png", Cow::Borrowed(scene::preset_png(style)))
            } else {
                (404, "text/plain", Cow::Borrowed(b"not found".as_slice()))
            }
        }
        _ => (404, "text/plain", Cow::Borrowed(b"not found".as_slice())),
    };
    wry::http::Response::builder()
        .status(status)
        .header("Content-Type", content_type)
        .header("Cache-Control", "no-store")
        .body(body)
        .unwrap_or_else(|_| wry::http::Response::new(Cow::Borrowed(b"error".as_slice())))
}

/// Decode the percent-encoded file name in a Recent-image URL. Null bytes,
/// separators and traversal are rejected later by `stored_image_path_in`.
fn percent_decode(value: &str) -> Option<String> {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let hex = value.get(index + 1..index + 3)?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            index += 3;
        } else {
            out.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(out).ok()
}

fn image_content_type(name: &str) -> &'static str {
    match Path::new(name)
        .extension()
        .and_then(|value| value.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("webp") => "image/webp",
        Some("gif") => "image/gif",
        Some("svg") => "image/svg+xml",
        Some("bmp") => "image/bmp",
        Some("ico") => "image/x-icon",
        _ => "application/octet-stream",
    }
}

/// A thumbnail request reads one stored crosshair image from the data folder.
/// The same size cap as the importer keeps a stray huge file out of memory.
fn stored_image_request(encoded: &str) -> Option<(&'static str, Vec<u8>)> {
    let name = percent_decode(encoded)?;
    let path = config::stored_image_path_in(&config::crosshairs_dir(), &name)?;
    let meta = std::fs::metadata(&path).ok()?;
    if !import_size_allowed(meta.len()) {
        return None;
    }
    let bytes = std::fs::read(&path).ok()?;
    Some((image_content_type(&name), bytes))
}

/// Turns the problems a config load or a restore undo had to clean up into one
/// banner, so the user learns why an imported curve disappeared instead of
/// finding only a log line.
fn cleanup_warning_banner(warnings: &[String]) -> Option<IpcEvent> {
    if warnings.is_empty() {
        return None;
    }
    Some(IpcEvent::DiagnosticBanner { code: "eq".into(), message: warnings.join(" "), recoverable: true })
}

/// Cumulative process CPU in 100 ns units. Delivered as a delta between two
/// reports, so the log can show a percentage of one core.
fn process_cpu_100ns() -> u64 {
    use windows::Win32::Foundation::FILETIME;
    use windows::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};

    let zero = || FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 };
    let (mut created, mut exited, mut kernel, mut user) = (zero(), zero(), zero(), zero());
    if unsafe { GetProcessTimes(GetCurrentProcess(), &mut created, &mut exited, &mut kernel, &mut user) }.is_err() {
        return 0;
    }
    filetime_100ns(kernel) + filetime_100ns(user)
}

/// Shared by the process sampler here and the overlay thread's own CPU counter.
pub(crate) fn filetime_100ns(time: windows::Win32::Foundation::FILETIME) -> u64 {
    time.dwLowDateTime as u64 | (time.dwHighDateTime as u64) << 32
}

#[derive(Debug, PartialEq, Eq)]
enum EmptyScan {
    /// Use the scan result, even when it is empty.
    Apply,
    /// Keep the cached entries that still exist on disk.
    Keep,
    /// Nothing cached survived, so the picker is emptied.
    Clear,
}

/// What an empty scan means. A background refresh never erases a picker the
/// user can still use, but it does drop entries whose executable is gone; an
/// explicit Rescan is applied as scanned, which is how a real uninstall-all
/// takes effect immediately.
fn empty_scan_outcome(explicit: bool, cached: usize, alive: usize) -> EmptyScan {
    if explicit || cached == 0 {
        return EmptyScan::Apply;
    }
    if alive == 0 {
        EmptyScan::Clear
    } else {
        EmptyScan::Keep
    }
}

fn cpu_percent(delta_100ns: u64, elapsed: Duration) -> f64 {
    let elapsed_100ns = elapsed.as_nanos() as f64 / 100.0;
    if elapsed_100ns <= 0.0 {
        return 0.0;
    }
    delta_100ns as f64 / elapsed_100ns * 100.0
}

fn overlay_cost_line(
    elapsed: Duration,
    previous: &OverlayCostSnapshot,
    current: &OverlayCostSnapshot,
    previous_process_100ns: u64,
    process_100ns: u64,
    visible: bool,
) -> String {
    let paints = current.paints.saturating_sub(previous.paints);
    let paint_ns = current.paint_ns.saturating_sub(previous.paint_ns);
    let average_ms = paint_ns as f64 / paints.max(1) as f64 / 1e6;
    let hertz = paints as f64 / elapsed.as_secs_f64().max(f64::EPSILON);
    let overlay_cpu = cpu_percent(current.thread_cpu_100ns.saturating_sub(previous.thread_cpu_100ns), elapsed);
    let process_cpu = cpu_percent(process_100ns.saturating_sub(previous_process_100ns), elapsed);
    format!(
        "overlay: cost {}s visible={visible} paints={paints} repaint_hz={hertz:.1} paint_avg={average_ms:.2}ms paint_max={:.2}ms overlay_cpu={overlay_cpu:.1}% process_cpu={process_cpu:.1}%",
        elapsed.as_secs(),
        current.peak_ns as f64 / 1e6,
    )
}

/// One line per window so the log says whether the animated preview produced
/// frames at the source's own rate and whether the page kept up with them.
fn preview_cost_line(elapsed: Duration, previous: &PreviewCost, current: &PreviewCost) -> String {
    let frames = current.frames.saturating_sub(previous.frames);
    let skipped = current.skipped.saturating_sub(previous.skipped);
    let serves = current.serves.saturating_sub(previous.serves);
    let render_ns = current.render_ns.saturating_sub(previous.render_ns);
    let average_ms = render_ns as f64 / frames.max(1) as f64 / 1e6;
    let seconds = elapsed.as_secs_f64().max(f64::EPSILON);
    let hertz = frames as f64 / seconds;
    let serve_hz = serves as f64 / seconds;
    format!(
        "preview: {}s {hertz:.1} fps frames={frames} skipped={skipped} render_avg={average_ms:.1}ms render_max={:.1}ms serves={serve_hz:.1}/s rev={}",
        elapsed.as_secs(),
        current.render_peak_ns as f64 / 1e6,
        current.rev,
    )
}

fn import_size_allowed(bytes: u64) -> bool {
    bytes <= config::MAX_IMPORT_BYTES
}

fn image_extension_supported(path: &Path) -> bool {
    let ext = path.extension().and_then(|value| value.to_str()).unwrap_or("").to_ascii_lowercase();
    config::IMAGE_EXTENSIONS.contains(&ext.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overlay_payload_carries_the_live_image_state() {
        let mut crosshair = crate::config::CrosshairConfig::default();
        crosshair.image = Some("C:/data/crosshairs/123-imported.png".into());
        crosshair.image_name = Some("imported.png".into());
        let input = overlay_input(&crosshair, Some((0, 0, 1920, 1080)), Vec::new(), "test".into());
        assert_eq!(input.config.image, crosshair.image, "the overlay payload must carry a newly imported image");
        assert_eq!(input.config.image_name, crosshair.image_name);

        crosshair.image = None;
        crosshair.image_name = None;
        let input = overlay_input(&crosshair, Some((0, 0, 1920, 1080)), Vec::new(), "test".into());
        assert!(input.config.image.is_none(), "the overlay payload must carry a removed image");
        assert!(input.config.image_name.is_none());
    }

    #[test]
    fn automated_modes_have_no_live_display_audio_or_startup_paths() {
        let normal = RunModes::default();
        assert!(normal.live_system());
        assert_eq!(display_path(normal), DisplayPath::Live);
        assert_eq!(audio_path(normal), AudioPath::Live);

        let sandboxed = RunModes { sandboxed: true, ..Default::default() };
        assert!(!sandboxed.live_system());
        assert_eq!(display_path(sandboxed), DisplayPath::Skip);
        assert_eq!(audio_path(sandboxed), AudioPath::Skip);

        for (label, modes) in [
            ("capture", RunModes { capture: true, sandboxed: true, ..Default::default() }),
            ("capture without a sandbox root", RunModes { capture: true, ..Default::default() }),
            ("preset verification", RunModes { verify: true, ..Default::default() }),
            ("FxSound verification", RunModes { fx_verify: true, sandboxed: true, ..Default::default() }),
            ("interaction test", RunModes { interaction: true, sandboxed: true, ..Default::default() }),
            ("stress harness", RunModes { stress: true, sandboxed: true, ..Default::default() }),
        ] {
            assert!(modes.automated(), "{label} is an automated mode");
            assert!(!modes.live_system(), "{label} must not launch real system actions or write the startup entry");
            assert_eq!(audio_path(modes), AudioPath::Skip, "{label} must not apply FxSound settings");
            let expected_display = if modes.capture { DisplayPath::Preview } else { DisplayPath::Skip };
            assert_eq!(display_path(modes), expected_display, "{label} display path");
        }
    }

    #[test]
    fn capture_self_test_never_clicks_live_launch_or_install_controls() {
        // The self-test may open modal-only controls, but a click on any of
        // these would reach the real installer, Explorer, the log, FxSound, an
        // external download, or a backup file outside the sandbox.
        for control in [
            "#fxInstallButton",
            "[data-action=\"setup-run\"]",
            "[data-action=\"repair-run\"]",
            "[data-action=\"open-data\"]",
            "[data-action=\"open-log\"]",
            "[data-action=\"open-fxsound\"]",
            "[data-action=\"open-fxsound-download\"]",
            "[data-action=\"open-windows-display\"]",
            "[data-action=\"backup-create\"]",
            "[data-action=\"backup-browse\"]",
            "[data-action=\"backup-restore\"]",
            "[data-action=\"backup-undo\"]",
        ] {
            for line in SELFTEST_JS.lines() {
                if line.contains(control) {
                    assert!(
                        !line.contains(".click()"),
                        "capture self-test must not click {control}: {line}"
                    );
                }
            }
        }
        // Unstick is clicked, so it must remain covered by the central gate.
        assert!(SELFTEST_JS.contains("[data-action=\"unstick-audio\"]')?.click()"));
        assert!(!RunModes { capture: true, ..Default::default() }.live_system());
        assert!(!RunModes { capture: true, sandboxed: true, ..Default::default() }.live_system());
    }

    #[test]
    fn capture_command_paths_use_the_central_live_mode_gate() {
        let source = std::fs::read_to_string(
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src").join("app.rs"),
        )
        .expect("the app source must be readable");
        assert!(source.contains(concat!("if self.modes().live_", "system()")), "the forced Sound-tab refresh must use the live-system gate");
        assert!(source.contains(concat!("!self.modes().live_", "system()")), "the polling refresh must use the live-system gate");
        assert!(source.contains(concat!("if !self.modes().live_", "system()")), "Compare must use the live-system gate");
        assert!(source.contains(concat!("apply_run_key_checked(&self.config.", "settings, self.modes())")), "Settings and reset must use the startup-entry gate");
        assert!(!source.contains(concat!("apply_run_key(&self.config.", "settings)")), "no handler may bypass the startup-entry gate");
        assert!(source.contains(concat!("DisplayRuntime::start(proxy.clone(), monitors.clone(), !modes.", "live_system())")), "automated runs must start the preview-only display worker");
        assert!(source.contains("self.sandbox_blocked(\"audio unstick\")"), "Unstick must use the central live-system gate");
        assert!(!source.contains(concat!("fn blocks_", "audio_unstick")), "Unstick must not carry a second mode list");
        assert!(
            source.matches("self.sandbox_blocked(\"display restore\")").count() >= 3,
            "suspend, logoff and reset must all use the central display-restore gate"
        );
        // Startup probes, the Sound tab and factory reset each force one
        // refresh; every one sits behind `live_system`.
        assert_eq!(source.matches(concat!("self.audio.refresh(", "true)")).count(), 3, "the forced FxSound refreshes must stay accounted for");
        assert!(source.matches(concat!("if self.modes().live_", "system()")).count() >= 4, "every forced FxSound refresh must use the live-system gate");
        assert!(source.contains("sandbox: tray skipped; no real shell icon"), "automated runs must not create a real tray icon");
        assert!(source.contains("sandbox: {} picker skipped; real dialog untouched"), "automated runs must not open native file pickers");
        assert!(source.contains("sandbox: backup create skipped; real files untouched"), "capture must not create backup archives");
        assert!(source.contains("sandbox: backup restore skipped; real files untouched"), "capture must not restore backups");
        assert!(source.contains("sandbox: backup undo skipped; real files untouched"), "capture must not undo restores");
        assert!(source.contains("self.sandbox_blocked(\"opening the log\")"), "capture must not launch Explorer for the log");
        assert!(
            source.contains("if !self.modes().live_system() && !self.interaction_mode && !self.stress_mode"),
            "verify/fxverify/capture must not publish an overlay"
        );
        assert!(
            source.contains("if !self.modes().live_system() {") && source.contains("slots::auto_activate_for_app"),
            "harnesses must not auto-switch slots from the foreground watcher"
        );
        assert!(
            source.contains("self.config.settings.auto_revert_slots && slots::active_target_stopped"),
            "the revert setting must gate only reverting, never activation"
        );
    }

    #[test]
    fn automated_and_sandboxed_modes_publish_no_global_hotkeys() {
        let config = Config::default();
        assert!(!active_hotkeys(RunModes::default(), &config).is_empty(), "a live launch keeps its hotkeys");
        for modes in [
            RunModes { capture: true, ..Default::default() },
            RunModes { verify: true, ..Default::default() },
            RunModes { fx_verify: true, ..Default::default() },
            RunModes { interaction: true, ..Default::default() },
            RunModes { stress: true, ..Default::default() },
            RunModes { sandboxed: true, ..Default::default() },
        ] {
            assert!(active_hotkeys(modes, &config).is_empty(), "{modes:?} must not register global hotkeys");
        }
    }

    #[test]
    fn fxsound_verify_script_never_launches_or_applies() {
        let source = include_str!("app.rs");
        assert!(!FX_VERIFY_JS.contains("launch.click()"), "verify must not click Launch FxSound");
        assert!(!FX_VERIFY_JS.contains("__crosshairFlushAudio"), "verify must not apply audio");
        assert!(!FX_VERIFY_JS.contains("ImportEq"), "verify must not import an EQ curve");
        assert!(!FX_VERIFY_JS.contains("UpdateSound"), "verify must not send a sound update");
        assert!(!FX_VERIFY_JS.contains("expectedAudio"), "the page must not supply sandbox audio as the expectation");
        assert!(source.contains("crate::config::live_config_path()"), "verify must compare the live config, not the sandbox copy");
        assert!(!FX_VERIFY_JS.contains("--status"), "verify must not invoke the FxSound status CLI");
        assert!(
            source.contains(concat!("self.exit_code = ", "1;")),
            "a failed verifier report must set a nonzero exit code"
        );
        assert!(
            source.contains(concat!("ControlFlow::ExitWithCode(self.", "exit_code)")),
            "the event loop must carry the verifier exit code"
        );
    }

    #[test]
    fn fxsound_verifier_reads_the_audio_settings_the_app_writes_and_reloads() {
        let dir = std::env::temp_dir().join(format!(
            "crosshairplus-fxverify-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");
        let mut config = Config::default();
        config.audio.enabled = true;
        config.audio.bands = [2.5, -1.0, 0.5, 1.0, -2.0, 3.0, 1.5, -0.5, 2.0, 4.0];
        config.audio.bass = 6.0;
        config.audio.device = Some("Speakers (Live)".into());
        config.save_to(&path).unwrap();
        // Startup loads the live config through Config::load_from. The verifier
        // must read that same audio section, not a sandbox copy or defaults.
        let reloaded = Config::load_from(&path);
        assert_eq!(reloaded.audio, config.audio, "the app must reload the audio section it saved");
        let (audio, warnings) = fx_verify_expected_audio(&path).expect("the live config must provide the expectation");
        assert!(warnings.is_empty(), "a clean saved audio section must load without warnings: {warnings:?}");
        assert_eq!(audio, reloaded.audio, "the verifier must compare the persisted audio section the app reloads");
        assert_ne!(audio, crate::config::AudioConfig::default(), "the expectation must not be sandbox defaults");
        assert!(
            fx_verify_expected_audio(&dir.join("missing.json")).is_err(),
            "a missing live config must fail instead of silently falling back to defaults"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn config_cleanup_warnings_become_one_audio_banner() {
        assert!(cleanup_warning_banner(&[]).is_none(), "a clean load must not notify");
        let event = cleanup_warning_banner(&[
            "The sound profile 'Desk EQ' was not applied because its preamp is not a valid number.".into(),
            "Dropped 1 band(s) with invalid numbers from the sound profile 'Desk EQ'.".into(),
        ])
        .expect("warnings become a banner");
        let value = serde_json::to_value(&event).unwrap();
        assert_eq!(value["type"], "DiagnosticBanner");
        assert_eq!(value["payload"]["code"], "eq");
        assert_eq!(value["payload"]["recoverable"], true);
        let message = value["payload"]["message"].as_str().expect("message text");
        assert!(message.contains("preamp is not a valid number"), "{message}");
        assert!(message.contains("Dropped 1 band(s)"), "{message}");
    }

    #[test]
    fn fxsound_status_comparison_matches_live_effect_scale() {
        let audio = crate::config::AudioConfig {
            enabled: true,
            bands: [1.0, 2.0, 3.0, 4.0, 5.0, -1.0, -2.0, -3.0, -4.0, 3.0],
            bass: 3.0,
            treble: 4.0,
            echo: 5.0,
            surround: 6.0,
            dynamic_boost: 7.0,
            output_level: -7.0,
            volume_leveling: 2.5,
            balance: -8.0,
            filter_q: 2.0,
            volume_guard: false,
            ..Default::default()
        };
        let (_, bands) = dsp_math::fx_bands(&audio);
        let actual = serde_json::json!({
            "power": true,
            "equalizer": {
                "num_bands": 10,
                "master_gain": -7.0,
                "volume_leveling": 2.5,
                "balance": -8.0,
                "filter_q": 2.0,
                "bands": bands.iter().map(|gain| serde_json::json!({ "gain": gain })).collect::<Vec<_>>(),
            },
            "effects": { "bass": 3.0, "clarity": 4.0, "ambience": 5.0, "surround": 6.0, "dynamicboost": 7.0 },
        });
        assert_eq!(compare_fx_status(&audio, &actual).get("ok").and_then(|v| v.as_bool()), Some(true));
        let mut wrong = actual;
        wrong["effects"]["bass"] = serde_json::json!(30.0);
        assert_eq!(compare_fx_status(&audio, &wrong).get("ok").and_then(|v| v.as_bool()), Some(false));
    }

    #[test]
    fn golden_status_fixture_reads_known_values() {
        let actual: serde_json::Value = serde_json::from_str(include_str!("testdata/fxsound-status.json"))
            .expect("golden FxSound status fixture must parse");
        let expected = crate::config::AudioConfig {
            enabled: true,
            device: Some("Headphones (Crusher Evo)".into()),
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
        };
        let comparison = compare_fx_status(&expected, &actual);
        assert_eq!(comparison.get("ok").and_then(|v| v.as_bool()), Some(true), "{comparison}");

        let mut regressed = actual;
        regressed["equalizer"]["bands"][0]["gain"] = serde_json::json!(9.0);
        assert_eq!(compare_fx_status(&expected, &regressed).get("ok").and_then(|v| v.as_bool()), Some(false));
    }

    #[test]
    fn navigation_allowlist_is_exact_origin() {
        for allowed in [
            "https://app.localhost",
            "https://app.localhost/",
            "https://app.localhost/preview?t=1",
            "https://app.localhost#settings",
            "HTTPS://APP.LOCALHOST/assets/scene_day.png",
        ] {
            assert!(navigation_allowed(allowed), "should allow {allowed}");
        }
        for blocked in [
            "http://app.localhost/",
            "https://app.localhost.evil.example/",
            "https://app.localhost@evil.example/",
            "https://app.localhost:8443/",
            "https://evil.example/?next=https://app.localhost/",
        ] {
            assert!(!navigation_allowed(blocked), "should block {blocked}");
        }
    }

    #[test]
    fn window_dimensions_clamp_for_desktop_and_small_panels() {
        assert_eq!(window_dimensions(2560, 1440), (1200, 780));
        assert_eq!(window_dimensions(1920, 1080), (1200, 780));
        assert_eq!(window_dimensions(1366, 768), (1200, 691));
        assert_eq!(window_dimensions(1024, 600), (1040, 680));
    }

    #[test]
    fn tray_panel_height_tracks_slot_rows_without_dead_space() {
        assert_eq!(TRAY_PANEL_WIDTH, 272.0);
        assert_eq!(tray_panel_height(0), 380.0);
        assert_eq!(tray_panel_height(3), 462.0);
        assert_eq!(tray_panel_height(6), 576.0);
        assert_eq!(tray_panel_height(99), tray_panel_height(crate::config::MAX_SLOTS));
        assert!(tray_panel_height(1) < tray_panel_height(3));
        assert!(tray_panel_height(3) < tray_panel_height(6));
    }

    #[test]
    fn run_key_value_quotes_the_current_executable() {
        let value = run_key_value(Path::new("C:\\Program Files\\Crosshair+\\CrosshairPlus.exe"));
        assert_eq!(value, "\"C:\\Program Files\\Crosshair+\\CrosshairPlus.exe\" --minimized");
    }

    struct TestRunKey(String);

    impl Drop for TestRunKey {
        fn drop(&mut self) {
            delete_test_run_key_at(&self.0);
        }
    }

    #[test]
    fn run_key_round_trips_through_a_test_only_key() {
        let subkey = format!("Software\\CrosshairPlusTestRun-{}", std::process::id());
        delete_test_run_key_at(&subkey);
        let _guard = TestRunKey(subkey.clone());
        let expected = run_key_value(&std::env::current_exe().unwrap());

        let enabled = Settings { start_with_windows: true, ..Default::default() };
        assert!(apply_run_key_at(&enabled, &subkey).is_ok(), "the test Run key must be writable");
        assert_eq!(read_run_key_at(&subkey).unwrap().as_deref(), Some(expected.as_str()));

        let disabled = Settings { start_with_windows: false, ..Default::default() };
        assert!(apply_run_key_at(&disabled, &subkey).is_ok());
        assert_eq!(read_run_key_at(&subkey).unwrap(), None);
    }

    #[test]
    fn late_result_from_a_released_scan_is_dropped_and_the_new_watchdog_stays_armed() {
        let mut scan = GameScan::default();
        let first = scan.begin().expect("the first scan starts");
        assert!(scan.time_out(Duration::ZERO), "the watchdog releases the picker");
        assert!(scan.started.is_none(), "a released scan is no longer watched");

        let second = scan.begin().expect("a released scan does not block a rescan");
        let armed = scan.started.expect("the new scan is watched");
        assert_ne!(first, second, "every scan gets its own generation");

        assert!(!scan.finish(first), "a result from the timed-out scan is stale");
        assert_eq!(scan.started, Some(armed), "the newer scan keeps its watchdog");

        assert!(scan.finish(second), "the current scan's result is accepted");
        assert!(scan.started.is_none(), "finishing ends the watchdog");
    }

    #[test]
    fn a_second_scan_while_one_is_watched_is_a_no_op() {
        let mut scan = GameScan::default();
        let first = scan.begin().expect("the first scan starts");
        assert_eq!(scan.begin(), None, "a scan already in flight is not replaced");
        assert!(scan.finish(first));
    }

    #[test]
    fn a_late_result_still_fills_the_picker_when_nothing_newer_started() {
        let mut scan = GameScan::default();
        let generation = scan.begin().expect("the scan starts");
        assert!(scan.time_out(Duration::ZERO));
        assert!(scan.finish(generation), "the released scan's own result is still used");
        assert!(scan.started.is_none(), "the watchdog is not re-armed by a late result");
    }

    #[test]
    fn an_empty_scan_keeps_living_entries_and_prunes_dead_ones() {
        assert_eq!(empty_scan_outcome(false, 12, 10), EmptyScan::Keep, "a transient refresh keeps what still exists");
        assert_eq!(empty_scan_outcome(false, 3, 3), EmptyScan::Keep, "nothing to prune, keep all three");
        assert_eq!(empty_scan_outcome(false, 12, 0), EmptyScan::Clear, "every cached game is gone");
        assert_eq!(empty_scan_outcome(true, 12, 10), EmptyScan::Apply, "Rescan is applied as scanned");
        assert_eq!(empty_scan_outcome(false, 0, 0), EmptyScan::Apply, "nothing cached to keep");
    }

    #[test]
    fn overlay_cost_line_reports_a_visible_window() {
        let previous = OverlayCostSnapshot::default();
        let current = OverlayCostSnapshot {
            paints: 900,
            paint_ns: 900 * 400_000,
            peak_ns: 2_100_000,
            thread_cpu_100ns: 4_500_000,
        };
        let line = overlay_cost_line(Duration::from_secs(30), &previous, &current, 0, 9_000_000, true);
        assert!(line.contains("overlay: cost 30s visible=true"), "{line}");
        assert!(line.contains("paints=900 repaint_hz=30.0"), "{line}");
        assert!(line.contains("paint_avg=0.40ms paint_max=2.10ms"), "{line}");
        assert!(line.contains("overlay_cpu=1.5% process_cpu=3.0%"), "{line}");
    }

    #[test]
    fn overlay_cost_line_reports_an_idle_window() {
        let unchanged = OverlayCostSnapshot {
            paints: 900,
            paint_ns: 360_000_000,
            peak_ns: 0,
            thread_cpu_100ns: 4_500_000,
        };
        let line = overlay_cost_line(Duration::from_secs(30), &unchanged, &unchanged, 9_000_000, 9_000_000, false);
        assert!(line.contains("visible=false paints=0"), "{line}");
        assert!(line.contains("repaint_hz=0.0 paint_avg=0.00ms"), "{line}");
        assert!(line.contains("overlay_cpu=0.0% process_cpu=0.0%"), "{line}");
    }

    #[test]
    fn preview_cost_line_reports_a_smooth_window() {
        let previous = PreviewCost::default();
        let current = PreviewCost {
            rev: 42,
            frames: 250,
            skipped: 12,
            serves: 252,
            render_ns: 250 * 6_000_000,
            render_peak_ns: 11_400_000,
        };
        let line = preview_cost_line(Duration::from_secs(10), &previous, &current);
        assert!(line.contains("preview: 10s 25.0 fps frames=250 skipped=12"), "{line}");
        assert!(line.contains("render_avg=6.0ms render_max=11.4ms"), "{line}");
        assert!(line.contains("serves=25.2/s rev=42"), "{line}");
    }

    #[test]
    fn destructive_actions_use_the_styled_confirm_dialog() {
        let js = include_str!("../ui/app.js");
        assert!(!js.contains("window.confirm"), "a native confirm dialog came back");
        assert_eq!(js.matches("confirmThen({").count(), 6, "every confirmed action must route through the styled dialog");
        assert!(include_str!("../ui/index.html").contains("id=\"confirmModal\""));
    }

    #[test]
    fn image_solid_controls_use_the_camel_case_config_keys() {
        // Config is serialized with rename_all = "camelCase", so direct property
        // access in the UI must use imageSolid; a snake_case write was silently
        // dropped and the solid-color toggle never reached the renderer.
        assert!(!include_str!("../ui/app.js").contains("image_solid"));
    }

    #[test]
    fn image_import_size_guard_matches_the_documented_limit() {
        assert!(import_size_allowed(0));
        assert!(import_size_allowed(config::MAX_IMPORT_BYTES));
        assert!(!import_size_allowed(config::MAX_IMPORT_BYTES + 1));
        assert_eq!(config::MAX_IMPORT_BYTES, 20 * 1024 * 1024);
    }

    #[test]
    fn image_import_accepts_only_plan_formats() {
        for path in ["a.png", "a.JPG", "a.jpeg", "a.webp", "a.gif", "a.svg", "a.bmp", "a.ICO"] {
            assert!(image_extension_supported(Path::new(path)), "{path}");
        }
        for path in ["a.exe", "a.txt", "a"] {
            assert!(!image_extension_supported(Path::new(path)), "{path}");
        }
    }

    #[test]
    fn crosshair_image_route_decodes_names_and_rejects_traversal() {
        assert_eq!(percent_decode("1700-aim%20shot.PNG").as_deref(), Some("1700-aim shot.PNG"));
        assert_eq!(percent_decode("na%C3%AFve.png").as_deref(), Some("naïve.png"));
        assert!(percent_decode("%ZZ").is_none());
        assert!(percent_decode("%2").is_none());
        assert!(percent_decode("..%2Fsecret.png").as_deref() == Some("../secret.png"));
        assert!(stored_image_request("..%2Fsecret.png").is_none(), "a decoded separator must never reach a file read");
        assert_eq!(image_content_type("a.GIF"), "image/gif");
        assert_eq!(image_content_type("a.webp"), "image/webp");
    }

    #[test]
    fn startup_audio_reconciliation_requires_a_running_ready_install() {
        let ready = FxSoundStatus { installed: true, running: true, cli_ready: true, ..Default::default() };
        assert!(startup_audio_eligible(&ready));
        assert!(!startup_audio_eligible(&FxSoundStatus { installed: false, ..ready.clone() }));
        assert!(!startup_audio_eligible(&FxSoundStatus { running: false, ..ready.clone() }));
        assert!(!startup_audio_eligible(&FxSoundStatus { cli_ready: false, ..ready }));
    }

    #[test]
    fn late_fxsound_startup_defers_only_for_a_ready_install_with_sound_on() {
        let closed = FxSoundStatus { installed: true, running: false, cli_ready: true, ..Default::default() };
        assert!(startup_audio_deferred(true, &closed), "a not-running ready install must defer");
        assert!(!startup_audio_deferred(false, &closed), "Sound off must not set the deferral");
        assert!(
            !startup_audio_deferred(true, &FxSoundStatus { installed: false, ..closed.clone() }),
            "a missing install must not set the deferral"
        );
        assert!(
            !startup_audio_deferred(true, &FxSoundStatus { cli_ready: false, ..closed.clone() }),
            "an outdated install must not set the deferral"
        );
        assert!(
            !startup_audio_deferred(true, &FxSoundStatus { running: true, ..closed }),
            "an already running install must not defer"
        );
    }

    #[test]
    fn late_fxsound_startup_waits_then_reconciles_exactly_once() {
        let closed = FxSoundStatus { installed: true, running: false, cli_ready: true, ..Default::default() };
        let mut pending = startup_audio_deferred(true, &closed);
        assert!(pending, "(a) a not-running FxSound must leave the catch-up pending");
        assert!(
            !deferred_audio_reconcile_due(&mut pending, true, &closed),
            "(a) the deferred status must not reconcile immediately"
        );
        assert!(pending, "(a) the flag must stay set");

        let old = FxSoundStatus { running: true, cli_ready: false, ..closed.clone() };
        assert!(
            !deferred_audio_reconcile_due(&mut pending, true, &old),
            "(b) an outdated FxSound must not reconcile"
        );
        assert!(pending, "(b) an outdated FxSound must leave the flag pending");

        let ready = FxSoundStatus { running: true, cli_ready: true, ..closed };
        assert!(
            deferred_audio_reconcile_due(&mut pending, true, &ready),
            "(c) a fresh ready status must reconcile"
        );
        assert!(!pending, "(c) the catch-up must clear the flag");
        assert!(
            !deferred_audio_reconcile_due(&mut pending, true, &ready),
            "(c) a further eligible status must not reconcile again"
        );
    }

    #[test]
    fn late_fxsound_startup_is_dropped_when_sound_is_turned_off() {
        let closed = FxSoundStatus { installed: true, running: false, cli_ready: true, ..Default::default() };
        let ready = FxSoundStatus { running: true, ..closed.clone() };
        let mut pending = startup_audio_deferred(true, &closed);
        assert!(pending);
        assert!(
            !deferred_audio_reconcile_due(&mut pending, false, &ready),
            "(d) Sound off must not apply the reconcile"
        );
        assert!(!pending, "(d) Sound off must clear the flag");
        assert!(
            !deferred_audio_reconcile_due(&mut pending, true, &ready),
            "(d) the dropped catch-up must not return"
        );
    }
}

