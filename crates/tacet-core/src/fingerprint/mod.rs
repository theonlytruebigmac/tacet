//! Fingerprint pipeline: audio region → STFT → spectral peaks → constellation hashes.
//!
//! `fingerprint()` runs the full pipeline; `compute_stft()` is exposed for the bench
//! and for callers that want intermediate frames (e.g. boundary refinement).

use num_complex::Complex;
use rustfft::FftPlanner;

use crate::audio::AudioRegion;
use crate::Config;

pub mod constellation;
pub mod peaks;

pub use constellation::Fingerprint;

/// Which window of an episode a fingerprint covers — intro (head) or credits
/// (tail). Used by detection routing and (when the `store` feature is on)
/// the on-disk file naming.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum FingerprintKind {
    Intro,
    Credits,
}

/// A single fingerprint hash anchored at a specific frame in the source audio.
#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize)]
pub struct FpHash {
    pub hash: u32,
    pub frame: u32,
}

/// Run the full pipeline on a decoded audio region.
pub fn fingerprint(region: &AudioRegion, config: &Config) -> Fingerprint {
    let spectrogram = compute_stft(&region.samples, config);
    let extracted = peaks::extract_peaks(&spectrogram, config);
    constellation::build_fingerprint(extracted, config, region.offset_seconds, region.sample_rate)
}

/// Compute the magnitude spectrogram using a Hann-windowed STFT.
///
/// Returns one magnitude vector per frame (length `fft_size / 2 + 1`).
pub fn compute_stft(samples: &[f32], config: &Config) -> Vec<Vec<f32>> {
    if samples.len() < config.fft_size {
        return Vec::new();
    }

    let mut planner = FftPlanner::<f32>::new();
    let fft = planner.plan_fft_forward(config.fft_size);
    let window = hann_window(config.fft_size);
    let num_bins = config.fft_size / 2 + 1;
    let num_frames = (samples.len() - config.fft_size) / config.hop_size + 1;

    let mut frames = Vec::with_capacity(num_frames);
    let mut buffer = vec![Complex::new(0.0_f32, 0.0_f32); config.fft_size];

    for f in 0..num_frames {
        let start = f * config.hop_size;
        for i in 0..config.fft_size {
            buffer[i] = Complex::new(samples[start + i] * window[i], 0.0);
        }
        fft.process(&mut buffer);

        let mags: Vec<f32> = buffer[..num_bins]
            .iter()
            .map(|c| (c.re * c.re + c.im * c.im).sqrt())
            .collect();
        frames.push(mags);
    }

    frames
}

fn hann_window(size: usize) -> Vec<f32> {
    (0..size)
        .map(|i| {
            let x = std::f32::consts::TAU * i as f32 / (size - 1) as f32;
            0.5 * (1.0 - x.cos())
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stft_produces_expected_frame_count() {
        let config = Config::default();
        let samples = vec![0.0_f32; config.fft_size * 4];
        let frames = compute_stft(&samples, &config);
        // (4*fft - fft)/hop + 1 = 3*fft/hop + 1 = 3*2 + 1 = 7
        assert_eq!(frames.len(), 7);
        assert_eq!(frames[0].len(), config.fft_size / 2 + 1);
    }

    #[test]
    fn stft_handles_undersized_input() {
        let config = Config::default();
        let frames = compute_stft(&[0.0; 10], &config);
        assert!(frames.is_empty());
    }
}
