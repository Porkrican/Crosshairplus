//! Best-effort panel-type detection.
//!
//! Windows exposes no authoritative panel-technology field. We combine EDID /
//! friendly-name keywords with DXGI luminance metadata, and fall back to LED.

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Luminance {
    pub min: f32,
    pub max: f32,
}

const OLED_MARKERS: [&str; 8] =
    ["QD-OLED", "WOLED", "OLED", "QZD", "AQDM", "FO27", "AW3423", "321URX"];
const MINILED_MARKERS: [&str; 2] = ["MINILED", "MINI LED"];
const IPS_MARKERS: [&str; 3] = ["NANO IPS", "NANOIPS", "FAST IPS"];

fn has_token(upper: &str, token: &str) -> bool {
    upper.split(|ch: char| !ch.is_ascii_alphanumeric()).any(|part| part == token)
}

pub fn infer_from_name(name: &str) -> Option<&'static str> {
    let upper = name.to_ascii_uppercase();
    if OLED_MARKERS.iter().any(|marker| upper.contains(marker)) {
        return Some("oled");
    }
    if MINILED_MARKERS.iter().any(|marker| upper.contains(marker)) {
        return Some("miniled");
    }
    if IPS_MARKERS.iter().any(|marker| upper.contains(marker)) || has_token(&upper, "IPS") {
        return Some("ips");
    }
    None
}

/// Luminance values are only trusted when they are HDR-class. Many SDR LCDs
/// report `min = 0`, so a zero black floor alone must never imply OLED.
pub fn infer_from_luminance(luminance: Luminance, hdr_context: bool) -> Option<&'static str> {
    let hdr_class = luminance.max >= 400.0 || (hdr_context && luminance.max >= 250.0);
    if hdr_class && luminance.min <= 0.0005 {
        return Some("oled");
    }
    if luminance.max >= 950.0 && luminance.min > 0.0005 {
        return Some("miniled");
    }
    None
}

pub fn detect(name: &str, luminance: Option<Luminance>, hdr_context: bool) -> String {
    if let Some(panel) = infer_from_name(name) {
        return panel.into();
    }
    if let Some(luminance) = luminance {
        if let Some(panel) = infer_from_luminance(luminance, hdr_context) {
            return panel.into();
        }
    }
    "led".into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_keywords_identify_oled_and_ips() {
        assert_eq!(infer_from_name("AOC AG276QZD2"), Some("oled"));
        assert_eq!(infer_from_name("Dell AW3423DW"), Some("oled"));
        assert_eq!(infer_from_name("MSI MPG 321URX"), Some("oled"));
        assert_eq!(infer_from_name("LG UltraGear OLED 27"), Some("oled"));
        assert_eq!(infer_from_name("ASUS ROG Swift PG27AQDM"), Some("oled"));
        assert_eq!(infer_from_name("LG Nano IPS 27GL850"), Some("ips"));
        assert_eq!(infer_from_name("BenQ MiniLED EX321UX"), Some("miniled"));
        assert_eq!(infer_from_name("Philips 27M1N5500"), None, "brand must not match IPS");
        assert_eq!(infer_from_name("Generic Monitor"), None);
    }

    #[test]
    fn luminance_needs_hdr_class_peak_not_just_zero_black() {
        assert_eq!(infer_from_luminance(Luminance { min: 0.0, max: 1000.0 }, false), Some("oled"));
        assert_eq!(infer_from_luminance(Luminance { min: 0.0, max: 270.0 }, false), None);
        assert_eq!(
            infer_from_luminance(Luminance { min: 0.02, max: 1200.0 }, false),
            Some("miniled")
        );
        assert_eq!(infer_from_luminance(Luminance { min: 0.02, max: 600.0 }, false), None);
    }

    #[test]
    fn detect_falls_back_to_led() {
        assert_eq!(detect("Generic Monitor", None, false), "led");
        assert_eq!(
            detect("Generic Monitor", Some(Luminance { min: 0.0, max: 270.0 }), false),
            "led"
        );
        assert_eq!(
            detect("Generic Monitor", Some(Luminance { min: 0.0, max: 900.0 }), true),
            "oled"
        );
    }
}
