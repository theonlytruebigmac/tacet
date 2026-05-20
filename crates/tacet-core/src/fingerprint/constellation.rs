//! Constellation map fingerprinting.
//!
//! Converts spectral peaks into a set of compact hashes by pairing each
//! "anchor" peak with several nearby "target" peaks. The hash encodes
//! (anchor_freq, target_freq, time_delta) — this is invariant to amplitude
//! and robust to noise.
//!
//! Fan-out factor controls density: higher = more hashes = more robust
//! matching at the cost of storage. For intro detection, we want dense
//! fingerprints since we're matching short segments.

use super::peaks::SpectralPeak;
use super::FpHash;
use crate::Config;

/// A complete fingerprint: a set of hashes with timing info.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Fingerprint {
    /// All hashes for this audio region
    pub hashes: Vec<FpHash>,
    /// Time offset of frame 0 relative to the start of the file (seconds)
    pub time_offset: f64,
    /// Seconds per frame (hop_size / sample_rate)
    pub frame_duration: f64,
}

impl Fingerprint {
    /// Convert a frame index to an absolute time in seconds.
    pub fn frame_to_seconds(&self, frame: u32) -> f64 {
        self.time_offset + frame as f64 * self.frame_duration
    }

    /// Total number of hashes.
    pub fn len(&self) -> usize {
        self.hashes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.hashes.is_empty()
    }
}

/// Build a fingerprint from extracted peaks using constellation pairing.
pub fn build_fingerprint(
    peaks: Vec<SpectralPeak>,
    config: &Config,
    time_offset: f64,
    sample_rate: u32,
) -> Fingerprint {
    let frame_duration = config.hop_size as f64 / sample_rate as f64;
    let mut hashes = Vec::with_capacity(peaks.len() * config.fan_out);

    // For each anchor peak, pair with the next `fan_out` peaks
    // within the target time window
    for (i, anchor) in peaks.iter().enumerate() {
        let mut paired = 0;

        for target in peaks[i + 1..].iter() {
            if paired >= config.fan_out {
                break;
            }

            let time_delta = target.frame.saturating_sub(anchor.frame);

            // Target must be ahead in time but within the max delta
            if time_delta == 0 || time_delta > config.max_target_delta as u32 {
                if time_delta > config.max_target_delta as u32 {
                    break; // Peaks are sorted by frame, so no more valid targets
                }
                continue;
            }

            let hash = compute_hash(anchor.bin, target.bin, time_delta as u16);

            hashes.push(FpHash {
                hash,
                frame: anchor.frame,
            });

            paired += 1;
        }
    }

    Fingerprint {
        hashes,
        time_offset,
        frame_duration,
    }
}

/// Hash a peak pair into a compact 32-bit token.
///
/// Layout: [anchor_freq:10 | target_freq:10 | time_delta:12]
/// This gives us 1024 freq bins × 1024 freq bins × 4096 time deltas.
#[inline(always)]
fn compute_hash(anchor_bin: u16, target_bin: u16, delta: u16) -> u32 {
    let a = (anchor_bin as u32) & 0x3FF; // 10 bits
    let t = (target_bin as u32) & 0x3FF; // 10 bits
    let d = (delta as u32) & 0xFFF; // 12 bits
    (a << 22) | (t << 12) | d
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_hash_packing() {
        let h = compute_hash(100, 200, 50);
        // Verify we can unpack
        let a = (h >> 22) & 0x3FF;
        let t = (h >> 12) & 0x3FF;
        let d = h & 0xFFF;
        assert_eq!(a, 100);
        assert_eq!(t, 200);
        assert_eq!(d, 50);
    }

    #[test]
    fn test_empty_peaks_produce_empty_fingerprint() {
        let config = Config::default();
        let fp = build_fingerprint(vec![], &config, 0.0, 16000);
        assert!(fp.is_empty());
    }
}
