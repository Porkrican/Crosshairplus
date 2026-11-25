#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod actions;
mod audio;
mod backup;
mod config;
mod display;
mod games;
mod hotkeys;
mod instance;
mod ipc;
mod logging;
mod monitors;
mod overlay;
mod qa;
mod render;
mod safety;
mod screenshot;
mod slots;
mod tray;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let has = |flag: &str| args.iter().any(|arg| arg == flag);
    // The Unstick harness re-enters this executable as a stub FxSound CLI. It
    // must branch before any app, config or WebView initialization so the child
    // behaves like the small helper process the real CLI is.
    if has("--fxsound-stub-parent") {
        std::process::exit(audio::fxsound_stub::run_parent());
    }
    if has("--fxsound-stub") {
        // argv[0] is the executable path, not a stub option.
        std::process::exit(audio::fxsound_stub::run(&args[1..]));
    }
    let verify_preset_delete = has("--verify-preset-delete");
    let verify_unstick = has("--verify-unstick");
    if verify_unstick {
        let conflict = [
            "--capture",
            "--verify-preset-delete",
            "--verify-fxsound",
            "--verify-games-cache",
            "--interaction-test",
            "--stress",
        ]
        .iter()
        .any(|flag| has(flag));
        if conflict {
            eprintln!("--verify-unstick cannot be combined with other verification modes");
            std::process::exit(2);
        }
    }
    if verify_preset_delete {
        // Opt-in isolation for the live preset-delete check: config, logs and
        // the WebView2 profile all live under this disposable root.
        let root = std::env::temp_dir().join("CrosshairPlus-preset-delete-sandbox");
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::create_dir_all(&root);
        std::env::set_var("CROSSHAIRPLUS_SANDBOX_DIR", &root);
    } else if verify_unstick && std::env::var_os("CROSSHAIRPLUS_SANDBOX_DIR").is_none() {
        // The harness is always disposable: without an explicit root it uses its
        // own fixed sandbox, so it can never reach live config or FxSound files.
        let root = std::env::temp_dir().join("CrosshairPlus-unstick-sandbox");
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::create_dir_all(&root);
        std::env::set_var("CROSSHAIRPLUS_SANDBOX_DIR", &root);
    } else if has("--capture") && std::env::var_os("CROSSHAIRPLUS_SANDBOX_DIR").is_none() {
        // Capture writes screenshots on purpose, but its config, log and
        // WebView2 profile are disposable. A caller-supplied sandbox still wins.
        let root = std::env::temp_dir().join("CrosshairPlus-capture-sandbox");
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::create_dir_all(&root);
        std::env::set_var("CROSSHAIRPLUS_SANDBOX_DIR", &root);
    }
    crate::config::ensure_dirs();
    crate::logging::init();
    if verify_preset_delete {
        crate::logging::log(&format!("verify: sandbox at {}", crate::config::data_dir().display()));
    }
    if verify_unstick {
        std::process::exit(verify_fxsound_unstick());
    }
    if has("--verify-games-cache") {
        if has("--capture") {
            crate::logging::log("gamesverify: --verify-games-cache cannot be combined with --capture");
            std::process::exit(2);
        }
        std::process::exit(verify_games_cache());
    }
    if let Err(err) = app::run() {
        crate::logging::log(&format!("fatal: {err}"));
        std::process::exit(1);
    }
    // The stress harness exits non-zero when it finds a defect, so a scripted QA
    // run can gate on the exit status instead of scraping the log.
    if crate::qa::failed() {
        std::process::exit(1);
    }
}

use serde_json::{json, Value};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const STUB_SETTINGS: &str = "<settings><setting name=\"version\" val=\"1.2.16.0\"/></settings>";

/// Runs the real Unstick sequence twice against a stub FxSound CLI in a
/// disposable tree: Sound on must rebuild the managed preset and apply the
/// whole curve/output; Sound off must only power off and leave the user's
/// preset/output alone. It also proves an orphaned stub exits when its parent
/// disappears.
fn verify_fxsound_unstick() -> i32 {
    let Some(sandbox) = crate::config::sandbox_root() else {
        eprintln!("--verify-unstick requires CROSSHAIRPLUS_SANDBOX_DIR");
        return 2;
    };
    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(err) => {
            crate::logging::log(&format!("unstickverify: cannot locate the stub executable: {err}"));
            return 1;
        }
    };
    let stub = sandbox.join("fxsound");
    std::env::set_var(crate::audio::fxsound::STUB_DIR_ENV, &stub);
    std::env::set_var(
        crate::audio::fxsound::STUB_PARENT_ENV,
        std::process::id().to_string(),
    );

    let sound_on = run_unstick_scenario(&stub, &exe, true, 10);
    let sound_on_20 = run_unstick_scenario(&stub, &exe, true, 20);
    let sound_off = run_unstick_scenario(&stub, &exe, false, 10);
    let parent_death = check_stub_parent_exit(&sandbox, &exe);
    let ok = [&sound_on, &sound_on_20, &sound_off, &parent_death]
        .iter()
        .all(|scenario| scenario.get("ok").and_then(Value::as_bool) == Some(true));
    let report = json!({
        "ok": ok,
        "mode": "--verify-unstick",
        "sandbox": sandbox.to_string_lossy(),
        "scenarios": { "soundOn": sound_on, "soundOn20": sound_on_20, "soundOff": sound_off },
        "parentDeath": parent_death,
    });
    let path = sandbox.join("fxsound-unstick-result.json");
    if let Err(err) = std::fs::write(&path, serde_json::to_string_pretty(&report).unwrap_or_default()) {
        crate::logging::log(&format!("unstickverify: cannot write {}: {err}", path.display()));
    }
    crate::logging::log(&format!(
        "unstickverify: ok={ok} soundOn={} soundOn20={} soundOff={} parentDeath={}",
        report["scenarios"]["soundOn"]["ok"],
        report["scenarios"]["soundOn20"]["ok"],
        report["scenarios"]["soundOff"]["ok"],
        report["parentDeath"]["ok"]
    ));
    if ok { 0 } else { 1 }
}

fn unstick_audio(enabled: bool, bands: usize) -> crate::config::AudioConfig {
    let mut audio = crate::config::AudioConfig {
        enabled,
        device: Some("Stub Headphones".into()),
        ..Default::default()
    };
    audio.bands = [1.0, 2.2, 3.45, 2.75, 1.0, -2.0, -3.78, -2.45, -0.2, 1.0];
    audio.bass = 4.0;
    audio.treble = 3.0;
    audio.echo = 1.0;
    audio.surround = 2.0;
    audio.dynamic_boost = 2.5;
    audio.volume_leveling = 1.0;
    audio.filter_q = 1.6;
    audio.balance = -5.0;
    if bands == 20 {
        audio.imported = Some(crate::config::ImportedProfile {
            name: "20-band parametric fixture".into(),
            raw: String::new(),
            bands: (0..20)
                .map(|index| crate::config::ImportedBand {
                    freq: 20.0 * (20_000.0f32 / 20.0).powf(index as f32 / 19.0),
                    gain: (index as f32 - 10.0) * 0.4,
                })
                .collect(),
            parametric: true,
            preamp: -1.0,
        });
    }
    audio
}

fn run_unstick_scenario(stub: &Path, exe: &Path, enabled: bool, bands: usize) -> Value {
    match unstick_scenario(stub, exe, enabled, bands) {
        Ok(report) => report,
        Err(err) => json!({ "ok": false, "enabled": enabled, "bands": bands, "error": err }),
    }
}

fn unstick_scenario(stub: &Path, exe: &Path, enabled: bool, bands: usize) -> Result<Value, String> {
    let _ = std::fs::remove_dir_all(stub);
    std::fs::create_dir_all(stub.join("Presets"))
        .map_err(|err| format!("could not create the stub tree: {err}"))?;
    std::fs::write(stub.join("FxSound.settings"), STUB_SETTINGS)
        .map_err(|err| format!("could not seed the stub settings: {err}"))?;
    let marker_path = crate::config::data_dir().join("fxsound-preset.json");
    let _ = std::fs::remove_file(&marker_path);

    // The guard stops this scenario's stub on every exit path, including an
    // early error, so the next scenario starts from a clean tree.
    struct StubGuard;
    impl Drop for StubGuard {
        fn drop(&mut self) {
            crate::audio::fxsound::stop_stub();
        }
    }
    let _guard = StubGuard;

    Command::new(exe)
        .arg("--fxsound-stub")
        .arg("--run_minimized")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|err| format!("could not start the stub: {err}"))?;
    let mut started = false;
    for _ in 0..100 {
        if crate::audio::fxsound::running() {
            started = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    if !started {
        return Err("the initial stub did not start".into());
    }
    let before_pid = crate::audio::fxsound::stub_pid(stub);
    let before = crate::audio::fxsound::status();
    let audio = unstick_audio(enabled, bands);
    let run = crate::audio::fxsound::unstick(&audio);

    let events = std::fs::read_to_string(stub.join("events.log")).unwrap_or_default();
    let after_pid = crate::audio::fxsound::stub_pid(stub);
    let status = crate::audio::fxsound::status();
    let preset_file = stub
        .join("Presets")
        .join(format!("{}.fac", crate::audio::fxsound::MANAGED_PRESET))
        .is_file();
    let marker = marker_path.is_file();
    let starts = events.lines().filter(|line| line.starts_with("start ")).count();
    let stops = events.lines().filter(|line| line.starts_with("stop ")).count();
    let pid_changed = before_pid.is_some() && after_pid.is_some() && before_pid != after_pid;
    let staged = [
        "command --power=0",
        "--preset=Crosshair+",
        "--set_band_gain=",
        "--output=Stub Headphones",
    ]
    .iter()
    .all(|needle| events.contains(needle));
    let preserved = status.preset.as_deref() == before.preset.as_deref()
        && status.output.as_deref() == before.output.as_deref();
    let powered_off = events.contains("command --power=0");
    let curve_written = ["--preset=", "--set_band_gain=", "--output=", "--power=1"]
        .iter()
        .any(|needle| events.contains(needle));
    let actual = crate::audio::fxsound::read_status_json().unwrap_or(Value::Null);
    let comparison = crate::app::compare_fx_status(&audio, &actual);
    let result = match &run {
        Ok(status) => json!({
            "ok": true,
            "running": status.running,
            "power": status.power,
            "preset": status.preset,
            "output": status.output,
        }),
        Err(err) => json!({ "ok": false, "error": err }),
    };
    let common = run.is_ok() && starts >= 2 && stops >= 1 && pid_changed && status.running;
    let ok = if enabled {
        common
            && status.power
            && status.num_bands == bands
            && status.output.as_deref() == Some("Stub Headphones")
            && status.preset.as_deref() == Some(crate::audio::fxsound::MANAGED_PRESET)
            && comparison.get("ok").and_then(Value::as_bool) == Some(true)
            && staged
            && preset_file
            && marker
    } else {
        common
            && !status.power
            && preserved
            && powered_off
            && !curve_written
            && !preset_file
            && !marker
            && comparison.get("ok").and_then(Value::as_bool) == Some(true)
    };
    Ok(json!({
        "ok": ok,
        "enabled": enabled,
        "expectedBands": bands,
        "pids": { "before": before_pid, "after": after_pid },
        "starts": starts,
        "stops": stops,
        "status": {
            "running": status.running,
            "power": status.power,
            "numBands": status.num_bands,
            "preset": status.preset,
            "output": status.output,
        },
        "before": { "preset": before.preset, "output": before.output },
        "staged": staged,
        "preserved": preserved,
        "poweredOff": powered_off,
        "curveWritten": curve_written,
        "presetFile": preset_file,
        "ownershipMarker": marker,
        "comparison": comparison,
        "result": result,
        "events": events.lines().collect::<Vec<_>>(),
    }))
}

/// The same executable's internal parent mode starts a real stub and stays
/// alive until the harness closes its stdin. The stub must notice that
/// live-to-dead transition and clean up its own pid stamp.
fn check_stub_parent_exit(sandbox: &Path, exe: &Path) -> Value {
    let dir = sandbox.join("fxsound-parent-death");
    let _ = std::fs::remove_dir_all(&dir);
    if let Err(err) = std::fs::create_dir_all(dir.join("Presets")) {
        return json!({ "ok": false, "error": format!("could not create the parent-death tree: {err}") });
    }
    let mut parent = match Command::new(exe)
        .arg("--fxsound-stub-parent")
        .env(crate::audio::fxsound::STUB_DIR_ENV, &dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(parent) => parent,
        Err(err) => return json!({ "ok": false, "error": format!("could not start the parent process: {err}") }),
    };
    let parent_pid = parent.id();
    let Some(release) = parent.stdin.take() else {
        let _ = parent.kill();
        let _ = parent.wait();
        return json!({ "ok": false, "error": "the parent mode did not expose a release pipe", "parentPid": parent_pid });
    };
    let deadline = Instant::now() + Duration::from_secs(8);
    let mut stub_pid = None;
    let mut parent_alive = false;
    while Instant::now() < deadline {
        if let Some(pid) = crate::audio::fxsound::stub_pid(&dir) {
            if crate::audio::fxsound::process_alive(pid) {
                stub_pid = Some(pid);
                parent_alive = matches!(parent.try_wait(), Ok(None));
                break;
            }
        }
        if !matches!(parent.try_wait(), Ok(None)) {
            break;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    let Some(stub_pid) = stub_pid.filter(|_| parent_alive) else {
        drop(release);
        let _ = parent.kill();
        let _ = parent.wait();
        if let Some(pid) = crate::audio::fxsound::stub_pid(&dir) {
            let _ = Command::new("taskkill").args(["/PID", &pid.to_string(), "/F"]).output();
        }
        return json!({
            "ok": false,
            "error": "the stub did not come up while its parent mode was alive",
            "parentPid": parent_pid,
            "events": std::fs::read_to_string(dir.join("events.log")).unwrap_or_default().lines().collect::<Vec<_>>(),
        });
    };

    // Closing the pipe is the release signal; the parent mode bounds its wait
    // to five seconds, and this side gives it eight before killing it.
    drop(release);
    let parent_status = wait_child_status(&mut parent, Duration::from_secs(8));
    if parent_status.is_none() {
        let _ = parent.kill();
        let _ = parent.wait();
    }
    let stub_exited = wait_process_gone(stub_pid, Duration::from_secs(5));
    if !stub_exited {
        let _ = Command::new("taskkill").args(["/PID", &stub_pid.to_string(), "/F"]).output();
        let _ = wait_process_gone(stub_pid, Duration::from_secs(2));
    }
    let events = std::fs::read_to_string(dir.join("events.log")).unwrap_or_default();
    let pid_cleared = crate::audio::fxsound::stub_pid(&dir).is_none();
    let logged = events.contains(&format!("exit parent={parent_pid} gone"));
    let parent_ok = parent_status.as_ref().map(|status| status.success()).unwrap_or(false);
    json!({
        "ok": parent_ok && stub_exited && pid_cleared && logged,
        "parentMode": "--fxsound-stub-parent",
        "parentPid": parent_pid,
        "stubPid": stub_pid,
        "parentAliveWhenStubStarted": parent_alive,
        "parentExit": parent_status.as_ref().and_then(|status| status.code()),
        "stubExited": stub_exited,
        "pidCleared": pid_cleared,
        "logged": logged,
        "events": events.lines().collect::<Vec<_>>(),
    })
}

fn wait_child_status(child: &mut std::process::Child, timeout: Duration) -> Option<std::process::ExitStatus> {
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Some(status),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(25)),
            _ => return None,
        }
    }
}

fn wait_process_gone(pid: u32, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if !crate::audio::fxsound::process_alive(pid) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    !crate::audio::fxsound::process_alive(pid)
}

/// Headless proof that a real scan reaches the cache file. It calls the same
/// `scan`, `save_cache` and `load_cache` the picker does rather than the app's
/// event handler, which needs a window and event loop for no extra coverage:
/// the only handler logic it skips is `empty_scan_outcome`, which decides what
/// an empty scan keeps, and `games::retain_existing`, both already unit-tested.
/// Sandbox-only, because it writes the path a normal launch reads.
fn verify_games_cache() -> i32 {
    if crate::config::sandbox_root().is_none() {
        crate::logging::log("gamesverify: CROSSHAIRPLUS_SANDBOX_DIR is required");
        eprintln!("--verify-games-cache requires CROSSHAIRPLUS_SANDBOX_DIR");
        return 2;
    }
    let path = crate::config::games_cache_path();
    let started = std::time::Instant::now();
    let games = crate::games::scan();
    let elapsed = started.elapsed().as_millis();
    crate::games::save_cache(&path, &games);
    let readback = crate::games::load_cache(&path);
    let bytes = std::fs::metadata(&path).map(|meta| meta.len()).unwrap_or(0);
    let ok = readback.as_ref().is_ok_and(|loaded| loaded == &games);
    let report = serde_json::json!({
        "ok": ok,
        "games": games.len(),
        "readback": readback.as_ref().map(|loaded| loaded.len()).unwrap_or(0),
        "cache": path.display().to_string(),
        "cacheBytes": bytes,
        "elapsedMs": elapsed,
    });
    let compact = report.to_string();
    let _ = std::fs::write(
        crate::config::data_dir().join("games-cache-result.json"),
        serde_json::to_string_pretty(&report).unwrap_or_default(),
    );
    crate::logging::log(&format!("gamesverify: {compact}"));
    if ok { 0 } else { 1 }
}
