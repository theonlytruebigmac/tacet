//! Top-level detection orchestrator.
//!
//! Coordinates audio decoding, fingerprinting, reference bootstrap, and
//! per-episode matching. Callers with a full season in hand should use
//! [`detect_season`]; for per-episode incremental detection use
//! [`detect_single_episode`].

use std::path::{Path, PathBuf};

use anyhow::Result;
use rayon::prelude::*;
use tracing::{debug, info, warn};

use crate::audio::{self, AudioRegion};
use crate::blackframe;
use crate::boundary;
use crate::fingerprint::{self, Fingerprint, FingerprintKind};
use crate::matching::{self, ReferenceFingerprint};
#[cfg(feature = "store")]
use crate::storage::Store;
use crate::{Config, Segment, SegmentMarkers};

/// One episode on disk, addressable by stable id and season-relative number.
#[derive(Debug, Clone)]
pub struct EpisodeFile {
    pub id: String,
    pub path: PathBuf,
    pub episode_number: u32,
}

/// A season's worth of episodes to scan together.
pub struct Season {
    pub series_id: String,
    pub season_number: u32,
    pub episodes: Vec<EpisodeFile>,
}

/// Output of [`detect_season`].
pub struct DetectionResult {
    pub markers: Vec<SegmentMarkers>,
    pub intro_references: Vec<ReferenceFingerprint>,
    pub credits_references: Vec<ReferenceFingerprint>,
}

/// Just the references — produced by [`bootstrap_season`] when the caller
/// wants to persist the reference set themselves and run per-file detection
/// later via [`detect_single_episode`].
pub struct SeasonReferences {
    pub intro: Vec<ReferenceFingerprint>,
    pub credits: Vec<ReferenceFingerprint>,
}

/// Number of episodes used to bootstrap each reference fingerprint.
const BOOTSTRAP_SIZE: usize = 3;

/// Below this hash count, a bootstrapped reference is considered too noisy to
/// be useful (likely the bootstrap episodes don't actually share content).
const MIN_REFERENCE_HASHES: usize = 100;

/// Detect intros + credits across an entire season.
///
/// Pipeline:
/// 1. Decode + fingerprint intro and credits windows of every episode in parallel.
/// 2. Adaptive bootstrap: build references from groups of episodes that share
///    intro/credits content; iterate to catch mid-season OP/ED swaps.
/// 3. Match every episode against the resulting reference set and snap the
///    boundaries on the best-fitting reference.
pub fn detect_season(season: &Season, config: &Config) -> Result<DetectionResult> {
    if season.episodes.is_empty() {
        return Ok(DetectionResult {
            markers: vec![],
            intro_references: vec![],
            credits_references: vec![],
        });
    }

    info!(
        series = %season.series_id,
        season = season.season_number,
        episodes = season.episodes.len(),
        "fingerprinting season"
    );

    let prints = analyze_season(season, config)?;
    Ok(match_analyses(prints, config))
}

fn match_analyses(prints: Vec<EpisodeAnalysis>, config: &Config) -> DetectionResult {
    let intro_fps: Vec<&Fingerprint> =
        prints.iter().filter_map(|p| p.intro.as_ref().map(|w| &w.fp)).collect();
    let credits_fps: Vec<&Fingerprint> =
        prints.iter().filter_map(|p| p.credits.as_ref().map(|w| &w.fp)).collect();

    let intro_refs = adaptive_bootstrap(&intro_fps, config, "intro");
    let credits_refs = adaptive_bootstrap(&credits_fps, config, "credits");

    let markers = prints
        .par_iter()
        .map(|p| {
            let intro = p
                .intro
                .as_ref()
                .and_then(|w| match_to_segment(&intro_refs, w, config, FingerprintKind::Intro));
            let credits = p
                .credits
                .as_ref()
                .and_then(|w| match_to_segment(&credits_refs, w, config, FingerprintKind::Credits))
                .or_else(|| credits_blackframe_fallback(&p.path, p.credits.as_ref(), config));
            SegmentMarkers {
                episode_id: p.episode_id.clone(),
                intro,
                credits,
            }
        })
        .collect();

    DetectionResult {
        markers,
        intro_references: intro_refs,
        credits_references: credits_refs,
    }
}

fn analyze_season(season: &Season, config: &Config) -> Result<Vec<EpisodeAnalysis>> {
    season
        .episodes
        .par_iter()
        .map(|ep| analyze_episode(ep, config))
        .collect()
}

/// Detect intros + credits for a single episode against pre-built references.
///
/// Each window can have multiple references (e.g. an anime with an OP swap
/// mid-season). The window matches the reference that gives the highest
/// confidence above `config.match_threshold`. Empty slices mean "not yet
/// bootstrapped" and the matching window is reported as not found.
///
/// Incremental path: a new episode arrives → load the season references from
/// storage → call this → save the result.
pub fn detect_single_episode(
    path: &Path,
    episode_id: &str,
    intro_references: &[ReferenceFingerprint],
    credits_references: &[ReferenceFingerprint],
    config: &Config,
) -> Result<SegmentMarkers> {
    let intro_window = if intro_references.is_empty() {
        None
    } else {
        decode_and_fingerprint(path, FingerprintKind::Intro, config).ok()
    };
    let credits_window = if credits_references.is_empty() {
        None
    } else {
        decode_and_fingerprint(path, FingerprintKind::Credits, config).ok()
    };

    let intro = intro_window
        .as_ref()
        .and_then(|w| match_to_segment(intro_references, w, config, FingerprintKind::Intro));
    let credits = credits_window
        .as_ref()
        .and_then(|w| match_to_segment(credits_references, w, config, FingerprintKind::Credits))
        .or_else(|| credits_blackframe_fallback(path, credits_window.as_ref(), config));

    Ok(SegmentMarkers {
        episode_id: episode_id.to_string(),
        intro,
        credits,
    })
}

/// Build the reference set for a season *without* running per-episode
/// detection. The caller is expected to persist the resulting references in
/// their own storage layer, then call [`detect_single_episode`] per file as
/// new episodes arrive.
///
/// Most media servers want this shape: a one-time bootstrap job that fires
/// when a show has accumulated enough episodes, plus per-file detection on
/// the discovery pipeline. Returns an empty [`SeasonReferences`] when fewer
/// than 3 files have decodable audio in either window — the caller treats
/// "not yet ready" the same as "no references".
pub fn bootstrap_season(paths: &[&Path], config: &Config) -> Result<SeasonReferences> {
    if paths.len() < BOOTSTRAP_SIZE {
        return Ok(SeasonReferences {
            intro: vec![],
            credits: vec![],
        });
    }

    let prints: Vec<EpisodeAnalysis> = paths
        .par_iter()
        .enumerate()
        .map(|(i, p)| {
            let ep = EpisodeFile {
                id: format!("bootstrap-{i}"),
                path: (*p).to_path_buf(),
                episode_number: (i as u32) + 1,
            };
            analyze_episode(&ep, config)
        })
        .collect::<Result<Vec<_>>>()?;

    let intro_fps: Vec<&Fingerprint> =
        prints.iter().filter_map(|p| p.intro.as_ref().map(|w| &w.fp)).collect();
    let credits_fps: Vec<&Fingerprint> =
        prints.iter().filter_map(|p| p.credits.as_ref().map(|w| &w.fp)).collect();

    Ok(SeasonReferences {
        intro: adaptive_bootstrap(&intro_fps, config, "intro"),
        credits: adaptive_bootstrap(&credits_fps, config, "credits"),
    })
}

/// Run [`detect_season`] and persist everything: markers, per-episode
/// fingerprints (so a later incremental run can skip re-decoding), and the
/// bootstrapped references.
///
/// Returns early without writing references if the season is too small to
/// bootstrap — callers should treat a missing reference as "not yet ready".
///
/// Gated on the `store` feature.
#[cfg(feature = "store")]
pub fn detect_and_persist(
    season: &Season,
    store: &Store,
    config: &Config,
) -> Result<DetectionResult> {
    let prints = analyze_season(season, config)?;
    persist_episode_fingerprints(store, season, &prints)?;

    if season.episodes.len() < BOOTSTRAP_SIZE {
        warn!(
            episodes = season.episodes.len(),
            bootstrap = BOOTSTRAP_SIZE,
            "not enough episodes to bootstrap a reference; fingerprinting only"
        );
        return Ok(DetectionResult {
            markers: prints
                .into_iter()
                .map(|p| SegmentMarkers {
                    episode_id: p.episode_id,
                    intro: None,
                    credits: None,
                })
                .collect(),
            intro_references: vec![],
            credits_references: vec![],
        });
    }

    let result = match_analyses(prints, config);

    for m in &result.markers {
        store.save_markers(m)?;
    }
    store.save_references(
        &season.series_id,
        season.season_number,
        FingerprintKind::Intro,
        &result.intro_references,
    )?;
    store.save_references(
        &season.series_id,
        season.season_number,
        FingerprintKind::Credits,
        &result.credits_references,
    )?;
    Ok(result)
}

struct AnalyzedWindow {
    region: AudioRegion,
    fp: Fingerprint,
}

struct EpisodeAnalysis {
    episode_id: String,
    // Only read by the `store`-gated persist path; harmlessly carried
    // around in non-store builds so the analysis pipeline doesn't need to
    // know about the feature flag.
    #[cfg_attr(not(feature = "store"), allow(dead_code))]
    episode_number: u32,
    path: PathBuf,
    intro: Option<AnalyzedWindow>,
    credits: Option<AnalyzedWindow>,
}

fn analyze_episode(ep: &EpisodeFile, config: &Config) -> Result<EpisodeAnalysis> {
    debug!(id = %ep.id, path = %ep.path.display(), "fingerprinting");
    let intro = match decode_and_fingerprint(&ep.path, FingerprintKind::Intro, config) {
        Ok(w) => Some(w),
        Err(e) => {
            warn!(id = %ep.id, error = format!("{e:#}"), "intro decode failed");
            None
        }
    };
    let credits = match decode_and_fingerprint(&ep.path, FingerprintKind::Credits, config) {
        Ok(w) => Some(w),
        Err(e) => {
            warn!(id = %ep.id, error = format!("{e:#}"), "credits decode failed");
            None
        }
    };
    Ok(EpisodeAnalysis {
        episode_id: ep.id.clone(),
        episode_number: ep.episode_number,
        path: ep.path.clone(),
        intro,
        credits,
    })
}

fn decode_and_fingerprint(
    path: &Path,
    kind: FingerprintKind,
    config: &Config,
) -> Result<AnalyzedWindow> {
    let region = match kind {
        FingerprintKind::Intro => audio::decode_intro_region(path, config)?,
        FingerprintKind::Credits => audio::decode_credits_region(path, config)?,
    };
    let fp = fingerprint::fingerprint(&region, config);
    Ok(AnalyzedWindow { region, fp })
}

/// Build N reference fingerprints by iteratively bootstrapping from groups of
/// fingerprints that *don't* match any existing reference.
///
/// This handles seasons where the intro or credits audio changes mid-season —
/// e.g. anime swapping OPs around episode 14. After building a reference from
/// the first 3 fingerprints, any fingerprints that fail to match are clustered
/// together and the next 3 of those are used for a second bootstrap, and so on.
fn adaptive_bootstrap(
    fingerprints: &[&Fingerprint],
    config: &Config,
    label: &str,
) -> Vec<ReferenceFingerprint> {
    let mut refs: Vec<ReferenceFingerprint> = Vec::new();
    let mut remaining: Vec<&Fingerprint> = fingerprints.to_vec();

    while remaining.len() >= BOOTSTRAP_SIZE {
        let bootstrap: Vec<&Fingerprint> = remaining.iter().take(BOOTSTRAP_SIZE).copied().collect();
        let Some(candidate) = matching::build_reference(&bootstrap, config) else {
            break;
        };
        if candidate.len() < MIN_REFERENCE_HASHES {
            // Too few hashes survived the intersection — the bootstrap episodes
            // don't actually share content. Drop the first one and try again so
            // we don't get stuck on an outlier.
            debug!(
                kind = label,
                hashes = candidate.len(),
                "bootstrap intersection too sparse; advancing one episode"
            );
            remaining.remove(0);
            continue;
        }

        // Compute the post-match remaining set without committing yet — a
        // candidate that catches nothing (not even its own bootstrap) would
        // otherwise spin forever rebuilding the same useless reference.
        let before = remaining.len();
        let new_remaining: Vec<&Fingerprint> = remaining
            .iter()
            .copied()
            .filter(|fp| matching::match_against_reference(&candidate, fp, config).is_none())
            .collect();

        if new_remaining.len() == before {
            debug!(
                kind = label,
                hashes = candidate.len(),
                "candidate matched no episodes (likely failed min_segment_seconds); advancing one"
            );
            remaining.remove(0);
            continue;
        }

        info!(
            kind = label,
            cluster = refs.len(),
            hashes = candidate.len(),
            bootstrap = bootstrap.len(),
            matched = before - new_remaining.len(),
            remaining = new_remaining.len(),
            "built reference fingerprint"
        );

        refs.push(candidate);
        remaining = new_remaining;
    }

    if !remaining.is_empty() {
        debug!(
            kind = label,
            unmatched = remaining.len(),
            "leftover fingerprints with no matching reference"
        );
    }
    refs
}

/// Locate the credits roll via the blackframe heuristic when the audio
/// fingerprint match was rejected (no shared ED audio across episodes).
///
/// Returns `None` when the fallback is disabled, ffmpeg isn't available, the
/// file duration is unknown, or no qualifying black segment is found.
fn credits_blackframe_fallback(
    path: &Path,
    credits_window: Option<&AnalyzedWindow>,
    config: &Config,
) -> Option<Segment> {
    if !config.blackframe_fallback {
        return None;
    }
    let total = credits_window?.region.total_duration?;

    let mut opts = blackframe::ScanOptions {
        scan_seconds: config.blackframe_scan_minutes as f64 * 60.0,
        min_black_seconds: config.blackframe_min_seconds as f64,
        pix_threshold: config.blackframe_pix_threshold as f64,
        sample_fps: config.blackframe_fps as f64,
        hwaccel: config.blackframe_hwaccel.clone(),
        timeout: std::time::Duration::from_secs(config.blackframe_timeout_seconds),
    };

    let segments = match blackframe::scan_tail(path, total, &opts) {
        Ok(s) => s,
        Err(e) if opts.hwaccel.is_some() => {
            // Broken / hanging hwaccel (the VAAPI 8-minute hang is the
            // motivating case). Retry once with software decode before
            // giving up on this episode.
            debug!(
                error = format!("{e:#}"),
                "blackframe hwaccel scan failed; retrying with software decode",
            );
            opts.hwaccel = None;
            match blackframe::scan_tail(path, total, &opts) {
                Ok(s) => s,
                Err(e2) => {
                    debug!(
                        error = format!("{e2:#}"),
                        "blackframe software fallback also failed; skipping",
                    );
                    return None;
                }
            }
        }
        Err(e) => {
            debug!(error = format!("{e:#}"), "blackframe scan failed; skipping fallback");
            return None;
        }
    };

    let start = blackframe::pick_credits_start(&segments, config.blackframe_min_seconds as f64)?;
    let end = total;
    if end - start < config.min_credits_seconds as f64 {
        return None;
    }
    debug!(start, end, "credits located via blackframe fallback");
    Some(Segment {
        start,
        end,
        // Heuristic detection — fixed score, the authoritative signal is
        // `source = Blackframe` so callers can distinguish from fingerprint
        // matches without parsing magic numbers out of `confidence`.
        confidence: 0.5,
        source: crate::SegmentSource::Blackframe,
    })
}

fn match_to_segment(
    references: &[ReferenceFingerprint],
    window: &AnalyzedWindow,
    config: &Config,
    kind: FingerprintKind,
) -> Option<Segment> {
    if references.is_empty() {
        return None;
    }
    let m = matching::match_against_references(references, &window.fp, config)?;
    let (start, end) = boundary::refine(&window.region, m.start_seconds, m.end_seconds);
    if end - start < config.min_segment_seconds as f64 {
        return None;
    }

    if kind == FingerprintKind::Credits {
        // Reject obvious false-positives: too short to be real end-credits, or
        // landing somewhere in the middle of the credits scan window instead of
        // anchored to the file tail.
        if (end - start) < config.min_credits_seconds as f64 {
            tracing::debug!(
                duration = end - start,
                "rejecting credits match below min_credits_seconds"
            );
            return None;
        }
        if let Some(total) = window.region.total_duration {
            let tail_gap = total - end;
            if tail_gap > config.max_credits_tail_gap as f64 {
                tracing::debug!(
                    tail_gap,
                    "rejecting credits match too far from file end"
                );
                return None;
            }
        }
    }

    Some(Segment {
        start,
        end,
        confidence: m.confidence,
        source: crate::SegmentSource::AudioFingerprint,
    })
}

#[cfg(feature = "store")]
fn persist_episode_fingerprints(
    store: &Store,
    season: &Season,
    prints: &[EpisodeAnalysis],
) -> Result<()> {
    for p in prints {
        if let Some(w) = &p.intro {
            store.save_episode_fingerprint(
                &season.series_id,
                season.season_number,
                p.episode_number,
                FingerprintKind::Intro,
                &w.fp,
            )?;
        }
        if let Some(w) = &p.credits {
            store.save_episode_fingerprint(
                &season.series_id,
                season.season_number,
                p.episode_number,
                FingerprintKind::Credits,
                &w.fp,
            )?;
        }
    }
    Ok(())
}
