use crate::config::{AudioConfig, ImportedProfile, BAND_FREQS};

/// Interpolate an imported curve at an arbitrary frequency on a log axis.
pub fn interpolate_import(profile: &ImportedProfile, freq: f32) -> f32 {
    if profile.bands.is_empty() {
        return 0.0;
    }
    let mut points: Vec<(f32, f32)> = profile.bands.iter().map(|b| (b.freq.max(1.0), b.gain)).collect();
    points.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
    let f = freq.max(1.0);
    if f <= points[0].0 {
        return points[0].1;
    }
    if f >= points[points.len() - 1].0 {
        return points[points.len() - 1].1;
    }
    for pair in points.windows(2) {
        let (f0, g0) = pair[0];
        let (f1, g1) = pair[1];
        if f >= f0 && f <= f1 {
            let t = (f.ln() - f0.ln()) / (f1.ln() - f0.ln()).max(0.0001);
            return g0 + (g1 - g0) * t;
        }
    }
    0.0
}

fn manual_gain_at(audio: &AudioConfig, freq: f32) -> f32 {
    // Interpolate the ten manual faders on a log axis. FxSound effects such as
    // Bass Boost and Clarity are deliberately not folded into this EQ curve;
    // they are separate processing blocks with their own response.
    let bands = &audio.bands;
    if freq <= BAND_FREQS[0] {
        bands[0]
    } else if freq >= BAND_FREQS[9] {
        bands[9]
    } else {
        let mut value = 0.0;
        for i in 0..9 {
            if freq >= BAND_FREQS[i] && freq <= BAND_FREQS[i + 1] {
                let t = (freq.ln() - BAND_FREQS[i].ln()) / (BAND_FREQS[i + 1].ln() - BAND_FREQS[i].ln());
                value = bands[i] + (bands[i + 1] - bands[i]) * t;
                break;
            }
        }
        value
    }
}

/// Combined 10-band display curve = imported base + manual faders.
pub fn effective_bands(audio: &AudioConfig) -> [f32; 10] {
    let mut out = [0.0f32; 10];
    for (i, f) in BAND_FREQS.iter().enumerate() {
        let base = audio.imported.as_ref().map(|p| interpolate_import(p, *f)).unwrap_or(0.0)
            + audio.imported.as_ref().map(|p| p.preamp).unwrap_or(0.0);
        out[i] = (base + manual_gain_at(audio, *f)).clamp(-18.0, 18.0);
    }
    out
}

/// The current combined response sampled at the ten manual band centres.
/// Used when the user chooses to fold an imported profile into the faders.
pub fn flatten_import(audio: &AudioConfig) -> Option<[f32; 10]> {
    audio.imported.as_ref()?;
    Some(effective_bands(audio).map(|gain| gain.clamp(-12.0, 12.0)))
}

/// Bands sent to FxSound. A parametric AutoEq import uses 20 bands.
pub fn fx_bands(audio: &AudioConfig) -> (Vec<f32>, Vec<f32>) {
    let parametric = audio.imported.as_ref().map(|p| p.parametric && p.bands.len() > 10).unwrap_or(false);
    let count = if parametric { 20 } else { 10 };
    let freqs: Vec<f32> = if parametric {
        (0..count)
            .map(|i| 20.0 * (20000.0f32 / 20.0).powf(i as f32 / (count - 1) as f32))
            .collect()
    } else {
        BAND_FREQS.to_vec()
    };
    let gains = freqs
        .iter()
        .map(|f| {
            let base = audio.imported.as_ref().map(|p| interpolate_import(p, *f)).unwrap_or(0.0)
                + audio.imported.as_ref().map(|p| p.preamp).unwrap_or(0.0);
            (base + manual_gain_at(audio, *f)).clamp(-12.0, 12.0)
        })
        .collect();
    (freqs, gains)
}

/// A temporary flat curve for Sound's Hold to Compare. FxSound stays powered
/// on (`--power=1`) so Windows never drops and re-adds the virtual endpoint;
/// only the EQ and effects are neutralised. The device and filter Q survive so
/// release is the only thing that changes.
pub fn flat_compare(audio: &AudioConfig) -> AudioConfig {
    AudioConfig {
        enabled: true,
        device: audio.device.clone(),
        bands: [0.0; 10],
        bass: 0.0,
        treble: 0.0,
        output_level: 0.0,
        echo: 0.0,
        surround: 0.0,
        dynamic_boost: 0.0,
        volume_leveling: 0.0,
        balance: 0.0,
        filter_q: audio.filter_q,
        volume_guard: audio.volume_guard,
        imported: None,
        active_preset: None,
    }
}

pub fn peak_gain(gains: &[f32]) -> f32 {
    gains.iter().cloned().fold(-60.0f32, f32::max)
}

/// Master gain: output level with optional anti-clip headroom. Guard reduces
/// by the positive peak plus 0.3 dB of safety margin, clamped to FxSound's
/// documented -20..+20 dB range. Emitting less than -20 makes FxSound silently
/// reset the value to 0 dB, which is louder than the requested attenuation.
pub fn master_gain(audio: &AudioConfig) -> f32 {
    let (_, gains) = fx_bands(audio);
    let peak = peak_gain(&gains);
    let reduction = if audio.volume_guard && peak > 0.0 { peak + 0.3 } else { 0.0 };
    (audio.output_level - reduction).clamp(-20.0, 20.0)
}

pub fn headroom_db(peak: f32, audio: &AudioConfig) -> f32 {
    if peak <= 0.00001 {
        -60.0
    } else {
        let db = 20.0 * peak.log10();
        db + audio.output_level - master_gain(audio)
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Biquad {
    pub b0: f32,
    pub b1: f32,
    pub b2: f32,
    pub a1: f32,
    pub a2: f32,
}

impl Biquad {
    pub fn peaking(gain_db: f32, freq: f32, q: f32, sample_rate: f32) -> Self {
        // A hostile or hand-edited value must never turn the coefficients into
        // NaN/Inf and poison every curve sample. Imported configs are already
        // clamped upstream; this is the last line of defence for the math.
        let gain_db = if gain_db.is_finite() { gain_db.clamp(-60.0, 60.0) } else { 0.0 };
        let freq = if freq.is_finite() { freq.clamp(10.0, 24000.0) } else { 1000.0 };
        let q = if q.is_finite() { q.max(0.1) } else { 0.7 };
        let a = 10f32.powf(gain_db / 40.0);
        let w0 = 2.0 * std::f32::consts::PI * freq / sample_rate.max(8000.0);
        let alpha = w0.sin() / (2.0 * q.max(0.1));
        let cos = w0.cos();
        let a0 = 1.0 + alpha / a;
        Self {
            b0: (1.0 + alpha * a) / a0,
            b1: (-2.0 * cos) / a0,
            b2: (1.0 - alpha * a) / a0,
            a1: (-2.0 * cos) / a0,
            a2: (1.0 - alpha / a) / a0,
        }
    }

    pub fn magnitude(&self, freq: f32, sample_rate: f32) -> f32 {
        let w = 2.0 * std::f32::consts::PI * freq / sample_rate.max(8000.0);
        let (sin1, cos1) = w.sin_cos();
        let (sin2, cos2) = (2.0 * w).sin_cos();
        let num_re = self.b0 + self.b1 * cos1 + self.b2 * cos2;
        let num_im = -(self.b1 * sin1 + self.b2 * sin2);
        let den_re = 1.0 + self.a1 * cos1 + self.a2 * cos2;
        let den_im = -(self.a1 * sin1 + self.a2 * sin2);
        let num = (num_re * num_re + num_im * num_im).sqrt();
        let den = (den_re * den_re + den_im * den_im).sqrt().max(0.000001);
        20.0 * (num / den).max(0.000001).log10()
    }
}

pub fn curve_db(bands: &[f32], sample_rate: f32, points: usize) -> Vec<(f32, f32)> {
    let mut out = Vec::with_capacity(points);
    for i in 0..points {
        let freq = 20.0 * (20000.0f32 / 20.0).powf(i as f32 / (points.saturating_sub(1).max(1)) as f32);
        let mut db = 0.0;
        for (band, gain) in BAND_FREQS.iter().zip(bands.iter()) {
            if gain.abs() > 0.001 {
                db += Biquad::peaking(*gain, *band, 1.41, sample_rate).magnitude(freq, sample_rate);
            }
        }
        out.push((freq, db));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn biquad_peak_and_guard() {
        let q = Biquad::peaking(6.0, 1000.0, 1.41, 48000.0);
        let at_center = q.magnitude(1000.0, 48000.0);
        assert!((at_center - 6.0).abs() < 0.5, "center gain {at_center}");
        let far = q.magnitude(100.0, 48000.0);
        assert!(far.abs() < 2.0, "far gain {far}");

        let mut audio = AudioConfig { volume_guard: true, output_level: 0.0, ..Default::default() };
        audio.bands[3] = 6.0;
        assert!((master_gain(&audio) - (-6.3)).abs() < 0.01);
        audio.volume_guard = false;
        assert!((master_gain(&audio) - 0.0).abs() < 0.01);
    }

    /// QA campaign: zero/negative Q, 24 dB and absurd gains must never produce a
    /// NaN/Inf biquad coefficient or curve sample.
    #[test]
    fn hostile_q_and_gain_keep_the_biquad_finite() {
        for &(gain, q) in &[(24.0, 0.0), (24.0, -5.0), (-30.0, 0.0), (1.0e30, 0.0), (1.0e30, -1.0)] {
            let biquad = Biquad::peaking(gain, 1000.0, q, 48000.0);
            for coefficient in [biquad.b0, biquad.b1, biquad.b2, biquad.a1, biquad.a2] {
                assert!(coefficient.is_finite(), "gain {gain} q {q} produced {coefficient}");
            }
            let magnitude = biquad.magnitude(1000.0, 48000.0);
            assert!(magnitude.is_finite(), "gain {gain} q {q} produced {magnitude}");
        }
        for freq in [0.0, -100.0, 999_999.0] {
            assert!(Biquad::peaking(6.0, freq, 1.41, 48000.0).magnitude(freq, 48000.0).is_finite(), "freq {freq}");
        }
        let curve = curve_db(&[24.0; 10], 48000.0, 128);
        assert!(curve.iter().all(|(freq, db)| freq.is_finite() && db.is_finite()));
    }

    #[test]
    fn master_gain_never_leaves_fxsound_range() {
        let mut audio = AudioConfig { volume_guard: true, output_level: -12.0, ..Default::default() };
        audio.bands = [12.0; 10];
        assert!((master_gain(&audio) + 20.0).abs() < 0.001, "guard must clamp at -20 dB, not reset to 0");
        audio.volume_guard = false;
        assert!((master_gain(&audio) + 12.0).abs() < 0.001);
        audio.output_level = 40.0;
        assert!((master_gain(&audio) - 20.0).abs() < 0.001);
    }

    #[test]
    fn flattening_an_import_preserves_the_combined_curve_at_fader_centres() {
        let mut audio = AudioConfig::default();
        audio.imported = Some(ImportedProfile {
            name: "Flatten".into(),
            raw: String::new(),
            bands: crate::config::BAND_FREQS.iter().map(|freq| crate::config::ImportedBand { freq: *freq, gain: 2.0 }).collect(),
            parametric: false,
            preamp: -1.0,
        });
        audio.bands[0] = 3.0;
        let expected = effective_bands(&audio);
        let flattened = flatten_import(&audio).unwrap();
        assert_eq!(flattened, expected);
        assert!((flattened[0] - 4.0).abs() < 0.001, "{flattened:?}");
        if let Some(profile) = audio.imported.as_mut() {
            profile.preamp = 20.0;
        }
        assert!(flatten_import(&audio).unwrap().iter().all(|gain| *gain <= 12.0));
        audio.imported = None;
        assert!(flatten_import(&audio).is_none());
    }

    #[test]
    fn imported_curve_is_finite_and_continuous_at_the_range_edges() {
        let profile = ImportedProfile {
            name: "Edges".into(),
            raw: String::new(),
            bands: vec![
                crate::config::ImportedBand { freq: 31.0, gain: -4.0 },
                crate::config::ImportedBand { freq: 125.0, gain: 3.0 },
                crate::config::ImportedBand { freq: 1000.0, gain: -1.5 },
                crate::config::ImportedBand { freq: 16_000.0, gain: 5.0 },
            ],
            parametric: false,
            preamp: 0.0,
        };
        for edge in [20.0f32, 31.0, 16_000.0, 20_000.0] {
            let value = interpolate_import(&profile, edge);
            assert!(value.is_finite(), "{edge} Hz -> {value}");
            for step in [0.999f32, 1.001] {
                let near = interpolate_import(&profile, edge * step);
                assert!(near.is_finite(), "{edge} Hz neighbour -> {near}");
                assert!((near - value).abs() < 0.25, "{edge} Hz jumps {value} -> {near}");
            }
        }
        // Below the first band and above the last the curve holds the endpoint
        // gain instead of decaying to zero.
        assert_eq!(interpolate_import(&profile, 20.0), -4.0);
        assert_eq!(interpolate_import(&profile, 20_000.0), 5.0);
        // The ten-band curve built from it stays finite across the whole range.
        let audio = AudioConfig { imported: Some(profile), ..Default::default() };
        assert!(effective_bands(&audio).iter().all(|gain| gain.is_finite()));
        assert!(fx_bands(&audio).1.iter().all(|gain| gain.is_finite()));
    }

    #[test]
    fn import_layers_additively() {
        let mut audio = AudioConfig::default();
        audio.imported = Some(ImportedProfile {
            name: "Test".into(),
            raw: String::new(),
            bands: BAND_FREQS.iter().enumerate().map(|(i, f)| crate::config::ImportedBand { freq: *f, gain: i as f32 }).collect(),
            parametric: false,
            preamp: -1.0,
        });
        audio.bands[0] = 2.0;
        let e = effective_bands(&audio);
        assert!((e[0] - 1.0).abs() < 0.001, "{e:?}");
        audio.imported = None;
        assert!((effective_bands(&audio)[0] - 2.0).abs() < 0.001);
    }
}
