use std::fs::{self, OpenOptions};
use std::io::Write;
use std::sync::{Mutex, OnceLock};

use windows::Win32::System::SystemInformation::GetLocalTime;

use crate::config::log_path;

static LOCK: OnceLock<Mutex<()>> = OnceLock::new();

pub fn init() {
    crate::config::ensure_dirs();
    let _ = LOCK.set(Mutex::new(()));
    rotate(&log_file_path());
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        log(&format!("PANIC: {info}"));
        log("safety: panic hook invoking restore");
        crate::safety::restore_all_blocking();
        // A sandboxed child test must not block on a modal dialog. Normal
        // launches still show the recovery message.
        if crate::config::sandbox_root().is_none() {
            let message = format!(
                "Crosshair+ hit an unexpected error and has restored your display and audio settings.\n\nLog: {}\n\n{info}",
                crate::config::log_path().display()
            );
            let wide: Vec<u16> = message.encode_utf16().chain(Some(0)).collect();
            unsafe {
                windows::Win32::UI::WindowsAndMessaging::MessageBoxW(
                    None,
                    windows::core::PCWSTR(wide.as_ptr()),
                    windows::core::w!("Crosshair+"),
                    windows::Win32::UI::WindowsAndMessaging::MB_ICONERROR | windows::Win32::UI::WindowsAndMessaging::MB_OK,
                );
            }
        }
        previous(info);
    }));
    log("=== Crosshair+ starting ===");
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn child_sandbox(label: &str) -> std::path::PathBuf {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let path = std::env::temp_dir().join(format!(
            "crosshair-plus-restore-{label}-{}-{stamp}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        path
    }

    fn run_child(test_name: &str, mode: &str, sandbox: &std::path::Path) -> std::process::Output {
        Command::new(std::env::current_exe().unwrap())
            .args(["--exact", test_name, "--test-threads=1"])
            .env("CROSSHAIRPLUS_SANDBOX_DIR", sandbox)
            .env("CROSSHAIRPLUS_RESTORE_CHILD", mode)
            .output()
            .unwrap()
    }

    #[test]
    fn unit_tests_never_append_to_the_live_session_log() {
        if crate::config::sandbox_root().is_some() {
            return;
        }
        assert_ne!(log_file_path(), crate::config::log_path(), "unit runs must not write the user's log");
        log("test: unit log routing");
        let text = fs::read_to_string(log_file_path()).unwrap_or_default();
        assert!(text.contains("test: unit log routing"), "unit logging must still be readable: {text}");
    }

    #[test]
    fn panic_child_under_test_env() {
        if std::env::var("CROSSHAIRPLUS_RESTORE_CHILD").ok().as_deref() != Some("panic") {
            return;
        }
        init();
        panic!("sandboxed panic hook test");
    }

    #[test]
    fn normal_exit_child_under_test_env() {
        if std::env::var("CROSSHAIRPLUS_RESTORE_CHILD").ok().as_deref() != Some("exit") {
            return;
        }
        init();
        crate::safety::restore_for_exit(true);
        log("test: normal exit restore returned");
    }

    #[test]
    fn panic_and_exit_children_run_the_restore_skeleton() {
        let exe = std::env::current_exe().unwrap();
        assert!(exe.is_file());

        let panic_sandbox = child_sandbox("panic");
        let panic = run_child("logging::tests::panic_child_under_test_env", "panic", &panic_sandbox);
        assert!(!panic.status.success(), "panic child unexpectedly succeeded: {panic:?}");
        let panic_log = fs::read_to_string(panic_sandbox.join("data\\crosshair.log")).unwrap();
        assert!(panic_log.contains("PANIC:"), "panic hook did not write the panic line: {panic_log}");
        assert!(panic_log.contains("safety: panic hook invoking restore"), "panic hook did not reach restore: {panic_log}");
        assert!(panic_log.contains("safety: restored OS display ramps and FxSound baseline"), "panic restore did not finish: {panic_log}");
        assert!(panic_log.contains("safety: sandbox; FxSound restore skipped"), "sandbox restore touched FxSound: {panic_log}");
        let _ = fs::remove_dir_all(&panic_sandbox);

        let exit_sandbox = child_sandbox("exit");
        let exit = run_child("logging::tests::normal_exit_child_under_test_env", "exit", &exit_sandbox);
        assert!(exit.status.success(), "normal-exit child failed: {exit:?}");
        let exit_log = fs::read_to_string(exit_sandbox.join("data\\crosshair.log")).unwrap();
        assert!(exit_log.contains("safety: restored OS display ramps and FxSound baseline"), "normal exit restore did not finish: {exit_log}");
        assert!(exit_log.contains("test: normal exit restore returned"), "normal exit child did not return: {exit_log}");
        assert!(exit_log.contains("safety: sandbox; FxSound restore skipped"), "normal exit touched FxSound: {exit_log}");
        let _ = fs::remove_dir_all(&exit_sandbox);
    }
}

/// The file this process appends to. Unit tests share the live data directory
/// unless they asked for a sandbox, so they must never append to the user's
/// real session log; sandboxed child tests still write where they assert.
fn log_file_path() -> std::path::PathBuf {
    #[cfg(test)]
    if crate::config::sandbox_root().is_none() {
        let dir = std::env::temp_dir().join(format!("crosshairplus-unit-logs-{}", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        return dir.join("crosshair.log");
    }
    log_path()
}

fn rotate(path: &std::path::Path) {
    if let Ok(meta) = fs::metadata(path) {
        if meta.len() > 2 * 1024 * 1024 {
            let old = path.with_file_name("crosshair.log.1");
            let _ = fs::remove_file(&old);
            let _ = fs::rename(path, old);
        }
    }
}

pub fn log(message: &str) {
    let guard = LOCK.get_or_init(|| Mutex::new(()));
    let _lock = guard.lock().unwrap_or_else(|e| e.into_inner());
    let path = log_file_path();
    if let Some(dir) = path.parent() {
        let _ = fs::create_dir_all(dir);
    }
    rotate(&path);
    let now = unsafe { GetLocalTime() };
    let stamp = format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}.{:03}",
        now.wYear, now.wMonth, now.wDay, now.wHour, now.wMinute, now.wSecond, now.wMilliseconds
    );
    if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(file, "[{stamp}] {message}");
    }
}
