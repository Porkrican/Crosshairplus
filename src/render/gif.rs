/// Returns the frame index for a looped GIF at `elapsed_ms`.  Kept pure so the
/// same timing is used by the overlay, the UI preview and tests.
pub fn frame_index(delays_ms: &[u32], elapsed_ms: u64, playing: bool) -> usize {
    if delays_ms.len() <= 1 || !playing {
        return 0;
    }
    let total: u64 = delays_ms.iter().map(|d| (*d).max(1) as u64).sum();
    if total == 0 {
        return 0;
    }
    let mut position = elapsed_ms % total;
    for (index, delay) in delays_ms.iter().enumerate() {
        let delay = (*delay).max(1) as u64;
        if position < delay {
            return index;
        }
        position -= delay;
    }
    0
}

/// Milliseconds until `elapsed_ms` reaches the next frame boundary. The preview
/// uses this to wake on the source's own cadence instead of a fixed timer.
/// None when there is nothing to animate.
pub fn ms_until_next(delays_ms: &[u32], elapsed_ms: u64, playing: bool) -> Option<u64> {
    if delays_ms.len() <= 1 || !playing {
        return None;
    }
    let total: u64 = delays_ms.iter().map(|d| (*d).max(1) as u64).sum();
    if total == 0 {
        return None;
    }
    let mut position = elapsed_ms % total;
    for delay in delays_ms {
        let delay = (*delay).max(1) as u64;
        if position < delay {
            return Some(delay - position);
        }
        position -= delay;
    }
    Some(total)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_timing_loops_and_honours_playing() {
        let delays = [100, 50, 150];
        assert_eq!(frame_index(&delays, 0, true), 0);
        assert_eq!(frame_index(&delays, 100, true), 1);
        assert_eq!(frame_index(&delays, 150, true), 2);
        assert_eq!(frame_index(&delays, 300, true), 0);
        assert_eq!(frame_index(&delays, 199, false), 0);
        assert_eq!(frame_index(&[], 500, true), 0);
    }

    #[test]
    fn next_frame_deadline_follows_the_source_delays() {
        let delays = [100, 50, 150];
        assert_eq!(ms_until_next(&delays, 0, true), Some(100));
        assert_eq!(ms_until_next(&delays, 99, true), Some(1));
        assert_eq!(ms_until_next(&delays, 100, true), Some(50), "second frame is 50ms");
        assert_eq!(ms_until_next(&delays, 149, true), Some(1));
        assert_eq!(ms_until_next(&delays, 150, true), Some(150), "third frame is 150ms");
        assert_eq!(ms_until_next(&delays, 299, true), Some(1));
        assert_eq!(ms_until_next(&delays, 300, true), Some(100), "the loop wraps");
        assert_eq!(ms_until_next(&delays, 100, false), None, "paused does not animate");
        assert_eq!(ms_until_next(&[40], 0, true), None, "a still image never advances");
        assert_eq!(ms_until_next(&[], 0, true), None);
    }
}
