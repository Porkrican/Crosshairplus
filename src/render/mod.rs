use std::sync::OnceLock;
use std::time::Instant;

/// Milliseconds since the process started. Shared by the overlay and the Aim
/// preview, so both pick the same animation frame for "now".
pub(crate) fn now_ms() -> u64 {
    static START: OnceLock<Instant> = OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_millis() as u64
}

pub mod crosshair;
pub mod decode;
pub mod engine;
pub mod gif;
pub mod preview;
pub mod scene;
pub mod vector;
