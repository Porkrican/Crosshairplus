use crate::config::DisplayValues;
use crate::monitors::MonitorInfo;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Panel {
    Led,
    Oled,
    Ips,
    Va,
    MiniLed,
    Tn,
}

impl Panel {
    pub fn from_str(value: &str) -> Self {
        match value {
            "oled" => Self::Oled,
            "ips" => Self::Ips,
            "va" => Self::Va,
            "miniled" => Self::MiniLed,
            "tn" => Self::Tn,
            _ => Self::Led,
        }
    }

    /// Panel pre-curves. OLED keeps true black at zero and lifts the first
    /// shadow steps so calibration does not crush near-black content.
    pub fn apply(self, x: f32) -> f32 {
        let x = x.clamp(0.0, 1.0);
        match self {
            Self::Oled => {
                if x <= 0.0 {
                    0.0
                } else {
                    let toe = 0.016 * (1.0 - (-9.0 * x).exp()) * (1.0 - x);
                    (x + toe).clamp(0.0, 1.0)
                }
            }
            Self::Ips => {
                let s = 0.5 + (x - 0.5) * 1.025;
                s.clamp(0.0, 1.0)
            }
            Self::Va => {
                let s = 0.5 + (x - 0.5) * 1.06;
                s.clamp(0.0, 1.0).powf(1.015)
            }
            Self::Tn => {
                let s = x.powf(1.035);
                s.clamp(0.0, 1.0)
            }
            Self::MiniLed => {
                let s = 0.5 + (x - 0.5) * 1.04;
                s.clamp(0.0, 1.0)
            }
            Self::Led => x,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Luts {
    pub r: [u8; 256],
    pub g: [u8; 256],
    pub b: [u8; 256],
}

impl Luts {
    pub fn neutral() -> Self {
        let mut r = [0u8; 256];
        let mut g = [0u8; 256];
        let mut b = [0u8; 256];
        for i in 0..256 {
            r[i] = i as u8;
            g[i] = i as u8;
            b[i] = i as u8;
        }
        Self { r, g, b }
    }

    pub fn to_ramp(&self) -> [u16; 768] {
        let mut ramp = [0u16; 768];
        for i in 0..256 {
            ramp[i] = self.r[i] as u16 * 257;
            ramp[256 + i] = self.g[i] as u16 * 257;
            ramp[512 + i] = self.b[i] as u16 * 257;
        }
        ramp
    }
}

/// Resolve the effective panel. `auto` uses the detected monitor panel when
/// available; a missing or unknown detection falls back to LED.
pub fn effective_panel(values: &DisplayValues, monitor: Option<&MonitorInfo>) -> Panel {
    if values.panel == "auto" {
        monitor.map(|m| Panel::from_str(&m.detected_panel)).unwrap_or(Panel::Led)
    } else {
        Panel::from_str(&values.panel)
    }
}

fn scale_channel(x: f32, values: &DisplayValues, channel: usize, panel: Panel) -> f32 {
    let mut v = panel.apply(x);
    v += values.brightness / 100.0 * 0.42;
    v = (v - 0.5) * (1.0 + values.contrast / 100.0 * 0.9) + 0.5;
    v = v.clamp(0.0, 1.0);
    v = v.powf(1.0 / values.gamma.clamp(0.5, 2.5));
    let warmth = values.warmth / 100.0;
    match channel {
        0 => {
            v *= values.r / 100.0;
            v += warmth * 0.075;
        }
        1 => {
            v *= values.g / 100.0;
            v += warmth * 0.008;
        }
        _ => {
            v *= values.b / 100.0;
            v -= warmth * 0.10;
        }
    }
    v.clamp(0.0, 1.0)
}

fn quantize(values: &DisplayValues, channel: usize, panel: Panel) -> [u8; 256] {
    let mut out = [0u8; 256];
    for i in 0..256 {
        out[i] = (scale_channel(i as f32 / 255.0, values, channel, panel) * 255.0)
            .round()
            .clamp(0.0, 255.0) as u8;
    }
    // Monotonic clamp: never invert. A tiny floor avoids hard reversals after
    // quantization while preserving black at zero.
    for i in 1..256 {
        if out[i] < out[i - 1] {
            out[i] = out[i - 1];
        }
    }
    out
}

pub fn build(values: &DisplayValues) -> Luts {
    build_for(values, None)
}

pub fn build_for(values: &DisplayValues, monitor: Option<&MonitorInfo>) -> Luts {
    let mut values = values.clone();
    values.normalize();
    let panel = effective_panel(&values, monitor);
    Luts {
        r: quantize(&values, 0, panel),
        g: quantize(&values, 1, panel),
        b: quantize(&values, 2, panel),
    }
}

#[cfg(test)]
pub fn is_monotonic(lut: &[u8; 256]) -> bool {
    lut.windows(2).all(|w| w[1] >= w[0])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn neutral_is_identity() {
        let luts = build(&DisplayValues::default());
        assert_eq!(luts.r[0], 0);
        assert_eq!(luts.r[128], 128);
        assert_eq!(luts.b[255], 255);
    }

    #[test]
    fn oled_black_and_toe_lift() {
        let mut v = DisplayValues::default();
        v.panel = "oled".into();
        let luts = build(&v);
        assert_eq!(luts.g[0], 0, "OLED must keep true black");
        let neutral = build(&DisplayValues::default()).g;
        assert!(luts.g[6] >= neutral[6], "toe should not crush shadows");
        assert!(luts.g[12] > neutral[12], "toe lift should be visible");
    }

    #[test]
    fn gamma_above_one_lifts_and_below_one_deepens_midtones() {
        let neutral = build(&DisplayValues::default()).g[64] as i32;
        let lifted = build(&DisplayValues { gamma: 1.18, ..Default::default() }).g[64] as i32;
        let deepened = build(&DisplayValues { gamma: 0.93, ..Default::default() }).g[64] as i32;
        assert!(lifted > neutral, "gamma 1.18 must lift shadows");
        assert!(deepened < neutral, "gamma 0.93 must deepen midtones");
    }

    #[test]
    fn auto_panel_uses_the_detected_monitor_panel() {
        let monitor = MonitorInfo { detected_panel: "oled".into(), ..Default::default() };
        let auto = DisplayValues { panel: "auto".into(), ..Default::default() };
        let luts = build_for(&auto, Some(&monitor));
        let oled = build(&DisplayValues { panel: "oled".into(), ..Default::default() });
        assert_eq!(luts.g, oled.g);
        let led = build(&auto);
        let neutral = build(&DisplayValues::default());
        assert_eq!(led.g, neutral.g);
    }

    #[test]
    fn extreme_values_stay_monotonic() {
        for panel in ["led", "oled", "ips", "va", "miniled", "tn"] {
            for &(b, c, g, w, r, gg, bb) in &[(-40.0, -30.0, 0.5, 80.0, 50.0, 50.0, 50.0), (40.0, 40.0, 2.5, 0.0, 120.0, 120.0, 120.0)] {
                let v = DisplayValues { brightness: b, contrast: c, gamma: g, warmth: w, r, g: gg, b: bb, panel: panel.into() };
                let luts = build(&v);
                assert!(is_monotonic(&luts.r));
                assert!(is_monotonic(&luts.g));
                assert!(is_monotonic(&luts.b));
                let panel = Panel::from_str(&v.panel);
                assert_eq!(luts.r[0], (scale_channel(0.0, &v, 0, panel) * 255.0).round() as u8);
            }
        }
    }
}
