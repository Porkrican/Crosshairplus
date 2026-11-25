use std::cell::RefCell;
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use tao::event_loop::EventLoopProxy;
use windows::core::{BOOL, PCWSTR};
use windows::Win32::Foundation::{
    COLORREF, GetLastError, HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, SIZE, WPARAM,
};
use windows::Win32::Graphics::Direct2D::Common::D2D_RECT_F;
use windows::Win32::Graphics::Direct2D::D2D1_ROUNDED_RECT;
use windows::Win32::Graphics::DirectWrite::{
    DWRITE_FONT_WEIGHT_SEMI_BOLD, DWRITE_PARAGRAPH_ALIGNMENT_CENTER, DWRITE_TEXT_ALIGNMENT_LEADING,
    DWRITE_TEXT_METRICS,
};
use windows::Win32::Graphics::Gdi::{
    AC_SRC_ALPHA, AC_SRC_OVER, BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BLENDFUNCTION, CreateCompatibleDC,
    CreateDIBSection, DIB_RGB_COLORS, DeleteDC, DeleteObject, GetDC, GetMonitorInfoW, MonitorFromWindow,
    ReleaseDC, SelectObject, MONITORINFO, MONITOR_DEFAULTTOPRIMARY,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::RemoteDesktop::WTSRegisterSessionNotification;
use windows::Win32::UI::Accessibility::{SetWinEventHook, UnhookWinEvent, HWINEVENTHOOK};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, RegisterHotKey, UnregisterHotKey, MOD_NOREPEAT, VK_RBUTTON,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DispatchMessageW, EnumWindows, GetClassNameW, GetCursorInfo,
    GetForegroundWindow, GetGUIThreadInfo, GetMessageW, GetWindowRect,
    GetWindowThreadProcessId, IsWindowVisible, KillTimer, PostMessageW, PostQuitMessage, RegisterClassW,
    SetTimer, SetWindowPos, ShowWindow, TranslateMessage, UpdateLayeredWindow, CURSORINFO, CURSOR_SHOWING,
    GUITHREADINFO, GUI_INMENUMODE, GUI_POPUPMENUMODE, GUI_SYSTEMMENUMODE, HWND_TOPMOST, MSG,
    PBT_APMRESUMEAUTOMATIC, PBT_APMSUSPEND, SW_HIDE, SW_SHOWNOACTIVATE,
    ULW_ALPHA, WNDCLASSW, WM_APP, WM_DISPLAYCHANGE, WM_ENDSESSION, WM_HOTKEY, WM_POWERBROADCAST,
    WM_QUERYENDSESSION, WM_TIMER, WM_WTSSESSION_CHANGE, WTS_SESSION_UNLOCK, WS_EX_LAYERED,
    WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_EX_TRANSPARENT, WS_POPUP, WS_VISIBLE,
    EVENT_SYSTEM_FOREGROUND, WINEVENT_OUTOFCONTEXT,
};
use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
use windows::Win32::UI::WindowsAndMessaging::{SWP_NOACTIVATE, SWP_SHOWWINDOW};

use crate::app::AppEvent;
use crate::config::CrosshairConfig;
use crate::render::crosshair::Renderer;
use crate::render::engine::Rgba;

const WM_APP_UPDATE: u32 = WM_APP + 1;
const WM_APP_TOAST: u32 = WM_APP + 2;
const TIMER_RENDER: usize = 1;
const TIMER_OSD: usize = 2;
const TOAST_MS: u128 = 1500;
const CLASS_NAME: &str = "CrosshairPlusOverlayHub";

#[derive(Clone, Debug)]
pub struct HotkeyBinding {
    pub id: i32,
    pub mods: u32,
    pub vk: u32,
    pub key: String,
    pub slot_id: Option<String>,
}

#[derive(Clone, Debug)]
pub struct Input {
    pub config: CrosshairConfig,
    pub rect: Option<(i32, i32, u32, u32)>,
    pub hotkeys: Vec<HotkeyBinding>,
    pub monitor_label: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct OverlayRuntimeState {
    pub enabled: bool,
    pub visible: bool,
    pub hidden: bool,
    pub render_requests: u64,
    pub renders: u64,
}

#[derive(Clone, Debug)]
pub enum OverlayEvent {
    Hotkey { id: i32 },
    HotkeyConflict { slot_id: Option<String>, key: String, owner: String },
    DisplayChanged,
    ForegroundChanged,
    PowerSuspend,
    PowerResume,
    SessionUnlock,
    EndSession,
    Diagnostic { code: String, message: String },
}

/// Counters kept by the overlay thread so a log line can answer what the
/// crosshair costs without a profiler. Only atomics on the paint path.
#[derive(Default)]
pub struct OverlayCost {
    paints: AtomicU64,
    paint_ns: AtomicU64,
    peak_ns: AtomicU64,
    thread_cpu_100ns: AtomicU64,
}

impl OverlayCost {
    fn record(&self, elapsed: Duration, thread_cpu_100ns: u64) {
        let ns = elapsed.as_nanos() as u64;
        self.paints.fetch_add(1, Ordering::Relaxed);
        self.paint_ns.fetch_add(ns, Ordering::Relaxed);
        self.peak_ns.fetch_max(ns, Ordering::Relaxed);
        self.thread_cpu_100ns.store(thread_cpu_100ns, Ordering::Relaxed);
    }

    fn take_snapshot(&self) -> OverlayCostSnapshot {
        OverlayCostSnapshot {
            paints: self.paints.load(Ordering::Relaxed),
            paint_ns: self.paint_ns.load(Ordering::Relaxed),
            peak_ns: self.peak_ns.swap(0, Ordering::Relaxed),
            thread_cpu_100ns: self.thread_cpu_100ns.load(Ordering::Relaxed),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct OverlayCostSnapshot {
    pub paints: u64,
    pub paint_ns: u64,
    /// Longest paint since the previous snapshot; taking one clears it, so the
    /// reported peak covers only the window it is reported for.
    pub peak_ns: u64,
    pub thread_cpu_100ns: u64,
}

struct Ctx {
    hub: HWND,
    overlay: Option<HWND>,
    hinstance: HINSTANCE,
    proxy: EventLoopProxy<AppEvent>,
    shared: Arc<Mutex<Option<Input>>>,
    shutdown: Arc<AtomicBool>,
    renderer: Option<Renderer>,
    target: Option<crate::render::engine::Target>,
    target_size: (u32, u32),
    registered: Vec<i32>,
    hotkey_sig: Vec<(i32, u32, u32, String, Option<String>)>,
    win_event: Option<HWINEVENTHOOK>,
    visible: bool,
    timer_running: bool,
    osd: Option<HWND>,
    osd_target: Option<crate::render::engine::Target>,
    osd_size: (u32, u32),
    toast: Option<Toast>,
    osd_timer_running: bool,
    toast_request: Arc<Mutex<Option<String>>>,
    runtime: Arc<Mutex<OverlayRuntimeState>>,
    cost: Arc<OverlayCost>,
}

struct Toast {
    text: String,
    started: Instant,
}

thread_local! {
    static CTX: RefCell<Option<Ctx>> = const { RefCell::new(None) };
}

pub struct OverlayHandle {
    shared: Arc<Mutex<Option<Input>>>,
    hub: Arc<AtomicIsize>,
    shutdown: Arc<AtomicBool>,
    toast_request: Arc<Mutex<Option<String>>>,
    runtime: Arc<Mutex<OverlayRuntimeState>>,
    cost: Arc<OverlayCost>,
}

impl OverlayHandle {
    pub fn start(proxy: EventLoopProxy<AppEvent>) -> Self {
        let shared = Arc::new(Mutex::new(None));
        let hub = Arc::new(AtomicIsize::new(0));
        let shutdown = Arc::new(AtomicBool::new(false));
        let toast_request = Arc::new(Mutex::new(None));
        let runtime = Arc::new(Mutex::new(OverlayRuntimeState::default()));
        let cost = Arc::new(OverlayCost::default());
        {
            let shared = shared.clone();
            let hub = hub.clone();
            let shutdown = shutdown.clone();
            let toast_request = toast_request.clone();
            let runtime = runtime.clone();
            let cost = cost.clone();
            thread::spawn(move || {
                if let Err(err) = run(proxy, shared, hub.clone(), shutdown, toast_request, runtime, cost) {
                    crate::logging::log(&format!("overlay: thread stopped: {err}"));
                }
                hub.store(0, Ordering::SeqCst);
            });
        }
        Self { shared, hub, shutdown, toast_request, runtime, cost }
    }

    #[cfg(test)]
    pub fn headless() -> Self {
        Self {
            shared: Arc::new(Mutex::new(None)),
            hub: Arc::new(AtomicIsize::new(0)),
            shutdown: Arc::new(AtomicBool::new(false)),
            toast_request: Arc::new(Mutex::new(None)),
            runtime: Arc::new(Mutex::new(OverlayRuntimeState::default())),
            cost: Arc::new(OverlayCost::default()),
        }
    }

    /// Ask the overlay thread to show a transient pill. The pill is independent
    /// of the crosshair, so it works with Aim off and while menus hide the
    /// crosshair.
    pub fn toast(&self, text: String) {
        *self.toast_request.lock().unwrap_or_else(|e| e.into_inner()) = Some(text);
        let hub = self.hub.load(Ordering::SeqCst);
        if hub != 0 {
            unsafe {
                let _ = PostMessageW(Some(HWND(hub as *mut _)), WM_APP_TOAST, WPARAM(0), LPARAM(0));
            }
        }
    }

    pub fn update(&self, input: Input) {
        note_input(&self.runtime, input.config.enabled);
        *self.shared.lock().unwrap_or_else(|e| e.into_inner()) = Some(input);
        self.post();
    }

    #[cfg(test)]
    pub fn state(&self) -> OverlayRuntimeState {
        self.runtime.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Deltas since the previous call; the peak paint covers that window only.
    pub fn cost_snapshot(&self) -> OverlayCostSnapshot {
        self.cost.take_snapshot()
    }

    pub fn is_visible(&self) -> bool {
        self.runtime.lock().unwrap_or_else(|e| e.into_inner()).visible
    }

    #[cfg(test)]
    pub fn record_render(&self) {
        mark_rendered(&self.runtime);
    }

    fn post(&self) {
        let hub = self.hub.load(Ordering::SeqCst);
        if hub != 0 {
            unsafe {
                let _ = PostMessageW(Some(HWND(hub as *mut c_void)), WM_APP_UPDATE, WPARAM(0), LPARAM(0));
            }
        }
    }

    pub fn shutdown(&self) {
        self.shutdown.store(true, Ordering::SeqCst);
        self.post();
    }
}

impl Drop for OverlayHandle {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(Some(0)).collect()
}

fn run(
    proxy: EventLoopProxy<AppEvent>,
    shared: Arc<Mutex<Option<Input>>>,
    hub_atomic: Arc<AtomicIsize>,
    shutdown: Arc<AtomicBool>,
    toast_request: Arc<Mutex<Option<String>>>,
    runtime: Arc<Mutex<OverlayRuntimeState>>,
    cost: Arc<OverlayCost>,
) -> windows::core::Result<()> {
    unsafe {
        let module = GetModuleHandleW(None)?;
        let hinstance: HINSTANCE = module.into();
        let class = wide(CLASS_NAME);
        let wc = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: hinstance,
            lpszClassName: PCWSTR(class.as_ptr()),
            ..Default::default()
        };
        if RegisterClassW(&wc) == 0 {
            let err = GetLastError();
            if err.0 != 1410 {
                crate::logging::log(&format!("overlay: RegisterClassW failed ({err:?})"));
            }
        }
        let hub = CreateWindowExW(
            WS_EX_TOOLWINDOW,
            PCWSTR(class.as_ptr()),
            PCWSTR(wide("Crosshair+ overlay hub").as_ptr()),
            WS_POPUP,
            0,
            0,
            0,
            0,
            None,
            None,
            Some(hinstance),
            None,
        )?;
        hub_atomic.store(hub.0 as isize, Ordering::SeqCst);
        if let Err(err) = WTSRegisterSessionNotification(hub, 0) {
            crate::logging::log(&format!("overlay: WTS session notifications unavailable: {err}"));
        }
        let renderer = match Renderer::new() {
            Ok(r) => Some(r),
            Err(err) => {
                crate::logging::log(&format!("overlay: renderer init failed: {err}"));
                let _ = proxy.send_event(AppEvent::Overlay(OverlayEvent::Diagnostic {
                    code: "overlay_init".into(),
                    message: format!("The crosshair renderer could not start: {err}"),
                }));
                None
            }
        };
        CTX.with(|slot| {
            *slot.borrow_mut() = Some(Ctx {
                hub,
                overlay: None,
                hinstance,
                proxy: proxy.clone(),
                shared: shared.clone(),
                shutdown: shutdown.clone(),
                renderer,
                target: None,
                target_size: (0, 0),
                registered: Vec::new(),
                hotkey_sig: Vec::new(),
                win_event: None,
                visible: false,
                timer_running: false,
                osd: None,
                osd_target: None,
                osd_size: (0, 0),
                toast: None,
                osd_timer_running: false,
                toast_request,
                runtime,
                cost,
            });
        });
        let win_event = SetWinEventHook(
            EVENT_SYSTEM_FOREGROUND,
            EVENT_SYSTEM_FOREGROUND,
            None,
            Some(foreground_hook),
            0,
            0,
            WINEVENT_OUTOFCONTEXT,
        );
        let win_event = if win_event.is_invalid() { None } else { Some(win_event) };
        CTX.with(|slot| {
            if let Some(ctx) = slot.borrow_mut().as_mut() {
                ctx.win_event = win_event;
            }
        });
        refresh();
        let mut msg = MSG::default();
        loop {
            let result = GetMessageW(&mut msg, None, 0, 0);
            if result.0 == 0 {
                break;
            }
            if result.0 == -1 {
                crate::logging::log("overlay: GetMessageW failed");
                break;
            }
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        CTX.with(|slot| {
            if let Some(ctx) = slot.borrow_mut().as_mut() {
                for id in ctx.registered.drain(..) {
                    let _ = UnregisterHotKey(Some(ctx.hub), id);
                }
                if let Some(hook) = ctx.win_event.take() {
                    let _ = UnhookWinEvent(hook);
                }
                if let Some(overlay) = ctx.overlay.take() {
                    let _ = ShowWindow(overlay, SW_HIDE);
                }
                if let Some(osd) = ctx.osd.take() {
                    let _ = ShowWindow(osd, SW_HIDE);
                }
            }
        });
    }
    Ok(())
}

unsafe extern "system" fn foreground_hook(
    _hook: HWINEVENTHOOK,
    event: u32,
    _hwnd: HWND,
    _id_object: i32,
    _id_child: i32,
    _id_thread: u32,
    _event_time: u32,
) {
    if event == EVENT_SYSTEM_FOREGROUND {
        CTX.with(|slot| {
            if let Some(ctx) = slot.borrow().as_ref() {
                let _ = ctx.proxy.send_event(AppEvent::Overlay(OverlayEvent::ForegroundChanged));
            }
        });
    }
}

fn power_event(wparam: usize) -> Option<OverlayEvent> {
    match wparam as u32 {
        PBT_APMSUSPEND => Some(OverlayEvent::PowerSuspend),
        PBT_APMRESUMEAUTOMATIC => Some(OverlayEvent::PowerResume),
        _ => None,
    }
}

fn session_event(wparam: usize) -> Option<OverlayEvent> {
    (wparam as u32 == WTS_SESSION_UNLOCK).then_some(OverlayEvent::SessionUnlock)
}

fn end_session_requested(wparam: usize) -> bool {
    wparam != 0
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_APP_UPDATE => {
                refresh();
                LRESULT(0)
            }
            WM_APP_TOAST => {
                let text = CTX.with(|slot| {
                    slot.borrow()
                        .as_ref()
                        .and_then(|ctx| ctx.toast_request.lock().unwrap_or_else(|e| e.into_inner()).take())
                });
                if let Some(text) = text {
                    start_toast(text);
                }
                LRESULT(0)
            }
            WM_TIMER => {
                if wparam.0 == TIMER_OSD {
                    tick_osd();
                } else {
                    tick();
                }
                LRESULT(0)
            }
            WM_HOTKEY => {
                let id = wparam.0 as i32;
                CTX.with(|slot| {
                    if let Some(ctx) = slot.borrow().as_ref() {
                        let _ = ctx.proxy.send_event(AppEvent::Overlay(OverlayEvent::Hotkey { id }));
                    }
                });
                LRESULT(0)
            }
            WM_DISPLAYCHANGE => {
                CTX.with(|slot| {
                    if let Some(ctx) = slot.borrow().as_ref() {
                        let _ = ctx.proxy.send_event(AppEvent::Overlay(OverlayEvent::DisplayChanged));
                    }
                });
                LRESULT(0)
            }
            WM_POWERBROADCAST => {
                if let Some(event) = power_event(wparam.0) {
                    CTX.with(|slot| {
                        if let Some(ctx) = slot.borrow().as_ref() {
                            let _ = ctx.proxy.send_event(AppEvent::Overlay(event));
                        }
                    });
                }
                LRESULT(1)
            }
            WM_WTSSESSION_CHANGE => {
                if let Some(event) = session_event(wparam.0) {
                    CTX.with(|slot| {
                        if let Some(ctx) = slot.borrow().as_ref() {
                            let _ = ctx.proxy.send_event(AppEvent::Overlay(event));
                        }
                    });
                }
                LRESULT(0)
            }
            WM_QUERYENDSESSION => LRESULT(1),
            WM_ENDSESSION => {
                if end_session_requested(wparam.0) {
                    crate::safety::restore_all_blocking();
                    CTX.with(|slot| {
                        if let Some(ctx) = slot.borrow().as_ref() {
                            let _ = ctx.proxy.send_event(AppEvent::Overlay(OverlayEvent::EndSession));
                        }
                    });
                }
                LRESULT(0)
            }
            windows::Win32::UI::WindowsAndMessaging::WM_DESTROY => {
                PostQuitMessage(0);
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}

fn refresh() {
    CTX.with(|slot| {
        let mut borrow = slot.borrow_mut();
        let Some(ctx) = borrow.as_mut() else { return };
        let input = ctx.shared.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let Some(input) = input else { return };
        if ctx.shutdown.load(Ordering::SeqCst) {
            if let Some(overlay) = ctx.overlay {
                let _ = unsafe { ShowWindow(overlay, SW_HIDE) };
            }
            unsafe {
                let _ = PostMessageW(Some(ctx.hub), windows::Win32::UI::WindowsAndMessaging::WM_CLOSE, WPARAM(0), LPARAM(0));
            }
            return;
        }
        let sig: Vec<_> = input.hotkeys.iter().map(|h| (h.id, h.mods, h.vk, h.key.clone(), h.slot_id.clone())).collect();
        if sig != ctx.hotkey_sig {
            register_all(ctx, &input.hotkeys);
            ctx.hotkey_sig = sig;
        }
        if !input.config.enabled {
            if ctx.visible {
                crate::logging::log("overlay: hidden because Aim is off");
            }
            hide(ctx);
            stop_timer(ctx);
            return;
        }
        if !ctx.timer_running {
            let timer = unsafe { SetTimer(Some(ctx.hub), TIMER_RENDER, 30, None) };
            ctx.timer_running = timer != 0;
            if !ctx.timer_running {
                crate::logging::log("overlay: SetTimer failed; overlay will not repaint");
            }
        }
        if hidden_by_policy(&input.config) {
            hide(ctx);
            return;
        }
        if let Err(err) = render(ctx, &input) {
            crate::logging::log(&format!("overlay: render failed: {err}"));
            hide(ctx);
        }
    });
}

fn tick() {
    CTX.with(|slot| {
        let mut borrow = slot.borrow_mut();
        let Some(ctx) = borrow.as_mut() else { return };
        if ctx.shutdown.load(Ordering::SeqCst) {
            stop_timer(ctx);
            unsafe {
                let _ = PostMessageW(Some(ctx.hub), windows::Win32::UI::WindowsAndMessaging::WM_CLOSE, WPARAM(0), LPARAM(0));
            }
            return;
        }
        let input = ctx.shared.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let Some(input) = input else { return };
        if !input.config.enabled {
            hide(ctx);
            stop_timer(ctx);
            return;
        }
        if hidden_by_policy(&input.config) {
            hide(ctx);
            return;
        }
        if let Err(err) = render(ctx, &input) {
            crate::logging::log(&format!("overlay: render failed: {err}"));
            hide(ctx);
        }
    });
}

fn register_all(ctx: &mut Ctx, bindings: &[HotkeyBinding]) {
    for id in ctx.registered.drain(..) {
        unsafe {
            let _ = UnregisterHotKey(Some(ctx.hub), id);
        }
    }
    for binding in bindings {
        let result = unsafe {
            RegisterHotKey(Some(ctx.hub), binding.id, windows::Win32::UI::Input::KeyboardAndMouse::HOT_KEY_MODIFIERS(binding.mods | MOD_NOREPEAT.0), binding.vk)
        };
        match result {
            Ok(()) => {
                crate::logging::log(&format!("overlay: registered hotkey {}", binding.key));
                ctx.registered.push(binding.id)
            }
            Err(err) => {
                crate::logging::log(&format!("overlay: hotkey {} was rejected: {err}", binding.key));
                let _ = ctx.proxy.send_event(AppEvent::Overlay(OverlayEvent::HotkeyConflict {
                    slot_id: binding.slot_id.clone(),
                    key: binding.key.clone(),
                    owner: "another application".into(),
                }));
            }
        }
    }
}

fn ensure_overlay(ctx: &mut Ctx) -> bool {
    if ctx.overlay.is_some() {
        return true;
    }
    let class = wide(CLASS_NAME);
    let ex = WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE;
    let result = unsafe {
        CreateWindowExW(
            ex,
            PCWSTR(class.as_ptr()),
            PCWSTR(wide("Crosshair+ overlay").as_ptr()),
            WS_POPUP | WS_VISIBLE,
            0,
            0,
            1,
            1,
            None,
            None,
            Some(ctx.hinstance),
            None,
        )
    };
    match result {
        Ok(hwnd) => {
            ctx.overlay = Some(hwnd);
            true
        }
        Err(err) => {
            crate::logging::log(&format!("overlay: CreateWindowExW failed: {err}"));
            false
        }
    }
}

fn ensure_osd(ctx: &mut Ctx) -> Option<HWND> {
    if let Some(hwnd) = ctx.osd {
        return Some(hwnd);
    }
    let class = wide(CLASS_NAME);
    let ex = WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE;
    let result = unsafe {
        CreateWindowExW(
            ex,
            PCWSTR(class.as_ptr()),
            PCWSTR(wide("Crosshair+ notification").as_ptr()),
            WS_POPUP,
            0,
            0,
            1,
            1,
            None,
            None,
            Some(ctx.hinstance),
            None,
        )
    };
    match result {
        Ok(hwnd) => {
            ctx.osd = Some(hwnd);
            Some(hwnd)
        }
        Err(err) => {
            crate::logging::log(&format!("overlay: OSD CreateWindowExW failed: {err}"));
            None
        }
    }
}

fn start_toast(text: String) {
    CTX.with(|slot| {
        let mut borrow = slot.borrow_mut();
        let Some(ctx) = borrow.as_mut() else { return };
        ctx.toast = Some(Toast { text, started: Instant::now() });
        if !ctx.osd_timer_running {
            let timer = unsafe { SetTimer(Some(ctx.hub), TIMER_OSD, 16, None) };
            ctx.osd_timer_running = timer != 0;
            if !ctx.osd_timer_running {
                crate::logging::log("overlay: OSD SetTimer failed; the pill will not fade");
            }
        }
        if let Err(err) = paint_osd(ctx) {
            crate::logging::log(&format!("overlay: OSD paint failed: {err}"));
            ctx.toast = None;
            stop_osd_timer(ctx);
        }
    });
}

fn tick_osd() {
    CTX.with(|slot| {
        let mut borrow = slot.borrow_mut();
        let Some(ctx) = borrow.as_mut() else { return };
        let elapsed = ctx.toast.as_ref().map(|toast| toast.started.elapsed().as_millis()).unwrap_or(TOAST_MS);
        if elapsed >= TOAST_MS {
            ctx.toast = None;
            stop_osd_timer(ctx);
            if let Some(hwnd) = ctx.osd {
                unsafe {
                    let _ = ShowWindow(hwnd, SW_HIDE);
                }
            }
            return;
        }
        if let Err(err) = paint_osd(ctx) {
            crate::logging::log(&format!("overlay: OSD paint failed: {err}"));
            ctx.toast = None;
            stop_osd_timer(ctx);
        }
    });
}

fn stop_osd_timer(ctx: &mut Ctx) {
    if ctx.osd_timer_running {
        unsafe {
            let _ = KillTimer(Some(ctx.hub), TIMER_OSD);
        }
        ctx.osd_timer_running = false;
    }
}

fn foreground_monitor() -> Option<(i32, i32, u32, u32, f32)> {
    unsafe {
        let monitor = MonitorFromWindow(GetForegroundWindow(), MONITOR_DEFAULTTOPRIMARY);
        if monitor.0.is_null() {
            return None;
        }
        let mut info = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        if !GetMonitorInfoW(monitor, &mut info as *mut _ as *mut MONITORINFO).as_bool() {
            return None;
        }
        let mut dpi_x = 96u32;
        let mut dpi_y = 96u32;
        let _ = GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y);
        let scale = (dpi_x as f32 / 96.0).clamp(1.0, 3.0);
        Some((
            info.rcMonitor.left,
            info.rcMonitor.top,
            (info.rcMonitor.right - info.rcMonitor.left).max(1) as u32,
            (info.rcMonitor.bottom - info.rcMonitor.top).max(1) as u32,
            scale,
        ))
    }
}

fn paint_osd(ctx: &mut Ctx) -> windows::core::Result<()> {
    let Some(text) = ctx.toast.as_ref().map(|toast| toast.text.clone()) else { return Ok(()) };
    let Some((monitor_x, monitor_y, monitor_w, _monitor_h, scale)) = foreground_monitor() else {
        return Ok(());
    };
    let Some(hwnd) = ensure_osd(ctx) else { return Ok(()) };
    let Some(renderer) = ctx.renderer.as_ref() else { return Ok(()) };
    let font_size = 14.0 * scale;
    let height = (40.0 * scale).max(28.0);
    let padding = 16.0 * scale;
    let max_text = (monitor_w as f32 - 48.0 * scale).max(80.0);
    let layout = renderer.engine.text(
        "Segoe UI",
        font_size,
        &text,
        Rgba::rgb(0xFFFFFF),
        max_text,
        height,
        DWRITE_FONT_WEIGHT_SEMI_BOLD,
    )?;
    unsafe {
        layout.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_LEADING)?;
        layout.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER)?;
    }
    let mut metrics = DWRITE_TEXT_METRICS::default();
    unsafe {
        layout.GetMetrics(&mut metrics)?;
    }
    let text_width = metrics.widthIncludingTrailingWhitespace.clamp(1.0, max_text);
    let width = (text_width + padding * 2.0).ceil().max(120.0 * scale).min(max_text + padding * 2.0);
    let width_u = width as u32;
    let height_u = height as u32;
    if ctx.osd_target.is_none() || ctx.osd_size != (width_u, height_u) {
        ctx.osd_target = Some(renderer.engine.target(width_u, height_u)?);
        ctx.osd_size = (width_u, height_u);
    }
    let target = ctx.osd_target.as_ref().expect("OSD target was just created");
    renderer.engine.begin(target, Rgba::rgb(0).alpha(0.0));
    let background = renderer.engine.brush(Rgba::rgb(0x000000).alpha(0.90))?;
    let border = renderer.engine.brush(Rgba::rgb(0xFFFFFF).alpha(0.20))?;
    let rounded = D2D1_ROUNDED_RECT {
        rect: D2D_RECT_F { left: 0.0, top: 0.0, right: width, bottom: height },
        radiusX: height * 0.5,
        radiusY: height * 0.5,
    };
    unsafe {
        renderer.engine.ctx.FillRoundedRectangle(&rounded, &background);
        renderer.engine.ctx.DrawRoundedRectangle(&rounded, &border, scale, None);
    }
    renderer.engine.draw_text(&layout, Rgba::rgb(0xFFFFFF), padding, 0.0, true)?;
    renderer.engine.end()?;
    let (_, mut premultiplied) = renderer.engine.readback(target)?;
    let elapsed = ctx.toast.as_ref().map(|toast| toast.started.elapsed().as_millis()).unwrap_or(TOAST_MS) as f32;
    let opacity = if elapsed < 120.0 {
        (elapsed / 120.0).clamp(0.0, 1.0)
    } else if elapsed > 1200.0 {
        ((TOAST_MS as f32 - elapsed) / 300.0).clamp(0.0, 1.0)
    } else {
        1.0
    };
    if opacity < 1.0 {
        for pixel in premultiplied.chunks_exact_mut(4) {
            for channel in &mut pixel[..3] {
                *channel = (*channel as f32 * opacity).round().clamp(0.0, 255.0) as u8;
            }
            pixel[3] = (pixel[3] as f32 * opacity).round().clamp(0.0, 255.0) as u8;
        }
    }
    let x = monitor_x + (monitor_w as i32 - width_u as i32) / 2;
    let y = monitor_y + (48.0 * scale) as i32;
    update_layered(hwnd, x, y, width_u, height_u, &premultiplied)?;
    unsafe {
        let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
    }
    Ok(())
}

fn render(ctx: &mut Ctx, input: &Input) -> windows::core::Result<()> {
    let paint_started = Instant::now();
    let Some(rect) = input.rect else { return Ok(()) };
    if !ensure_overlay(ctx) {
        return Ok(());
    }
    let Some(renderer) = ctx.renderer.as_mut() else { return Ok(()) };
    renderer.sync_image(&input.config);
    let (half_w, half_h) = renderer.content_half_size(&input.config);
    let padding = 18.0;
    let mut width = ((half_w * 2.0 + padding * 2.0).ceil() as i32).max(24);
    let mut height = ((half_h * 2.0 + padding * 2.0).ceil() as i32).max(24);
    width = width.min((rect.2 as i32).max(24));
    height = height.min((rect.3 as i32).max(24));
    let width_u = width as u32;
    let height_u = height as u32;
    let center_x = rect.0 + rect.2 as i32 / 2 + input.config.offset_x;
    let center_y = rect.1 + rect.3 as i32 / 2 + input.config.offset_y;
    let left = center_x - width / 2;
    let top = center_y - height / 2;
    if let Err(err) = unsafe {
        SetWindowPos(
            ctx.overlay.unwrap(),
            Some(HWND_TOPMOST),
            left,
            top,
            width,
            height,
            SWP_NOACTIVATE | SWP_SHOWWINDOW,
        )
        } {
        crate::logging::log(&format!("overlay: SetWindowPos failed: {err}"));
        return Err(err);
    }

    if ctx.target_size != (width_u, height_u) || ctx.target.is_none() {
        ctx.target = Some(renderer.engine.target(width_u, height_u)?);
        ctx.target_size = (width_u, height_u);
    }
    let target = ctx.target.as_ref().unwrap();
    renderer.engine.begin(target, Rgba::rgb(0x000000).alpha(0.0));
    let center = windows_numerics::Vector2 { X: width as f32 * 0.5, Y: height as f32 * 0.5 };
    let draw = renderer.draw(&renderer.engine.ctx, &input.config, center, crate::render::now_ms());
    let end = renderer.engine.end();
    draw?;
    end?;
    let (_, premul) = renderer.engine.readback(target)?;
    let overlay = ctx.overlay.unwrap();
    if let Err(err) = update_layered(overlay, left, top, width_u, height_u, &premul) {
        crate::logging::log(&format!("overlay: UpdateLayeredWindow failed: {err}"));
        return Err(err);
    }
    unsafe {
        let _ = ShowWindow(overlay, SW_SHOWNOACTIVATE);
    }
    if !ctx.visible {
        crate::logging::log(&format!("overlay: shown at {left},{top} {width}x{height} on {}", input.monitor_label));
    }
    ctx.visible = true;
    mark_rendered(&ctx.runtime);
    ctx.cost.record(paint_started.elapsed(), thread_cpu_100ns());
    Ok(())
}

/// CPU time this overlay thread has used so far, in 100 ns units: the closest
/// attribution of what the crosshair itself costs.
fn thread_cpu_100ns() -> u64 {
    use windows::Win32::Foundation::FILETIME;
    use windows::Win32::System::Threading::{GetCurrentThread, GetThreadTimes};

    let zero = || FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 };
    let (mut created, mut exited, mut kernel, mut user) = (zero(), zero(), zero(), zero());
    let opened = unsafe { GetThreadTimes(GetCurrentThread(), &mut created, &mut exited, &mut kernel, &mut user) };
    if opened.is_err() {
        return 0;
    }
    crate::app::filetime_100ns(kernel) + crate::app::filetime_100ns(user)
}

fn update_layered(hwnd: HWND, x: i32, y: i32, width: u32, height: u32, premultiplied: &[u8]) -> windows::core::Result<()> {
    unsafe {
        let screen = GetDC(None);
        if screen.0.is_null() {
            return Err(windows::core::Error::from(windows::Win32::Foundation::E_FAIL));
        }
        let mem = CreateCompatibleDC(Some(screen));
        if mem.0.is_null() {
            ReleaseDC(None, screen);
            return Err(windows::core::Error::from(windows::Win32::Foundation::E_FAIL));
        }
        let bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: width as i32,
                biHeight: -(height as i32),
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits: *mut c_void = std::ptr::null_mut();
        let dib = match CreateDIBSection(Some(mem), &bmi, DIB_RGB_COLORS, &mut bits, None, 0) {
            Ok(dib) => dib,
            Err(err) => {
                let _ = DeleteDC(mem);
                ReleaseDC(None, screen);
                return Err(err);
            }
        };
        if dib.0.is_null() || bits.is_null() {
            if !dib.0.is_null() {
                let _ = DeleteObject(dib.into());
            }
            let _ = DeleteDC(mem);
            ReleaseDC(None, screen);
            return Err(windows::core::Error::from(windows::Win32::Foundation::E_FAIL));
        }
        let copy_len = (width * height * 4) as usize;
        if premultiplied.len() < copy_len {
            let _ = DeleteObject(dib.into());
            let _ = DeleteDC(mem);
            ReleaseDC(None, screen);
            return Err(windows::core::Error::from(windows::Win32::Foundation::E_FAIL));
        }
        let old = SelectObject(mem, dib.into());
        let dst = POINT { x, y };
        let size = SIZE { cx: width as i32, cy: height as i32 };
        let src = POINT { x: 0, y: 0 };
        let blend = BLENDFUNCTION {
            BlendOp: AC_SRC_OVER as u8,
            BlendFlags: 0,
            SourceConstantAlpha: 255,
            AlphaFormat: AC_SRC_ALPHA as u8,
        };
        std::ptr::copy_nonoverlapping(premultiplied.as_ptr(), bits as *mut u8, copy_len);
        let result = UpdateLayeredWindow(
            hwnd,
            Some(screen),
            Some(&dst),
            Some(&size),
            Some(mem),
            Some(&src),
            COLORREF(0),
            Some(&blend),
            ULW_ALPHA,
        );
        SelectObject(mem, old);
        let _ = DeleteObject(dib.into());
        let _ = DeleteDC(mem);
        ReleaseDC(None, screen);
        result
    }
}

fn hide(ctx: &mut Ctx) {
    if let Some(overlay) = ctx.overlay {
        unsafe {
            let _ = ShowWindow(overlay, SW_HIDE);
        }
    }
    ctx.visible = false;
    let mut state = ctx.runtime.lock().unwrap_or_else(|e| e.into_inner());
    state.visible = false;
    state.hidden = true;
}

fn note_input(runtime: &Arc<Mutex<OverlayRuntimeState>>, enabled: bool) {
    let mut state = runtime.lock().unwrap_or_else(|e| e.into_inner());
    state.enabled = enabled;
    state.render_requests += 1;
    if enabled {
        state.hidden = false;
    } else {
        state.visible = false;
        state.hidden = true;
    }
}

fn mark_rendered(runtime: &Arc<Mutex<OverlayRuntimeState>>) {
    let mut state = runtime.lock().unwrap_or_else(|e| e.into_inner());
    state.visible = true;
    state.hidden = false;
    state.renders += 1;
}

fn stop_timer(ctx: &mut Ctx) {
    if ctx.timer_running {
        unsafe {
            let _ = KillTimer(Some(ctx.hub), TIMER_RENDER);
        }
        ctx.timer_running = false;
    }
}

/// The visibility rule is pure so all three modes and the menu precedence can
/// be tested without a window station. Menus always win: `Only on Aim` must not
/// paint the crosshair over Start, a taskbar menu or an app menu.
pub(crate) fn aim_hidden(visibility: &str, hide_in_menus: bool, rmb_down: bool, menu_open: bool) -> bool {
    if hide_in_menus && menu_open {
        return true;
    }
    match visibility {
        "hide" => rmb_down,
        "only" => !rmb_down,
        _ => false,
    }
}

fn hidden_by_policy(config: &CrosshairConfig) -> bool {
    let visibility = config
        .aim_visibility
        .as_deref()
        .filter(|value| matches!(*value, "always" | "hide" | "only"))
        .unwrap_or(if config.hide_on_rmb { "hide" } else { "always" });
    let rmb_down = unsafe {
        let state = GetAsyncKeyState(VK_RBUTTON.0 as i32);
        (state as u16 & 0x8000) != 0
    };
    let menu = config.hide_in_menus && menu_open();
    aim_hidden(visibility, config.hide_in_menus, rmb_down, menu)
}

fn menu_open() -> bool {
    unsafe {
        let mut cursor = CURSORINFO { cbSize: std::mem::size_of::<CURSORINFO>() as u32, ..Default::default() };
        let _ = GetCursorInfo(&mut cursor);
        if cursor.flags.0 & CURSOR_SHOWING.0 == 0 {
            return false;
        }
        // Classic Win32 menus report through the foreground GUI thread.
        // GetGUIThreadInfo(0) follows the active window, which can remain a
        // non-menu owner while another thread owns the popup, so check both
        // the active thread and the actual foreground thread.
        let foreground = GetForegroundWindow();
        let foreground_thread = GetWindowThreadProcessId(foreground, None);
        for thread in [0, foreground_thread] {
            let mut gui = GUITHREADINFO { cbSize: std::mem::size_of::<GUITHREADINFO>() as u32, ..Default::default() };
            if GetGUIThreadInfo(thread, &mut gui).is_ok() {
                let mask = GUI_INMENUMODE.0 | GUI_POPUPMENUMODE.0 | GUI_SYSTEMMENUMODE.0;
                if gui.flags.0 & mask != 0 {
                    return true;
                }
            }
        }
        // Windows 11 shell menus (desktop, taskbar, Start) are WinUI popup
        // hosts and do not set GUI_INMENUMODE. Detect their visible popup
        // window directly. Classic #32768 menus are covered too.
        let open = menu_popup_open();
        static LOGGED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        if open && !LOGGED.swap(true, std::sync::atomic::Ordering::Relaxed) {
            crate::logging::log("overlay: menu popup detected");
        } else if !open {
            LOGGED.store(false, std::sync::atomic::Ordering::Relaxed);
        }
        open
    }
}

fn menu_popup_open() -> bool {
    unsafe extern "system" fn callback(hwnd: HWND, lparam: LPARAM) -> BOOL {
        let found = &mut *(lparam.0 as *mut bool);
        if !IsWindowVisible(hwnd).as_bool() {
            return true.into();
        }
        let mut class_name = [0u16; 128];
        let len = GetClassNameW(hwnd, &mut class_name);
        if len <= 0 {
            return true.into();
        }
        let class_name = String::from_utf16_lossy(&class_name[..len as usize]);
        if class_name == "#32768" || class_name == "Microsoft.UI.Content.PopupWindowSiteBridge" {
            let mut rect = RECT::default();
            if GetWindowRect(hwnd, &mut rect).is_ok() && rect.right > rect.left && rect.bottom > rect.top {
                *found = true;
                return false.into();
            }
        }
        true.into()
    }
    let mut found = false;
    let _ = unsafe { EnumWindows(Some(callback), LPARAM(&mut found as *mut bool as isize)) };
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn three_way_visibility_and_menu_precedence() {
        assert!(!aim_hidden("always", false, true, false));
        assert!(!aim_hidden("always", false, false, false));
        assert!(aim_hidden("hide", false, true, false));
        assert!(!aim_hidden("hide", false, false, false));
        assert!(aim_hidden("only", false, false, false));
        assert!(!aim_hidden("only", false, true, false));
        assert!(aim_hidden("only", true, true, true), "a menu hides even while aiming");
        assert!(aim_hidden("hide", true, false, true));
        assert!(!aim_hidden("hide", false, false, true), "menus only apply when enabled");
    }

    #[test]
    fn power_and_session_messages_map_to_the_expected_events() {
        assert!(matches!(power_event(PBT_APMSUSPEND as usize), Some(OverlayEvent::PowerSuspend)));
        assert!(matches!(power_event(PBT_APMRESUMEAUTOMATIC as usize), Some(OverlayEvent::PowerResume)));
        assert!(power_event(0xFFFF).is_none());
        assert!(matches!(session_event(WTS_SESSION_UNLOCK as usize), Some(OverlayEvent::SessionUnlock)));
        assert!(session_event(0).is_none());
        assert!(end_session_requested(1));
        assert!(!end_session_requested(0));
    }
}
