//! # tacet — ultra-fast audio fingerprinting + blackframe credits detection
//!
//! This is the engine, designed to be embedded in any media server. The HTTP
//! service and CLI live in separate crates (`tacet-api`, `tacet-cli`); the
//! SQLite persistence layer is gated behind the `store` feature so callers
//! that bring their own storage don't pay for rusqlite + bincode.
//!
//! ## Library usage
//!
//! ```no_run
//! use std::path::Path;
//! use tacet::{Config, detection};
//!
//! # fn demo() -> anyhow::Result<()> {
//! let config = Config::default();
//!
//! // Bootstrap a season's reference set from at least three episodes:
//! let paths = [
//!     Path::new("/media/show/s01e01.mkv"),
//!     Path::new("/media/show/s01e02.mkv"),
//!     Path::new("/media/show/s01e03.mkv"),
//! ];
//! let refs = detection::bootstrap_season(&paths, &config)?;
//!
//! // Persist `refs.intro` / `refs.credits` (each a `Vec<ReferenceFingerprint>`)
//! // however you like — they're plain `serde::Serialize`.
//!
//! // Per-episode detection once references are in hand:
//! let markers = detection::detect_single_episode(
//!     Path::new("/media/show/s01e04.mkv"),
//!     "show-s01e04",
//!     &refs.intro,
//!     &refs.credits,
//!     &config,
//! )?;
//! # Ok(()) }
//! ```
//!
//! ## What the engine provides
//!
//! - **Symphonia-first audio decode** with an automatic ffmpeg subprocess
//!   fallback for codecs symphonia rejects (HE-AAC, E-AC3/Atmos, PCM-in-MKV…).
//! - **Constellation-hash fingerprints** with adaptive multi-anchor
//!   bootstrapping that survives mid-season OP/ED swaps.
//! - **Blackframe credits fallback** for live-action shows whose end-credits
//!   are unique per episode — uses ffmpeg `blackdetect` on a downscaled,
//!   sub-sampled tail of the file.
//! - **Confidence math anchored to the reference**, so the match_threshold
//!   means the same thing regardless of scan window size.

pub mod audio;
pub mod blackframe;
pub mod boundary;
pub mod fingerprint;
pub mod matching;
pub mod detection;

#[cfg(feature = "store")]
pub mod storage;

/// Core configuration for the fingerprinting engine.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Config {
    /// Target sample rate for analysis (lower = faster, 16kHz is sweet spot).
    pub sample_rate: u32,
    /// FFT window size in samples.
    pub fft_size: usize,
    /// Hop size between windows (fft_size / 2 is standard).
    pub hop_size: usize,
    /// Number of frequency bands for peak extraction.
    pub num_bands: usize,
    /// How many minutes from the start to scan for intros.
    pub intro_scan_minutes: f32,
    /// How many minutes from the end to scan for credits.
    pub credits_scan_minutes: f32,
    /// Minimum segment duration in seconds to report (general floor; used by
    /// the matcher and as the intro minimum).
    pub min_segment_seconds: f32,
    /// Additional minimum length for a *credits* detection, beyond the general
    /// floor. Real end-credits are typically ≥30s; tighter than the intro
    /// minimum because short tail-anchored stings (network branding, leitmotifs)
    /// are common false positives.
    pub min_credits_seconds: f32,
    /// Reject a credits match whose end is more than this many seconds before
    /// the file end. Real credits land at the tail of the episode; matches in
    /// the middle of the credits scan window are almost always recurring
    /// non-credit content (scene transitions, act breaks).
    pub max_credits_tail_gap: f32,
    /// When audio fingerprinting cannot find credits, fall back to scanning
    /// the file tail for a long fade-to-black (typical live-action credits
    /// transition). Set false to disable the fallback entirely.
    pub blackframe_fallback: bool,
    /// Minutes from the end of the file to inspect for black frames. Kept
    /// tight (real credits land in the last 1-3 minutes) because the decode
    /// is the slow part of the fallback.
    pub blackframe_scan_minutes: f32,
    /// Sample rate (frames per second) for the blackframe decoder. Real
    /// credits transitions are seconds long, so 2 fps is plenty to detect
    /// them and ~12× cheaper than decoding at 24 fps.
    pub blackframe_fps: f32,
    /// Minimum duration (seconds) of a black segment to be treated as a
    /// credits transition rather than an intra-scene cut.
    pub blackframe_min_seconds: f32,
    /// Luma threshold used by ffmpeg `blackdetect`. Lower = stricter
    /// (only near-pure black qualifies). 0.10 catches dim fade-to-blacks.
    pub blackframe_pix_threshold: f32,
    /// Optional ffmpeg `-hwaccel` value to pass for the blackframe decode.
    ///
    /// Defaults to `None` (software decode). Set to `Some("auto")` to let
    /// ffmpeg pick an accelerator — useful for **single-file** workflows
    /// (a media server's per-job detection worker) where the GPU is idle
    /// and a 7× CPU reduction frees cores for other work like transcoding.
    ///
    /// Do **not** enable this for **batch / parallel** workflows (e.g. the
    /// `tacet scan` CLI fingerprinting a whole season at once): 10+ parallel
    /// blackframe calls serialize on the GPU's 1-4 decoder slots and total
    /// wall time goes *up* even as per-call CPU goes down.
    ///
    /// On failure or timeout (broken VAAPI drivers can hang for many
    /// minutes), detection retries automatically with software decode.
    pub blackframe_hwaccel: Option<String>,
    /// Wall-clock deadline (seconds) for the blackframe ffmpeg invocation.
    /// Some hardware accelerators (e.g. a broken VAAPI driver) silently
    /// hang for many minutes; this watchdog kills the child and falls back
    /// to software decode.
    pub blackframe_timeout_seconds: u64,
    /// Minimum matching hash ratio (fraction of *reference* hashes voting at
    /// the dominant alignment) required to accept a match.
    pub match_threshold: f64,
    /// Number of peak pairs per anchor for fingerprint density.
    pub fan_out: usize,
    /// Maximum time delta between anchor and target (in frames).
    pub max_target_delta: usize,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            sample_rate: 16_000,
            fft_size: 4096,
            hop_size: 2048,
            num_bands: 6,
            // 18 min default catches even the long-tail "abnormal cold open"
            // case — measured: Silo S01E05's intro starts at 14:28 and runs
            // ~90s, so a 15-min window only catches the head of the intro
            // music (often not enough hash overlap to match). 18 min covers
            // it without significantly bloating decode work for normal shows.
            intro_scan_minutes: 18.0,
            credits_scan_minutes: 8.0,
            min_segment_seconds: 5.0,
            min_credits_seconds: 30.0,
            max_credits_tail_gap: 30.0,
            blackframe_fallback: true,
            blackframe_scan_minutes: 3.0,
            blackframe_fps: 2.0,
            blackframe_min_seconds: 3.0,
            blackframe_pix_threshold: 0.10,
            // Default off — see field-level docs for why this isn't `Some("auto")`.
            // Media servers running per-file detection should flip this on.
            blackframe_hwaccel: None,
            blackframe_timeout_seconds: 60,
            match_threshold: 0.08,
            fan_out: 5,
            max_target_delta: 50,
        }
    }
}

/// Which detection strategy produced a segment.
///
/// Callers that already have a marker-source taxonomy (e.g. a media server
/// distinguishing chapter-derived markers from heuristic ones) can map this
/// enum into their own. The `confidence` field is the algorithm's own signal;
/// `source` tells the caller *how* that signal was derived.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SegmentSource {
    /// Matched against a bootstrapped audio fingerprint reference.
    AudioFingerprint,
    /// Located via the ffmpeg `blackdetect` fade-to-black heuristic, after
    /// audio fingerprinting could not find shared content (live-action
    /// credits with per-episode unique audio).
    Blackframe,
}

/// Result of detection for a single episode.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SegmentMarkers {
    pub episode_id: String,
    pub intro: Option<Segment>,
    pub credits: Option<Segment>,
}

/// One detected intro or credits region.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Segment {
    /// Start time in seconds.
    pub start: f64,
    /// End time in seconds.
    pub end: f64,
    /// Confidence score 0.0–1.0. For [`SegmentSource::AudioFingerprint`] this
    /// is the fraction of the reference's hashes that voted at the dominant
    /// alignment. For [`SegmentSource::Blackframe`] this is a fixed heuristic
    /// score — the source enum is the authoritative signal of *how* it was
    /// found, not the confidence number.
    pub confidence: f64,
    /// How this segment was located. New in v0.2; old serialized payloads
    /// missing this field deserialize as `AudioFingerprint` for back-compat.
    #[serde(default = "default_source")]
    pub source: SegmentSource,
}

fn default_source() -> SegmentSource {
    SegmentSource::AudioFingerprint
}

impl Segment {
    /// Start time as milliseconds (truncated toward zero). Convenience for
    /// callers whose storage layer holds marker positions as `i64 ms`.
    #[inline]
    pub fn start_ms(&self) -> i64 {
        (self.start * 1000.0) as i64
    }
    /// End time as milliseconds (truncated toward zero).
    #[inline]
    pub fn end_ms(&self) -> i64 {
        (self.end * 1000.0) as i64
    }
    /// Duration in seconds.
    #[inline]
    pub fn duration(&self) -> f64 {
        (self.end - self.start).max(0.0)
    }
}
