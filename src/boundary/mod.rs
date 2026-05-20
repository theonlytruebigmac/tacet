//! Boundary refinement.
//!
//! The matcher reports a rough span (the min/max query frames that voted for
//! the dominant alignment). That span tends to over-shoot by a few seconds
//! because Shazam-style hashing keeps firing across short silences. We snap
//! each edge to the nearest sharp energy transition in the surrounding audio
//! so the reported start/end land on clean cut points.
//!
//! Algorithm (per edge):
//! 1. Compute short-window RMS energy in dB across a search neighborhood.
//! 2. Find the frame index where the dB delta between adjacent windows is
//!    most extreme (positive for an onset, negative for an offset).
//! 3. Translate that frame index back to seconds.
//!
//! Pure-Rust, no allocation in the hot loop — safe to call per-episode.

use crate::audio::AudioRegion;

/// How far (in seconds) to look on either side of the rough boundary.
const SEARCH_RADIUS_SECONDS: f64 = 2.0;

/// Length of each RMS analysis window in seconds.
const RMS_WINDOW_SECONDS: f64 = 0.05;

/// Refine the (start, end) seconds of a matched span by snapping to the
/// strongest energy onset before `rough_start` and the strongest energy
/// offset after `rough_end`.
///
/// `rough_start` / `rough_end` are absolute seconds within the source file;
/// the region's `offset_seconds` tells us where the slice begins.
pub fn refine(region: &AudioRegion, rough_start: f64, rough_end: f64) -> (f64, f64) {
    if region.samples.is_empty() || region.sample_rate == 0 {
        return (rough_start, rough_end);
    }

    let window_samples =
        ((RMS_WINDOW_SECONDS * region.sample_rate as f64) as usize).max(1);
    let rms = compute_rms_db(&region.samples, window_samples);
    if rms.len() < 3 {
        return (rough_start, rough_end);
    }

    let window_seconds = window_samples as f64 / region.sample_rate as f64;
    let radius_windows = (SEARCH_RADIUS_SECONDS / window_seconds).ceil() as isize;

    let to_window = |t_abs: f64| -> isize {
        ((t_abs - region.offset_seconds) / window_seconds).round() as isize
    };
    let to_seconds = |w: isize| -> f64 {
        region.offset_seconds + (w as f64 + 0.5) * window_seconds
    };

    let start_w = find_best_transition(&rms, to_window(rough_start), radius_windows, Edge::Onset);
    let end_w = find_best_transition(&rms, to_window(rough_end), radius_windows, Edge::Offset);

    let refined_start = start_w
        .map(to_seconds)
        .unwrap_or(rough_start)
        .clamp(region.offset_seconds, rough_end);
    let refined_end = end_w
        .map(to_seconds)
        .unwrap_or(rough_end)
        .clamp(refined_start, rough_end + SEARCH_RADIUS_SECONDS);

    (refined_start, refined_end)
}

#[derive(Clone, Copy)]
enum Edge {
    Onset,
    Offset,
}

fn find_best_transition(
    rms: &[f32],
    center: isize,
    radius: isize,
    edge: Edge,
) -> Option<isize> {
    let lo = (center - radius).max(1);
    let hi = (center + radius).min(rms.len() as isize - 1);
    if lo >= hi {
        return None;
    }

    let mut best_w: Option<isize> = None;
    let mut best_score = f32::NEG_INFINITY;

    for w in lo..=hi {
        let delta = rms[w as usize] - rms[w as usize - 1];
        let score = match edge {
            Edge::Onset => delta,
            Edge::Offset => -delta,
        };
        if score > best_score {
            best_score = score;
            best_w = Some(w);
        }
    }

    // Require the transition to be at least 3 dB — otherwise we're snapping to noise.
    if best_score >= 3.0 {
        best_w
    } else {
        None
    }
}

fn compute_rms_db(samples: &[f32], window: usize) -> Vec<f32> {
    let num_windows = samples.len() / window;
    let mut out = Vec::with_capacity(num_windows);
    for w in 0..num_windows {
        let start = w * window;
        let slice = &samples[start..start + window];
        let mean_sq = slice.iter().map(|s| s * s).sum::<f32>() / window as f32;
        let db = 10.0 * (mean_sq + 1e-12).log10();
        out.push(db);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn region_with(samples: Vec<f32>, sample_rate: u32, offset: f64) -> AudioRegion {
        AudioRegion {
            samples,
            sample_rate,
            offset_seconds: offset,
            total_duration: None,
        }
    }

    #[test]
    fn snaps_start_to_energy_onset() {
        // 1 second of silence, then 1 second of tone, at 16 kHz.
        let mut samples = vec![0.0_f32; 16_000];
        samples.extend((0..16_000).map(|i| (i as f32 * 0.05).sin() * 0.5));
        let region = region_with(samples, 16_000, 0.0);

        // Pretend the matcher said the intro starts at 0.5s — the real onset is at 1.0s.
        let (start, _end) = refine(&region, 0.5, 1.8);
        assert!((start - 1.0).abs() < 0.1, "got {start}, expected ~1.0");
    }

    #[test]
    fn snaps_end_to_energy_offset() {
        // 1 second of tone, then 1 second of silence.
        let mut samples: Vec<f32> = (0..16_000).map(|i| (i as f32 * 0.05).sin() * 0.5).collect();
        samples.extend(vec![0.0_f32; 16_000]);
        let region = region_with(samples, 16_000, 0.0);

        // Matcher overshoots into silence.
        let (_start, end) = refine(&region, 0.1, 1.5);
        assert!((end - 1.0).abs() < 0.1, "got {end}, expected ~1.0");
    }

    #[test]
    fn falls_back_when_no_clear_transition() {
        let samples = vec![0.1_f32; 16_000 * 3];
        let region = region_with(samples, 16_000, 10.0);
        let (start, end) = refine(&region, 11.0, 12.0);
        assert!((start - 11.0).abs() < 1e-6);
        assert!((end - 12.0).abs() < 1e-6);
    }
}
