use serde_json::Value;

use crate::audio::dsp_math;
use crate::config::AudioConfig;

fn push_check(
    checks: &mut Vec<Value>,
    all_ok: &mut bool,
    name: &str,
    expected_value: Value,
    actual_value: Value,
    tolerance: Option<f64>,
) {
    let ok = if let Some(tolerance) = tolerance {
        match (expected_value.as_f64(), actual_value.as_f64()) {
            (Some(expected_number), Some(actual_number)) => {
                (actual_number - expected_number).abs() <= tolerance
            }
            _ => false,
        }
    } else {
        expected_value == actual_value
    };
    *all_ok &= ok;
    checks.push(serde_json::json!({
        "field": name,
        "expected": expected_value,
        "actual": actual_value,
        "ok": ok,
    }));
}

/// The number of EQ bands FxSound should report for this configuration.
/// Parametric imports use 20; everything else uses the normal 10-band map.
fn expected_band_count(audio: &AudioConfig) -> usize {
    dsp_math::fx_bands(audio).1.len()
}

/// FxSound rounds `--master_gain` to the nearest whole dB. The volume guard
/// deliberately requests a fractional value, so the comparison must expect the
/// value FxSound actually applies, not the pre-quantized request.
fn fxsound_master_gain(audio: &AudioConfig) -> f32 {
    dsp_math::master_gain(audio).round()
}

/// Compare the requested audio configuration with FxSound's raw status JSON.
///
/// While Sound is off, `apply` deliberately sends only `--power=0`, so the
/// user's bands, effects, and output are expected to differ and are not
/// failures. A missing requested output is also skipped here; the caller
/// reports that fallback separately.
pub fn compare_fx_status(expected: &AudioConfig, actual: &Value) -> Value {
    let equalizer = actual.get("equalizer");
    let effects = actual.get("effects");
    let mut checks = Vec::new();
    let mut all_ok = true;

    let power_ok = actual.get("power").and_then(Value::as_bool) == Some(expected.enabled);
    all_ok &= power_ok;
    checks.push(serde_json::json!({
        "field": "power",
        "expected": expected.enabled,
        "actual": actual.get("power"),
        "ok": power_ok,
    }));

    let expected_band_count = expected_band_count(expected);
    let (_, expected_bands) = dsp_math::fx_bands(expected);

    // Sound off is a power-only operation. Comparing the retained user preset
    // here would make every intentional Sound-off state look broken.
    if expected.enabled {
        let actual_band_count = equalizer
            .and_then(|e| e.get("num_bands"))
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let bands_ok = actual_band_count == expected_band_count as u64;
        all_ok &= bands_ok;
        checks.push(serde_json::json!({
            "field": "num_bands",
            "expected": expected_band_count,
            "actual": equalizer.and_then(|e| e.get("num_bands")),
            "ok": bands_ok,
        }));

        let actual_bands = equalizer.and_then(|e| e.get("bands")).and_then(Value::as_array);
        for (index, gain) in expected_bands.iter().enumerate() {
            let value = actual_bands
                .and_then(|bands| bands.get(index))
                .and_then(|band| band.get("gain"))
                .cloned()
                .unwrap_or(Value::Null);
            push_check(
                &mut checks,
                &mut all_ok,
                &format!("band{index}"),
                serde_json::json!(*gain as f64),
                value,
                Some(0.16),
            );
        }
            push_check(
                &mut checks,
                &mut all_ok,
            "master_gain",
            serde_json::json!(fxsound_master_gain(expected) as f64),
            equalizer
                .and_then(|e| e.get("master_gain"))
                .cloned()
                .unwrap_or(Value::Null),
            Some(0.16),
        );
        push_check(
            &mut checks,
            &mut all_ok,
            "volume_leveling",
            serde_json::json!(expected.volume_leveling as f64),
            equalizer
                .and_then(|e| e.get("volume_leveling"))
                .cloned()
                .unwrap_or(Value::Null),
            Some(0.16),
        );
        push_check(
            &mut checks,
            &mut all_ok,
            "balance",
            serde_json::json!(expected.balance as f64),
            equalizer
                .and_then(|e| e.get("balance"))
                .cloned()
                .unwrap_or(Value::Null),
            Some(0.16),
        );
        push_check(
            &mut checks,
            &mut all_ok,
            "filter_q",
            serde_json::json!(expected.filter_q as f64),
            equalizer
                .and_then(|e| e.get("filter_q"))
                .cloned()
                .unwrap_or(Value::Null),
            Some(0.16),
        );
        for (name, value) in [
            ("bass", expected.bass),
            ("clarity", expected.treble),
            ("ambience", expected.echo),
            ("surround", expected.surround),
            ("dynamicboost", expected.dynamic_boost),
        ] {
            push_check(
                &mut checks,
                &mut all_ok,
                &format!("effect_{name}"),
                serde_json::json!(value as f64),
                effects
                    .and_then(|e| e.get(name))
                    .cloned()
                    .unwrap_or(Value::Null),
                Some(0.16),
            );
        }
        if let Some(device) = expected.device.as_deref().filter(|d| !d.trim().is_empty()) {
            let actual_device = actual
                .get("selected_output")
                .and_then(Value::as_str)
                .unwrap_or("");
            let ok = actual_device.eq_ignore_ascii_case(device);
            all_ok &= ok;
            checks.push(serde_json::json!({
                "field": "selected_output",
                "expected": device,
                "actual": actual_device,
                "ok": ok,
            }));
        }
    }

    serde_json::json!({
        "ok": all_ok,
        "checks": checks,
        "expected": {
            "power": expected.enabled,
            "num_bands": expected_band_count,
            "bands": expected_bands,
            "master_gain": fxsound_master_gain(expected),
            "volume_leveling": expected.volume_leveling,
            "balance": expected.balance,
            "filter_q": expected.filter_q,
            "bass": expected.bass,
            "clarity": expected.treble,
            "ambience": expected.echo,
            "surround": expected.surround,
            "dynamicboost": expected.dynamic_boost,
        }
    })
}

/// Turn failed comparison fields into short human-readable items. Band gains
/// are grouped so a whole misplaced 20-band curve produces one notice, not 20.
pub fn mismatch_messages(expected: &AudioConfig, actual: &Value) -> Vec<String> {
    let report = compare_fx_status(expected, actual);
    let Some(checks) = report.get("checks").and_then(Value::as_array) else {
        return Vec::new();
    };
    let mut messages = Vec::new();
    let mut band_mismatch = false;
    for check in checks {
        if check.get("ok").and_then(Value::as_bool) != Some(false) {
            continue;
        }
        let field = check.get("field").and_then(Value::as_str).unwrap_or("setting");
        if field.starts_with("band") {
            band_mismatch = true;
            continue;
        }
        let label = match field {
            "power" => "sound on/off".to_string(),
            "num_bands" => {
                let expected = check.get("expected").and_then(Value::as_u64).unwrap_or(0);
                let actual = check
                    .get("actual")
                    .and_then(Value::as_u64)
                    .map(|value| value.to_string())
                    .unwrap_or_else(|| "none".into());
                format!("band count (expected {expected}, got {actual})")
            }
            "master_gain" => "master gain".into(),
            "volume_leveling" => "volume leveling".into(),
            "balance" => "balance".into(),
            "filter_q" => "filter Q".into(),
            "selected_output" => "output device".into(),
            field if field.starts_with("effect_") => {
                format!("{} effect", &field["effect_".len()..])
            }
            other => other.replace('_', " "),
        };
        if !messages.contains(&label) {
            messages.push(label);
        }
    }
    if band_mismatch {
        messages.insert(0, "EQ band gains".into());
    }
    messages
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ImportedBand, ImportedProfile};

    fn twenty_band_audio(enabled: bool) -> AudioConfig {
        let mut audio = AudioConfig {
            enabled,
            device: Some("Stub Headphones".into()),
            ..Default::default()
        };
        audio.imported = Some(ImportedProfile {
            name: "20-band fixture".into(),
            raw: String::new(),
            bands: (0..20)
                .map(|index| ImportedBand {
                    freq: 20.0 * (20000.0f32 / 20.0).powf(index as f32 / 19.0),
                    gain: (index as f32 - 10.0) * 0.4,
                })
                .collect(),
            parametric: true,
            preamp: -1.0,
        });
        audio
    }

    fn status_for(audio: &AudioConfig, power: bool) -> Value {
        let (_, gains) = dsp_math::fx_bands(audio);
        serde_json::json!({
            "power": power,
            "selected_output": "Stub Headphones",
            "equalizer": {
                "num_bands": gains.len(),
                "bands": gains.iter().enumerate().map(|(index, gain)| serde_json::json!({
                    "index": index,
                    "gain": gain,
                })).collect::<Vec<_>>(),
                "master_gain": fxsound_master_gain(audio),
                "volume_leveling": audio.volume_leveling,
                "balance": audio.balance,
                "filter_q": audio.filter_q,
            },
            "effects": {
                "bass": audio.bass,
                "clarity": audio.treble,
                "ambience": audio.echo,
                "surround": audio.surround,
                "dynamicboost": audio.dynamic_boost,
            }
        })
    }

    #[test]
    fn expected_band_count_follows_fx_bands() {
        assert_eq!(expected_band_count(&AudioConfig::default()), 10);
        assert_eq!(expected_band_count(&twenty_band_audio(true)), 20);
    }

    #[test]
    fn master_gain_expectation_uses_fxsound_whole_db_rounding() {
        let mut audio = AudioConfig {
            enabled: true,
            volume_guard: true,
            ..Default::default()
        };
        audio.bands = [3.0; 10];
        assert!((dsp_math::master_gain(&audio) + 3.3).abs() < 0.001);
        assert_eq!(fxsound_master_gain(&audio), -3.0);

        let actual = status_for(&audio, true);
        assert_eq!(actual["equalizer"]["master_gain"], serde_json::json!(-3.0));
        let report = compare_fx_status(&audio, &actual);
        assert_eq!(report.get("ok").and_then(Value::as_bool), Some(true), "{report}");

        let mut wrong = actual;
        wrong["equalizer"]["master_gain"] = serde_json::json!(-2.0);
        assert_eq!(
            compare_fx_status(&audio, &wrong).get("ok").and_then(Value::as_bool),
            Some(false)
        );
        assert!(mismatch_messages(&audio, &wrong).iter().any(|message| message == "master gain"));
    }

    #[test]
    fn comparison_rejects_a_ten_band_status_for_a_twenty_band_profile() {
        let audio = twenty_band_audio(true);
        let mut actual = status_for(&audio, true);
        actual["equalizer"]["num_bands"] = serde_json::json!(10);
        actual["equalizer"]["bands"] = actual["equalizer"]["bands"]
            .as_array()
            .unwrap()
            .iter()
            .take(10)
            .cloned()
            .collect::<Vec<_>>()
            .into();
        let report = compare_fx_status(&audio, &actual);
        assert_eq!(report.get("ok").and_then(Value::as_bool), Some(false));
        assert_eq!(report["expected"]["num_bands"], serde_json::json!(20));
        assert!(mismatch_messages(&audio, &actual)
            .iter()
            .any(|message| message.contains("band count")));
    }

    #[test]
    fn sound_on_twenty_band_fixture_matches_a_parametric_profile() {
        let actual: Value = serde_json::from_str(include_str!("../testdata/fxsound-status-20-band.json"))
            .expect("golden 20-band status fixture must parse");
        let audio = twenty_band_audio(true);
        let (_, gains) = dsp_math::fx_bands(&audio);
        assert_eq!(gains.len(), 20, "the parametric profile must request 20 bands");

        let report = compare_fx_status(&audio, &actual);
        assert_eq!(
            report.get("ok").and_then(Value::as_bool),
            Some(true),
            "a Sound-on 20-band status must match the 20-band request: {report}"
        );
        assert_eq!(report["expected"]["num_bands"], serde_json::json!(20));

        let mut ten_band_reply = actual;
        ten_band_reply["equalizer"]["num_bands"] = serde_json::json!(10);
        ten_band_reply["equalizer"]["bands"] = ten_band_reply["equalizer"]["bands"]
            .as_array()
            .unwrap()
            .iter()
            .take(10)
            .cloned()
            .collect::<Vec<_>>()
            .into();
        assert_eq!(
            compare_fx_status(&audio, &ten_band_reply).get("ok").and_then(Value::as_bool),
            Some(false)
        );
    }

    #[test]
    fn sound_off_fixture_reports_power_mismatch_without_comparing_the_user_curve() {
        let actual: Value = serde_json::from_str(include_str!("../testdata/fxsound-status-sound-off.json"))
            .expect("golden Sound-off status fixture must parse");
        let audio = AudioConfig {
            enabled: false,
            device: Some("Missing Headset".into()),
            ..Default::default()
        };

        let report = compare_fx_status(&audio, &actual);
        assert_eq!(report.get("ok").and_then(Value::as_bool), Some(false));
        assert_eq!(report["checks"].as_array().map(Vec::len), Some(1));
        assert_eq!(report["checks"][0]["field"], "power");
        assert_eq!(report["checks"][0]["expected"], serde_json::json!(false));
        assert_eq!(report["checks"][0]["actual"], serde_json::json!(true));
        assert_eq!(mismatch_messages(&audio, &actual), vec!["sound on/off".to_string()]);

        let mut powered_off = actual;
        powered_off["power"] = serde_json::json!(false);
        assert_eq!(
            compare_fx_status(&audio, &powered_off).get("ok").and_then(Value::as_bool),
            Some(true)
        );
    }

    #[test]
    fn mismatch_messages_name_ignored_fields() {
        let audio = twenty_band_audio(true);
        let mut actual = status_for(&audio, true);
        actual["equalizer"]["num_bands"] = serde_json::json!(10);
        actual["effects"]["bass"] = serde_json::json!(5.0);
        let messages = mismatch_messages(&audio, &actual);
        assert!(messages.iter().any(|message| message.contains("band count")));
        assert!(messages.iter().any(|message| message.contains("bass effect")));
    }

    #[test]
    fn missing_output_is_not_an_output_mismatch() {
        let mut audio = AudioConfig {
            enabled: true,
            device: None,
            ..Default::default()
        };
        audio.bands = [1.0; 10];
        let actual = status_for(&audio, true);
        assert_eq!(
            compare_fx_status(&audio, &actual).get("ok").and_then(Value::as_bool),
            Some(true)
        );
    }
}
