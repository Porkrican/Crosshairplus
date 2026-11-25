use std::ffi::c_void;

use windows::core::PCWSTR;
use windows::Win32::Foundation::WIN32_ERROR;
use windows::Win32::System::Registry::{RegGetValueW, HKEY_LOCAL_MACHINE, RRF_RT_REG_BINARY};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EdidIdentity {
    pub brand: String,
    pub model: String,
    pub product_code: u16,
    pub serial: Option<u32>,
}

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(Some(0)).collect()
}

/// Parse the subset of EDID 1.x needed for a stable brand/model label.
pub fn parse(bytes: &[u8]) -> Option<EdidIdentity> {
    if bytes.len() < 128 || bytes[..8] != [0x00, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x00] {
        return None;
    }
    let manufacturer = u16::from_be_bytes([bytes[8], bytes[9]]);
    let letters = [
        ((manufacturer >> 10) & 0x1F) as u8,
        ((manufacturer >> 5) & 0x1F) as u8,
        (manufacturer & 0x1F) as u8,
    ];
    if letters.iter().any(|value| *value == 0 || *value > 26) {
        return None;
    }
    let brand_code: String = letters.iter().map(|value| (b'A' + value - 1) as char).collect();
    let product_code = u16::from_le_bytes([bytes[10], bytes[11]]);
    let serial = u32::from_le_bytes([bytes[12], bytes[13], bytes[14], bytes[15]]);
    let mut model = String::new();
    for offset in [54usize, 72, 90, 108] {
        if bytes.get(offset) == Some(&0)
            && bytes.get(offset + 1) == Some(&0)
            && bytes.get(offset + 3) == Some(&0xFC)
        {
            let raw = &bytes[offset + 5..offset + 18];
            let text: String = raw
                .iter()
                .take_while(|byte| **byte != 0 && **byte != 0x0A)
                .map(|byte| *byte as char)
                .collect();
            model = text.trim().to_string();
            if !model.is_empty() {
                break;
            }
        }
    }
    if model.is_empty() {
        model = format!("{brand_code} {product_code:04X}");
    }
    Some(EdidIdentity { brand: brand_code, model, product_code, serial: (serial != 0).then_some(serial) })
}

/// Read the EDID blob for the active monitor instance from the PnP registry.
pub fn read_for_device_path(device_path: &str) -> Option<EdidIdentity> {
    let parts: Vec<&str> = device_path
        .split(|c| c == '#' || c == '\\')
        .filter(|part| !part.is_empty())
        .collect();
    let index = parts.iter().position(|part| part.eq_ignore_ascii_case("DISPLAY"))?;
    let pnp = *parts.get(index + 1)?;
    let instance = *parts.get(index + 2)?;
    let subkey = format!("SYSTEM\\CurrentControlSet\\Enum\\DISPLAY\\{pnp}\\{instance}\\Device Parameters");
    let subkey = wide(&subkey);
    let value = wide("EDID");
    let mut buffer = [0u8; 1024];
    let mut size = buffer.len() as u32;
    let status: WIN32_ERROR = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            PCWSTR(subkey.as_ptr()),
            PCWSTR(value.as_ptr()),
            RRF_RT_REG_BINARY,
            None,
            Some(buffer.as_mut_ptr() as *mut c_void),
            Some(&mut size),
        )
    };
    if status.0 != 0 || size < 128 {
        return None;
    }
    let identity = parse(&buffer[..size as usize])?;
    crate::logging::log(&format!("edid: {} {} pnp={} product={:04X}", identity.brand, identity.model, pnp, identity.product_code));
    Some(identity)
}

/// Brand/model fallback for monitors whose EDID is unavailable.
pub fn identify(device_path: &str, friendly: &str) -> (String, String) {
    if let Some(identity) = read_for_device_path(device_path) {
        return (identity.brand, identity.model);
    }
    let pnp = device_path
        .split(|c| c == '#' || c == '\\')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .windows(2)
        .find(|pair| pair[0].eq_ignore_ascii_case("DISPLAY"))
        .map(|pair| pair[1])
        .unwrap_or("");
    let brand = pnp_brand(pnp).to_string();
    let model = if friendly.trim().is_empty() {
        pnp.to_string()
    } else {
        friendly.trim().to_string()
    };
    (brand, model)
}

fn pnp_brand(pnp: &str) -> &str {
    let code = pnp.get(..3).unwrap_or(pnp);
    match code.to_ascii_uppercase().as_str() {
        "AOC" => "AOC",
        "ACR" => "Acer",
        "ALI" => "Alienware",
        "APP" => "Apple",
        "ASU" | "AUS" => "ASUS",
        "BNQ" => "BenQ",
        "CMN" => "Chi Mei",
        "DEL" => "Dell",
        "EIZ" => "EIZO",
        "GSM" | "LGD" => "LG",
        "HWP" => "HP",
        "LEN" => "Lenovo",
        "MSI" => "MSI",
        "PHL" => "Philips",
        "SAM" => "Samsung",
        "SNY" => "Sony",
        "VSC" => "ViewSonic",
        _ => pnp,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_edid() -> [u8; 128] {
        let mut bytes = [0u8; 128];
        bytes[..8].copy_from_slice(&[0x00, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x00]);
        bytes[8] = 0x05;
        bytes[9] = 0xE3; // AOC
        bytes[10] = 0x60;
        bytes[11] = 0x27; // product 0x2760
        bytes[12..16].copy_from_slice(&0x12345678u32.to_le_bytes());
        bytes[54..59].copy_from_slice(&[0, 0, 0, 0xFC, 0]);
        let name = b"AG276QZD2\n";
        bytes[59..59 + name.len()].copy_from_slice(name);
        bytes
    }

    #[test]
    fn parses_brand_model_and_serial() {
        let parsed = parse(&sample_edid()).unwrap();
        assert_eq!(parsed.brand, "AOC");
        assert_eq!(parsed.model, "AG276QZD2");
        assert_eq!(parsed.product_code, 0x2760);
        assert_eq!(parsed.serial, Some(0x12345678));
    }

    #[test]
    fn rejects_non_edid_and_derives_fallback_identity() {
        assert!(parse(&[0u8; 128]).is_none());
        let (brand, model) = identify("\\\\?\\DISPLAY#AOCA610#7&272e3773&0&UID264#{guid}", "Generic Monitor (AG276QZD2)");
        assert_eq!(brand, "AOC");
        assert!(model.contains("AG276QZD2"));
    }
}