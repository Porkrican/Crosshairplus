use crate::config::{ImportedBand, ImportedProfile};

/// Limits shared with backup restore, so a profile arriving from an archive is
/// held to exactly the same rules as one parsed from a file.
pub const PREAMP_RANGE: (f32, f32) = (-30.0, 12.0);
pub const BAND_GAIN_RANGE: (f32, f32) = (-30.0, 24.0);
pub const BAND_FREQ_RANGE: (f32, f32) = (10.0, 24000.0);
pub const MAX_IMPORT_BANDS: usize = 32;
/// Display-only fields; the curve never reads them. Capped here so a restored
/// profile obeys the same size limits as one parsed from a file.
pub const MAX_IMPORT_NAME: usize = 64;
pub const MAX_IMPORT_RAW: usize = 20_000;

/// Parse Equalizer APO, AutoEq and GraphicEQ text. Malformed numeric input is
/// rejected with a clear message rather than silently producing a partial curve.
pub fn parse(text: &str, name: &str) -> Result<ImportedProfile, String> {
    let mut bands: Vec<ImportedBand> = Vec::new();
    let mut preamp = 0.0f32;
    let mut parametric = false;
    let mut numeric_error = false;

    for raw_line in text.lines() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with("//") {
            continue;
        }
        if let Some(rest) = line.strip_prefix("Preamp:") {
            let value = rest.trim().split_whitespace().next().unwrap_or("");
            match value.parse::<f32>() {
                Ok(v) if v.is_finite() => preamp = v.clamp(PREAMP_RANGE.0, PREAMP_RANGE.1),
                _ => numeric_error = true,
            }
            continue;
        }
        if let Some(rest) = find_graphic_eq(line) {
            for pair in rest.split(|c| c == ';' || c == ',') {
                let mut bits = pair.split_whitespace();
                let Some(freq) = bits.next() else { continue };
                let Some(gain) = bits.next() else { continue };
                match (freq.parse::<f32>(), gain.parse::<f32>()) {
                    (Ok(f), Ok(g))
                        if f.is_finite()
                            && g.is_finite()
                            && (BAND_FREQ_RANGE.0..=BAND_FREQ_RANGE.1).contains(&f) =>
                    {
                        bands.push(ImportedBand { freq: f, gain: g.clamp(BAND_GAIN_RANGE.0, BAND_GAIN_RANGE.1) });
                    }
                    _ => numeric_error = true,
                }
            }
            continue;
        }
        if line.to_ascii_uppercase().starts_with("FILTER ") {
            let lower = line.to_ascii_lowercase();
            if lower.contains(" pk ") || lower.contains("on pk") {
                parametric = true;
            }
            let freq = extract_number(&lower, "fc");
            let gain = extract_number(&lower, "gain");
            if let (Some(freq), Some(gain)) = (freq, gain) {
                if freq.is_finite()
                    && gain.is_finite()
                    && (BAND_FREQ_RANGE.0..=BAND_FREQ_RANGE.1).contains(&freq)
                {
                    bands.push(ImportedBand { freq, gain: gain.clamp(BAND_GAIN_RANGE.0, BAND_GAIN_RANGE.1) });
                } else {
                    numeric_error = true;
                }
            } else if lower.contains("pk") {
                numeric_error = true;
            }
        }
    }

    bands.retain(|b| b.freq.is_finite() && b.gain.is_finite());
    bands.sort_by(|a, b| a.freq.partial_cmp(&b.freq).unwrap_or(std::cmp::Ordering::Equal));
    if bands.is_empty() {
        return Err(if numeric_error {
            "The profile contains malformed band values.".into()
        } else {
            "No Equalizer APO / AutoEq bands were found in that text.".into()
        });
    }
    if numeric_error {
        return Err("The profile contains malformed band or preamp values.".into());
    }
    if bands.len() > MAX_IMPORT_BANDS {
        bands.truncate(MAX_IMPORT_BANDS);
    }
    Ok(ImportedProfile {
        name: name.trim().chars().take(MAX_IMPORT_NAME).collect::<String>(),
        raw: text.chars().take(MAX_IMPORT_RAW).collect(),
        bands,
        parametric,
        preamp,
    })
}

/// Validate an imported profile that did not come from [`parse`]: one restored
/// from a backup or read from a hand-edited config file.
///
/// Non-finite numbers cannot produce a usable curve, so a profile that holds
/// them (or that lost every band to them) is rejected as a whole; finite values
/// outside the supported ranges are clamped and reported, and surplus bands are
/// cut exactly like the importer does. A profile that never had bands is left
/// alone: it is a preamp-only offset, not corrupt input.
pub fn sanitize_imported(
    imported: Option<ImportedProfile>,
    warnings: &mut Vec<String>,
) -> Option<ImportedProfile> {
    let mut profile = imported?;
    // Display-only fields are capped exactly as the importer caps them, so stored
    // data cannot carry an oversized name or raw text into the config. The curve
    // never reads either field.
    profile.name = profile.name.trim().chars().take(MAX_IMPORT_NAME).collect();
    profile.raw = profile.raw.chars().take(MAX_IMPORT_RAW).collect();
    let label = if profile.name.is_empty() { "Imported profile" } else { profile.name.as_str() };
    if !profile.preamp.is_finite() {
        warnings.push(format!(
            "The sound profile '{label}' was not applied because its preamp is not a valid number."
        ));
        return None;
    }
    let before = profile.bands.len();
    profile.bands.retain(|band| band.freq.is_finite() && band.gain.is_finite());
    let dropped = before - profile.bands.len();
    if before > 0 && profile.bands.is_empty() {
        warnings.push(format!(
            "The sound profile '{label}' was not applied because none of its bands are valid numbers."
        ));
        return None;
    }
    if dropped > 0 {
        warnings.push(format!("Dropped {dropped} band(s) with invalid numbers from the sound profile '{label}'."));
    }
    profile
        .bands
        .sort_by(|a, b| a.freq.partial_cmp(&b.freq).unwrap_or(std::cmp::Ordering::Equal));
    let mut clamped = false;
    profile.preamp = clamp_reported(profile.preamp, PREAMP_RANGE, &mut clamped);
    for band in &mut profile.bands {
        band.freq = clamp_reported(band.freq, BAND_FREQ_RANGE, &mut clamped);
        band.gain = clamp_reported(band.gain, BAND_GAIN_RANGE, &mut clamped);
    }
    if clamped {
        warnings.push(format!(
            "Out-of-range values in the sound profile '{label}' were clamped to {} to {} dB preamp, {} to {} dB band gain and {} to {} Hz.",
            PREAMP_RANGE.0, PREAMP_RANGE.1, BAND_GAIN_RANGE.0, BAND_GAIN_RANGE.1, BAND_FREQ_RANGE.0, BAND_FREQ_RANGE.1
        ));
    }
    if profile.bands.len() > MAX_IMPORT_BANDS {
        profile.bands.truncate(MAX_IMPORT_BANDS);
        warnings.push(format!(
            "Only the first {MAX_IMPORT_BANDS} bands of the sound profile '{label}' were kept."
        ));
    }
    Some(profile)
}

fn clamp_reported(value: f32, range: (f32, f32), clamped: &mut bool) -> f32 {
    let limited = value.clamp(range.0, range.1);
    *clamped |= limited != value;
    limited
}

fn find_graphic_eq(line: &str) -> Option<&str> {
    let lower = line.to_ascii_lowercase();
    let idx = lower.find("graphiceq:")?;
    Some(&line[idx + "graphiceq:".len()..])
}

fn extract_number(line: &str, key: &str) -> Option<f32> {
    let idx = line.find(key)?;
    let rest = line[idx + key.len()..].trim_start();
    let rest = rest.strip_prefix(':').unwrap_or(rest).trim_start();
    let rest = rest.strip_prefix('=').unwrap_or(rest).trim_start();
    let token = rest.split_whitespace().next()?;
    token.parse::<f32>().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn graphic_eq_and_auto_eq() {
        let eq = "GraphicEQ: 31 3.5; 62 1.0; 125 -2; 250 0; 500 1; 1000 2; 2000 -1; 4000 0; 8000 2; 16000 4";
        let p = parse(eq, "Graphic").unwrap();
        assert_eq!(p.bands.len(), 10);
        assert_eq!(p.bands[0].gain, 3.5);
        let auto = "Preamp: -6.0 dB\nFilter 1: ON PK Fc 105 Hz Gain 4.5 dB Q 1.0\nFilter 2: ON PK Fc 3200 Hz Gain -3.0 dB Q 2.0";
        let p = parse(auto, "AutoEq").unwrap();
        assert!(p.parametric);
        assert_eq!(p.preamp, -6.0);
        assert_eq!(p.bands.len(), 2);
        assert!((p.bands[1].freq - 3200.0).abs() < 0.1);
    }

    #[test]
    fn malformed_rejected() {
        assert!(parse("", "empty").is_err());
        assert!(parse("GraphicEQ: 31 nope; 62 0", "bad").is_err());
        assert!(parse("Filter 1: ON PK Fc nope Gain 3 dB", "bad").is_err());
    }

    /// Hostile parametric text from the QA campaign: zero/negative/absurd
    /// frequencies, absurd gain and an empty GraphicEQ. Whatever is accepted
    /// must be finite and inside the shared ranges; the rest must be rejected.
    #[test]
    fn hostile_parametric_values_stay_finite_and_bounded() {
        for text in [
            "Filter 1: ON PK Fc 0 Hz Gain 3 dB Q 0",
            "Filter 1: ON PK Fc -100 Hz Gain 3 dB Q -5",
            "Filter 1: ON PK Fc 999999 Hz Gain 3 dB Q 0",
            "Filter 1: ON PK Fc 100 Hz Gain +999 dB Q 0",
            "GraphicEQ:",
            "Preamp: +999 dB",
        ] {
            if let Ok(profile) = parse(text, "Hostile") {
                assert!(profile.preamp.is_finite(), "{text}");
                assert!(profile.preamp >= PREAMP_RANGE.0 && profile.preamp <= PREAMP_RANGE.1, "{text}");
                assert!(profile.bands.len() <= MAX_IMPORT_BANDS, "{text}");
                for band in &profile.bands {
                    assert!(band.freq.is_finite() && band.gain.is_finite(), "{text}");
                    assert!(BAND_FREQ_RANGE.0 <= band.freq && band.freq <= BAND_FREQ_RANGE.1, "{text}");
                    assert!(BAND_GAIN_RANGE.0 <= band.gain && band.gain <= BAND_GAIN_RANGE.1, "{text}");
                }
            }
        }
        let long: String = (0..2000)
            .map(|i| format!("Filter {i}: ON PK Fc {} Hz Gain {} dB Q 1\n", 20 + (i % 2000), (i % 40) as i32 - 20))
            .collect();
        let profile = parse(&long, "Long").expect("a long valid file still imports");
        assert_eq!(profile.bands.len(), MAX_IMPORT_BANDS, "the band budget is enforced");
        assert!(profile.bands.iter().all(|band| band.freq.is_finite() && band.gain.is_finite()));
    }
}
