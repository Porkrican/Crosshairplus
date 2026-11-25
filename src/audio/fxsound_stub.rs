//! Stub FxSound CLI for the sandboxed `--verify-unstick` harness.
//!
//! The parent process redirects FxSound's data tree and re-enters this
//! executable with `--fxsound-stub`. The child then behaves like the pieces of
//! FxSound that Unstick drives: it keeps a `status.json`, persists presets,
//! applies the CLI options the app builds, and stays alive as the "running"
//! instance until the harness stops it by PID. `--fxsound-stub-parent` is the
//! harness's internal parent mode for proving the orphan-exit path.

use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use super::fxsound;

const VERSION: &str = "1.2.16.0";

/// Entry point for a stub child. Returns the process exit code.
pub fn run(args: &[String]) -> i32 {
    let Some(dir) = fxsound::stub_dir() else {
        let _ = writeln!(std::io::stderr(), "--fxsound-stub requires CROSSHAIRPLUS_FXSOUND_STUB_DIR");
        return 2;
    };
    if let Err(err) = fs::create_dir_all(dir.join("Presets")) {
        let _ = writeln!(std::io::stderr(), "could not create stub tree: {err}");
        return 1;
    }
    let clean: Vec<String> = args.iter().filter(|arg| arg.as_str() != "--fxsound-stub").cloned().collect();
    if fxsound::stub_pid_alive(&dir) {
        return command(&dir, &clean);
    }
    // Only a bare startup (optionally with the first-instance power flag)
    // becomes the long-running app; every other call is a forwarded command.
    if clean.iter().all(|arg| arg == "--run_minimized" || arg.starts_with("--power=")) {
        return start(&dir, &clean);
    }
    command(&dir, &clean)
}

/// Internal parent mode for the orphan-exit check. It starts the stub as a
/// real child, waits until that child has stamped its PID, then stays alive
/// until the harness closes stdin. The close is the release signal; a bounded
/// wait keeps an abandoned harness from pinning this parent forever.
pub fn run_parent() -> i32 {
    let Some(dir) = fxsound::stub_dir() else {
        let _ = writeln!(
            std::io::stderr(),
            "--fxsound-stub-parent requires CROSSHAIRPLUS_FXSOUND_STUB_DIR"
        );
        return 2;
    };
    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(err) => {
            let _ = writeln!(std::io::stderr(), "could not locate the stub executable: {err}");
            return 1;
        }
    };
    std::env::set_var(fxsound::STUB_PARENT_ENV, std::process::id().to_string());
    let mut stub = match Command::new(&exe)
        .arg("--fxsound-stub")
        .arg("--run_minimized")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(stub) => stub,
        Err(err) => {
            let _ = writeln!(std::io::stderr(), "could not start the stub: {err}");
            return 1;
        }
    };
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline && !fxsound::stub_pid_alive(&dir) {
        std::thread::sleep(Duration::from_millis(25));
    }
    if !fxsound::stub_pid_alive(&dir) {
        let _ = stub.kill();
        let _ = stub.wait();
        let _ = writeln!(std::io::stderr(), "the stub did not start");
        return 1;
    }
    // A reader thread blocks on the pipe, while the parent itself waits at most
    // five seconds. EOF (the harness dropping its end) or any line releases it;
    // a harness that hangs without closing cannot leave this parent running.
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut release = String::new();
        let _ = std::io::stdin().read_line(&mut release);
        let _ = release_tx.send(());
    });
    if release_rx.recv_timeout(Duration::from_secs(5)).is_err() {
        let _ = writeln!(std::io::stderr(), "the harness did not release the parent");
        return 1;
    }
    0
}

fn start(dir: &Path, args: &[String]) -> i32 {
    let mut status = read_status(dir).unwrap_or_else(default_status);
    // The first-instance options are a subset of the running-instance ones, so
    // the same applier seeds the file before this process starts idling.
    if let Err(err) = apply_args(dir, &mut status, args) {
        let _ = writeln!(std::io::stderr(), "{err}");
        return 1;
    }
    if let Err(err) = write_status(dir, &status) {
        let _ = writeln!(std::io::stderr(), "{err}");
        return 1;
    }
    if let Err(err) = fs::write(dir.join("stub.pid"), std::process::id().to_string()) {
        let _ = writeln!(std::io::stderr(), "could not write stub pid: {err}");
        return 1;
    }
    fxsound::append_stub_event(dir, &format!("start pid={}", std::process::id()));
    // The harness normally stops this process by PID. If it exits or crashes
    // first, the orphaned stub must not keep pretending FxSound is running.
    let parent = std::env::var(fxsound::STUB_PARENT_ENV)
        .ok()
        .and_then(|value| value.trim().parse::<u32>().ok());
    let mut ticks = 0u8;
    loop {
        std::thread::sleep(Duration::from_millis(100));
        let Some(parent) = parent else { continue };
        ticks += 1;
        if ticks < 5 {
            continue;
        }
        ticks = 0;
        if !fxsound::process_alive(parent) {
            fxsound::append_stub_event(dir, &format!("exit parent={parent} gone"));
            let _ = fs::remove_file(dir.join("stub.pid"));
            return 0;
        }
    }
}

fn command(dir: &Path, args: &[String]) -> i32 {
    let mut status = read_status(dir).unwrap_or_else(default_status);
    if let Err(err) = apply_args(dir, &mut status, args) {
        let _ = writeln!(std::io::stderr(), "{err}");
        return 1;
    }
    if let Err(err) = write_status(dir, &status) {
        let _ = writeln!(std::io::stderr(), "{err}");
        return 1;
    }
    fxsound::append_stub_event(dir, &format!("command {}", args.join(" ")));
    0
}

/// The option set `fxsound::build_call` emits, plus power/preset/output/status.
fn apply_args(dir: &Path, status: &mut Value, args: &[String]) -> Result<(), String> {
    for arg in args {
        if arg == "--run_minimized" || arg == "--status" {
            continue;
        }
        if let Some(value) = arg.strip_prefix("--power=") {
            status["power"] = json!(value == "1");
        } else if let Some(value) = arg.strip_prefix("--preset=") {
            let name = value.trim();
            if !dir.join("Presets").join(format!("{name}.fac")).is_file() {
                return Err(format!("unknown FxSound preset '{name}'"));
            }
            status["selected_preset"] = json!(name);
        } else if let Some(value) = arg.strip_prefix("--output=") {
            status["selected_output"] = json!(value);
        } else if let Some(value) = arg.strip_prefix("--num_bands=") {
            resize_bands(status, value.parse::<usize>().unwrap_or(10));
        } else if let Some(value) = arg.strip_prefix("--filter_q=") {
            status["equalizer"]["filter_q"] = json!(half_step(value));
        } else if let Some(value) = arg.strip_prefix("--master_gain=") {
            status["equalizer"]["master_gain"] = json!(number(value).round());
        } else if let Some(value) = arg.strip_prefix("--volume_leveling=") {
            status["equalizer"]["volume_leveling"] = json!(half_step(value));
        } else if let Some(value) = arg.strip_prefix("--balance=") {
            status["equalizer"]["balance"] = json!(number(value).round());
        } else if let Some(value) = arg.strip_prefix("--set_band_gain=") {
            apply_band_gains(status, value);
        } else if let Some(value) = arg.strip_prefix("--set_effect=") {
            apply_effects(status, value);
        }
    }
    refresh_presets(dir, status);
    ensure_shape(status);
    Ok(())
}

fn number(text: &str) -> f64 {
    text.parse::<f64>().unwrap_or(0.0)
}

/// FxSound rounds filter Q and volume leveling to the nearest 0.5, matching
/// the Settings dialog sliders.
fn half_step(text: &str) -> f64 {
    (number(text) * 2.0).round() / 2.0
}

fn band_frequencies(count: usize) -> Vec<f32> {
    match count {
        10 => fxsound::FXSOUND_10_BANDS.to_vec(),
        20 => (0..20)
            .map(|index| 20.0 * (20_000.0f32 / 20.0).powf(index as f32 / 19.0))
            .collect(),
        _ => {
            if count <= 1 {
                return vec![20.0];
            }
            (0..count)
                .map(|index| 20.0 * (20_000.0f32 / 20.0).powf(index as f32 / (count - 1) as f32))
                .collect()
        }
    }
}

fn resize_bands(status: &mut Value, count: usize) {
    // Keep the stub aligned with the real CLI's count rather than assuming
    // the only layouts are the built-in 10-band map and the 20-band AutoEq
    // layout. Real releases can report other counts after a preset change.
    let count = count.clamp(1, 64);
    let mut gains = band_gains(status);
    gains.resize(count, 0.0);
    let frequencies = band_frequencies(count);
    status["equalizer"]["num_bands"] = json!(count);
    status["equalizer"]["bands"] = Value::Array(
        gains
            .iter()
            .enumerate()
            .map(|(index, gain)| {
                json!({
                    "index": index,
                    "frequency": frequencies[index],
                    "gain": gain,
                })
            })
            .collect(),
    );
}

fn band_gains(status: &Value) -> Vec<f64> {
    let count = status
        .get("equalizer")
        .and_then(|equalizer| equalizer.get("num_bands"))
        .and_then(Value::as_u64)
        .map(|count| count as usize)
        .unwrap_or(10)
        .max(1);
    let mut gains = vec![0.0; count];
    if let Some(bands) = status.get("equalizer").and_then(|equalizer| equalizer.get("bands")).and_then(Value::as_array) {
        for band in bands {
            let index = band.get("index").and_then(Value::as_u64).unwrap_or(usize::MAX as u64);
            let gain = band.get("gain").and_then(Value::as_f64).unwrap_or(0.0);
            if let Some(slot) = gains.get_mut(index as usize) {
                *slot = gain;
            }
        }
    }
    gains
}

fn apply_band_gains(status: &mut Value, text: &str) {
    let mut gains = band_gains(status);
    for pair in text.split(',') {
        let mut parts = pair.split(':');
        let Some(index) = parts.next().and_then(|value| value.parse::<usize>().ok()) else { continue };
        let Some(gain) = parts.next().and_then(|value| value.parse::<f64>().ok()) else { continue };
        if let Some(slot) = gains.get_mut(index) {
            *slot = gain;
        }
    }
    let frequencies = band_frequencies(gains.len());
    status["equalizer"]["num_bands"] = json!(gains.len());
    status["equalizer"]["bands"] = Value::Array(
        gains
            .iter()
            .enumerate()
            .map(|(index, gain)| {
                json!({
                    "index": index,
                    "frequency": frequencies.get(index).copied().unwrap_or(0.0),
                    "gain": gain,
                })
            })
            .collect(),
    );
}

fn apply_effects(status: &mut Value, text: &str) {
    for pair in text.split(',') {
        let mut parts = pair.split(':');
        let Some(key) = parts.next() else { continue };
        let Some(value) = parts.next().and_then(|value| value.parse::<f64>().ok()) else { continue };
        if matches!(key, "bass" | "clarity" | "ambience" | "surround" | "dynamicboost") {
            status["effects"][key] = json!(value);
        }
    }
}

fn refresh_presets(dir: &Path, status: &mut Value) {
    let mut names: Vec<String> = fs::read_dir(dir.join("Presets"))
        .map(|entries| {
            entries
                .flatten()
                .filter_map(|entry| entry.path().file_stem().and_then(|stem| stem.to_str()).map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    let user_defined: Vec<Value> = names
        .iter()
        .filter(|name| name.as_str() != "Default")
        .map(|name| json!({ "name": name, "modified": false }))
        .collect();
    status["presets"] = json!({
        "built_in": [{ "name": "Default", "modified": false }],
        "user_defined": user_defined,
    });
}

fn default_equalizer() -> Value {
    json!({
        "num_bands": 10,
        "master_gain": 0.0,
        "volume_leveling": 0.0,
        "filter_q": 1.0,
        "balance": 0.0,
        "bands": (0..10)
            .map(|index| json!({
                "index": index,
                "frequency": fxsound::FXSOUND_10_BANDS[index],
                "gain": 0.0,
            }))
            .collect::<Vec<_>>(),
    })
}

fn default_status() -> Value {
    json!({
        "version": VERSION,
        "power": false,
        "selected_preset": "Default",
        "selected_output": "Stub Speakers",
        "equalizer": default_equalizer(),
        "effects": { "bass": 0.0, "clarity": 0.0, "ambience": 0.0, "surround": 0.0, "dynamicboost": 0.0 },
        "presets": { "built_in": [{ "name": "Default", "modified": false }], "user_defined": [] },
    })
}

fn ensure_shape(status: &mut Value) {
    let defaults = default_status();
    for (key, value) in defaults.as_object().expect("default status is an object") {
        if status.get(key).is_none() {
            status[key] = value.clone();
        }
    }
    let bands_ok = status["equalizer"]["bands"]
        .as_array()
        .map(|bands| !bands.is_empty())
        .unwrap_or(false);
    if bands_ok {
        status["equalizer"]["num_bands"] = json!(status["equalizer"]["bands"].as_array().map(Vec::len).unwrap_or(10));
    }
    if !bands_ok {
        status["equalizer"] = default_equalizer();
    }
    for key in ["bass", "clarity", "ambience", "surround", "dynamicboost"] {
        if status["effects"].get(key).is_none() {
            status["effects"][key] = json!(0.0);
        }
    }
    status["version"] = json!(VERSION);
}

fn read_status(dir: &Path) -> Option<Value> {
    let text = fs::read_to_string(dir.join("status.json")).ok()?;
    serde_json::from_str(&text).ok()
}

fn write_status(dir: &Path, status: &Value) -> Result<(), String> {
    let text = serde_json::to_string_pretty(status).map_err(|err| err.to_string())?;
    let tmp = dir.join("status.json.tmp");
    fs::write(&tmp, text).map_err(|err| format!("could not write {}: {err}", tmp.display()))?;
    fs::rename(&tmp, dir.join("status.json")).map_err(|err| format!("could not replace status.json: {err}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AudioConfig;
    use std::path::PathBuf;

    fn temp_dir(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "crosshairplus-fxsound-stub-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or(0)
        ))
    }

    fn fixture() -> (PathBuf, AudioConfig) {
        let dir = temp_dir("call");
        fs::create_dir_all(dir.join("Presets")).unwrap();
        fs::write(dir.join("Presets/Crosshair+.fac"), b"managed preset").unwrap();
        let mut audio = AudioConfig {
            enabled: true,
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
        (dir, audio)
    }

    #[test]
    fn stub_applies_the_call_the_app_builds() {
        let (dir, audio) = fixture();
        let call = fxsound::build_call(&audio, 10, 1.0);
        let mut status = default_status();
        apply_args(&dir, &mut status, &call.args).unwrap();
        assert_eq!(status["power"], json!(true));
        assert_eq!(status["selected_output"], json!("Stub Headphones"));
        assert_eq!(status["equalizer"]["filter_q"], json!(1.5));
        assert_eq!(status["equalizer"]["volume_leveling"], json!(1.0));
        assert_eq!(status["equalizer"]["balance"], json!(-5.0));
        let gains: Vec<f64> = status["equalizer"]["bands"]
            .as_array()
            .unwrap()
            .iter()
            .map(|band| band["gain"].as_f64().unwrap())
            .collect();
        assert_eq!(gains, vec![1.0, 2.2, 3.45, 2.75, 1.0, -2.0, -3.78, -2.45, -0.2, 1.0]);
        assert_eq!(status["effects"]["bass"], json!(4.0));
        assert_eq!(status["effects"]["clarity"], json!(3.0));
        assert_eq!(status["effects"]["ambience"], json!(1.0));
        assert_eq!(status["effects"]["surround"], json!(2.0));
        assert_eq!(status["effects"]["dynamicboost"], json!(2.5));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn stub_supports_twenty_band_parametric_call() {
        let (dir, mut audio) = fixture();
        audio.imported = Some(crate::config::ImportedProfile {
            name: "20-band fixture".into(),
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
        let call = fxsound::build_call(&audio, 10, 1.0);
        assert!(call.args.iter().any(|arg| arg == "--num_bands=20"));
        let mut status = default_status();
        apply_args(&dir, &mut status, &call.args).unwrap();
        assert_eq!(status["equalizer"]["num_bands"], json!(20));
        let bands = status["equalizer"]["bands"].as_array().unwrap();
        assert_eq!(bands.len(), 20);
        let frequencies: Vec<f64> = bands
            .iter()
            .map(|band| band["frequency"].as_f64().unwrap())
            .collect();
        assert!(frequencies[0] >= 19.9 && frequencies[0] <= 20.1, "{frequencies:?}");
        assert!(frequencies[19] >= 19_990.0 && frequencies[19] <= 20_010.0, "{frequencies:?}");
        let (_, expected_gains) = crate::audio::dsp_math::fx_bands(&audio);
        let actual_gains: Vec<f64> = bands
            .iter()
            .map(|band| band["gain"].as_f64().unwrap())
            .collect();
        for (actual, expected) in actual_gains.iter().zip(expected_gains.iter()) {
            assert!((actual - *expected as f64).abs() < 0.02, "{actual} vs {expected}");
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn stub_accepts_any_band_count_the_cli_reports() {
        let (dir, _) = fixture();
        let mut status = default_status();
        apply_args(&dir, &mut status, &["--num_bands=14".into()]).unwrap();
        assert_eq!(status["equalizer"]["num_bands"], json!(14));
        let bands = status["equalizer"]["bands"].as_array().unwrap();
        assert_eq!(bands.len(), 14);
        let first = bands[0]["frequency"].as_f64().unwrap();
        let last = bands[13]["frequency"].as_f64().unwrap();
        assert!(first >= 19.9 && first <= 20.1, "{first}");
        assert!(last >= 19_990.0 && last <= 20_010.0, "{last}");

        apply_args(&dir, &mut status, &["--set_band_gain=13:9.0".into()]).unwrap();
        assert_eq!(status["equalizer"]["bands"][13]["gain"], json!(9.0));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn stub_rounds_values_like_the_fxsound_cli() {
        let (dir, _) = fixture();
        let mut status = default_status();
        apply_args(
            &dir,
            &mut status,
            &[
                "--master_gain=-3.30".into(),
                "--filter_q=1.60".into(),
                "--volume_leveling=0.30".into(),
                "--balance=-5.40".into(),
            ],
        )
        .unwrap();
        assert_eq!(status["equalizer"]["master_gain"], json!(-3.0));
        assert_eq!(status["equalizer"]["filter_q"], json!(1.5));
        assert_eq!(status["equalizer"]["volume_leveling"], json!(0.5));
        assert_eq!(status["equalizer"]["balance"], json!(-5.0));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn stub_rejects_a_preset_it_does_not_have() {
        let (dir, _) = fixture();
        let mut status = default_status();
        let error = apply_args(
            &dir,
            &mut status,
            &["--preset=NotThere".into(), "--run_minimized".into()],
        )
        .expect_err("an unknown preset must fail like the real CLI");
        assert!(error.contains("unknown FxSound preset 'NotThere'"), "{error}");
        let _ = fs::remove_dir_all(&dir);
    }
}
