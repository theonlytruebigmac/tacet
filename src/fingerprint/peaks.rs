//! Spectral peak extraction from STFT magnitude frames.
//!
//! Uses logarithmic frequency bands with local maximum detection.
//! Each frame contributes at most `num_bands` peaks — one per band.
//!
//! Performance critical: this runs on every frame of every episode.
//! The inner loop is designed to auto-vectorize with LLVM.

use crate::Config;

/// A spectral peak: frequency bin + time frame + magnitude
#[derive(Debug, Clone, Copy)]
pub struct SpectralPeak {
    pub frame: u32,
    pub bin: u16,
    pub magnitude: f32,
}

/// Extract peaks from the full spectrogram using logarithmic band partitioning.
///
/// Returns peaks sorted by (frame, bin) for efficient constellation pairing.
pub fn extract_peaks(spectrogram: &[Vec<f32>], config: &Config) -> Vec<SpectralPeak> {
    let num_bins = spectrogram.first().map(|f| f.len()).unwrap_or(0);
    if num_bins == 0 {
        return Vec::new();
    }

    let bands = compute_band_edges(num_bins, config.num_bands);

    // Pre-allocate: ~num_bands peaks per frame
    let mut peaks = Vec::with_capacity(spectrogram.len() * config.num_bands);

    for (frame_idx, magnitudes) in spectrogram.iter().enumerate() {
        for band in &bands {
            if let Some(peak) = find_band_peak(magnitudes, band.0, band.1, frame_idx as u32) {
                peaks.push(peak);
            }
        }
    }

    peaks
}

/// Compute logarithmically-spaced frequency band boundaries.
///
/// Band edges grow exponentially, matching human pitch perception
/// and ensuring good coverage across the spectrum.
fn compute_band_edges(num_bins: usize, num_bands: usize) -> Vec<(usize, usize)> {
    let min_bin = 10; // skip DC and very low bins
    let max_bin = num_bins.saturating_sub(1);

    if max_bin <= min_bin {
        return vec![(min_bin, max_bin)];
    }

    let log_min = (min_bin as f64).ln();
    let log_max = (max_bin as f64).ln();
    let step = (log_max - log_min) / num_bands as f64;

    (0..num_bands)
        .map(|i| {
            let lo = (log_min + step * i as f64).exp() as usize;
            let hi = (log_min + step * (i + 1) as f64).exp() as usize;
            (lo.max(min_bin), hi.min(max_bin))
        })
        .filter(|(lo, hi)| hi > lo)
        .collect()
}

/// Find the strongest peak within a frequency band.
///
/// Uses a simple local maximum check: the peak must be greater than
/// both its immediate neighbors to avoid flat spectral regions.
#[inline]
fn find_band_peak(
    magnitudes: &[f32],
    band_lo: usize,
    band_hi: usize,
    frame: u32,
) -> Option<SpectralPeak> {
    let mut best_bin = 0u16;
    let mut best_mag = 0.0f32;

    // Inner loop: designed for auto-vectorization
    for bin in band_lo..=band_hi.min(magnitudes.len() - 1) {
        let mag = magnitudes[bin];
        if mag > best_mag {
            // Local maximum check (skip edges of band)
            let is_local_max = (bin == band_lo || mag > magnitudes[bin - 1])
                && (bin >= band_hi || mag > magnitudes[bin + 1]);

            if is_local_max {
                best_mag = mag;
                best_bin = bin as u16;
            }
        }
    }

    // Noise floor gate: ignore very quiet peaks
    if best_mag > 1e-4 {
        Some(SpectralPeak {
            frame,
            bin: best_bin,
            magnitude: best_mag,
        })
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_band_edges_logarithmic() {
        let bands = compute_band_edges(2049, 6);
        assert_eq!(bands.len(), 6);
        // Each band should be wider than the previous (log spacing)
        for i in 1..bands.len() {
            let prev_width = bands[i - 1].1 - bands[i - 1].0;
            let curr_width = bands[i].1 - bands[i].0;
            assert!(curr_width >= prev_width, "Bands should grow logarithmically");
        }
    }

    #[test]
    fn test_peak_extraction_empty() {
        let config = Config::default();
        let peaks = extract_peaks(&[], &config);
        assert!(peaks.is_empty());
    }
}
