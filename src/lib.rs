//! # Tacet — Ultra-fast audio fingerprinting for intro/credit detection
//!
//! Designed to be faster and more accurate than Plex's built-in detection.
//!
//! ## Key advantages over Plex:
//! - **No transcoder dependency**: Pure Rust audio decoding via symphonia
//! - **O(n) matching**: Reference-fingerprint strategy avoids O(n²) pairwise comparison
//! - **SIMD via rustfft**: FFT auto-selects AVX2/SSE4/NEON; hot loops shaped for autovectorization
//! - **Streaming decode**: Never loads a full episode into memory
//! - **Sub-second per episode**: Processes only the relevant windows (first/last N minutes)
//! - **Credits detection built-in**: Not a separate, inferior system
//! - **Short intros supported**: No arbitrary 20-second minimum

pub mod audio;
pub mod boundary;
pub mod fingerprint;
pub mod matching;
pub mod detection;
pub mod storage;

#[cfg(feature = "api")]
pub mod api;

pub mod cli;

/// Core configuration for the fingerprinting engine
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Config {
    /// Target sample rate for analysis (lower = faster, 16kHz is sweet spot)
    pub sample_rate: u32,
    /// FFT window size in samples
    pub fft_size: usize,
    /// Hop size between windows (fft_size / 2 is standard)
    pub hop_size: usize,
    /// Number of frequency bands for peak extraction
    pub num_bands: usize,
    /// How many minutes from the start to scan for intros
    pub intro_scan_minutes: f32,
    /// How many minutes from the end to scan for credits
    pub credits_scan_minutes: f32,
    /// Minimum segment duration in seconds to report
    pub min_segment_seconds: f32,
    /// Minimum matching hash ratio to consider a match
    pub match_threshold: f64,
    /// Number of peak pairs per anchor for fingerprint density
    pub fan_out: usize,
    /// Maximum time delta between anchor and target (in frames)
    pub max_target_delta: usize,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            sample_rate: 16_000,
            fft_size: 4096,
            hop_size: 2048,
            num_bands: 6,
            intro_scan_minutes: 10.0,
            credits_scan_minutes: 8.0,
            min_segment_seconds: 5.0, // Plex ignores <20s — we catch 5s+
            match_threshold: 0.08,
            fan_out: 5,
            max_target_delta: 50,
        }
    }
}

/// Result of detection for a single episode
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SegmentMarkers {
    pub episode_id: String,
    pub intro: Option<Segment>,
    pub credits: Option<Segment>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Segment {
    /// Start time in seconds
    pub start: f64,
    /// End time in seconds
    pub end: f64,
    /// Confidence score 0.0–1.0
    pub confidence: f64,
}
