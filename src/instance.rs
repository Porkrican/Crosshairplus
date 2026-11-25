//! Single-instance guard.
//!
//! A second copy would stack an overlay, a tray icon and a config writer on top
//! of the first, so a plain launch hands over to the window that already runs.

use std::time::Duration;

use windows::core::PCWSTR;
use windows::Win32::Foundation::{CloseHandle, GetLastError, ERROR_ALREADY_EXISTS, HANDLE};
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::UI::WindowsAndMessaging::{
    FindWindowW, SetForegroundWindow, ShowWindow, SW_RESTORE, SW_SHOW,
};

/// Session-scoped, so two signed-in users each keep their own instance.
const MUTEX_NAME: &str = "Local\\CrosshairPlus-SingleInstance";
/// Matches the main window built in `app::init_window`.
const WINDOW_TITLE: &str = "Crosshair+";
/// A copy launched immediately after the first can arrive before that window
/// exists, so the lookup is retried for a few seconds.
const FOCUS_ATTEMPTS: usize = 30;
const FOCUS_RETRY: Duration = Duration::from_millis(100);

/// Held for the life of the process; closing the handle releases the mutex.
pub struct SingleInstance(HANDLE);

impl Drop for SingleInstance {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(Some(0)).collect()
}

/// Claims the instance mutex. `None` means another copy already owns it.
pub fn acquire() -> Option<SingleInstance> {
    acquire_named(MUTEX_NAME)
}

fn acquire_named(name: &str) -> Option<SingleInstance> {
    let name = wide(name);
    let handle = unsafe { CreateMutexW(None, false.into(), PCWSTR(name.as_ptr())) }.ok()?;
    // CreateMutexW still returns a usable handle for an existing mutex; the
    // error says this copy is the second one.
    if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
        unsafe {
            let _ = CloseHandle(handle);
        }
        return None;
    }
    Some(SingleInstance(handle))
}

/// Brings the running window to the front, restoring it from the tray or a
/// minimised state. False when no window answered in time.
pub fn focus_existing() -> bool {
    let title = wide(WINDOW_TITLE);
    for _ in 0..FOCUS_ATTEMPTS {
        if let Ok(window) = unsafe { FindWindowW(PCWSTR::null(), PCWSTR(title.as_ptr())) } {
            unsafe {
                let _ = ShowWindow(window, SW_SHOW);
                let _ = ShowWindow(window, SW_RESTORE);
                let _ = SetForegroundWindow(window);
            }
            return true;
        }
        std::thread::sleep(FOCUS_RETRY);
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_name(label: &str) -> String {
        format!("Local\\CrosshairPlus-Test-{label}-{}", std::process::id())
    }

    #[test]
    fn a_second_copy_cannot_claim_a_running_instance() {
        let name = test_name("guard");
        let first = acquire_named(&name).expect("the first copy claims the mutex");
        assert!(acquire_named(&name).is_none(), "a second copy must be refused");
        drop(first);
        assert!(acquire_named(&name).is_some(), "the mutex frees when its owner exits");
    }
}
