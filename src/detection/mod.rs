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
use crate::boundary;
use crate::fingerprint::{self, Fingerprint};
use crate::matching::{self, ReferenceFingerprint};
use crate::storage::{FingerprintKind, Store};
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
    pub intro_reference: Option<ReferenceFingerprint>,
    pub credits_reference: Option<ReferenceFingerprint>,
}

/// Number of episodes used to bootstrap the reference fingerprint.
const BOOTSTRAP_SIZE: usize = 3;

/// Detect intros + credits across an entire season.
///
/// Pipeline:
/// 1. Decode + fingerprint intro and credits windows of every episode in parallel.
/// 2. Bootstrap intro/credits references from the first `BOOTSTRAP_SIZE` episodes.
/// 3. Match every episode against those references and snap the boundaries.
pub fn detect_season(season: &Season, config: &Config) -> Result<DetectionResult> {
    if season.episodes.is_empty() {
        return Ok(DetectionResult {
            markers: vec![],
            intro_reference: None,
            credits_reference: None,
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

    let intro_ref = build_reference(&intro_fps, config, "intro");
    let credits_ref = build_reference(&credits_fps, config, "credits");

    let markers = prints
        .par_iter()
        .map(|p| SegmentMarkers {
            episode_id: p.episode_id.clone(),
            intro: p
                .intro
                .as_ref()
                .zip(intro_ref.as_ref())
                .and_then(|(w, r)| match_to_segment(r, w, config)),
            credits: p
                .credits
                .as_ref()
                .zip(credits_ref.as_ref())
                .and_then(|(w, r)| match_to_segment(r, w, config)),
        })
        .collect();

    DetectionResult {
        markers,
        intro_reference: intro_ref,
        credits_reference: credits_ref,
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
/// If a reference is `None` for a window, that window is reported as not found.
/// Incremental path: a new episode arrives → load the season references from
/// storage → call this → save the result.
pub fn detect_single_episode(
    path: &Path,
    episode_id: &str,
    intro_reference: Option<&ReferenceFingerprint>,
    credits_reference: Option<&ReferenceFingerprint>,
    config: &Config,
) -> Result<SegmentMarkers> {
    let intro_window = match intro_reference {
        Some(_) => decode_and_fingerprint(path, FingerprintKind::Intro, config).ok(),
        None => None,
    };
    let credits_window = match credits_reference {
        Some(_) => decode_and_fingerprint(path, FingerprintKind::Credits, config).ok(),
        None => None,
    };

    let intro = intro_window
        .as_ref()
        .zip(intro_reference)
        .and_then(|(w, r)| match_to_segment(r, w, config));
    let credits = credits_window
        .as_ref()
        .zip(credits_reference)
        .and_then(|(w, r)| match_to_segment(r, w, config));

    Ok(SegmentMarkers {
        episode_id: episode_id.to_string(),
        intro,
        credits,
    })
}

/// Run [`detect_season`] and persist everything: markers, per-episode
/// fingerprints (so a later incremental run can skip re-decoding), and the
/// bootstrapped references.
///
/// Returns early without writing references if the season is too small to
/// bootstrap — callers should treat a missing reference as "not yet ready".
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
            intro_reference: None,
            credits_reference: None,
        });
    }

    let result = match_analyses(prints, config);

    for m in &result.markers {
        store.save_markers(m)?;
    }
    if let Some(r) = &result.intro_reference {
        store.save_reference(&season.series_id, season.season_number, FingerprintKind::Intro, r)?;
    }
    if let Some(r) = &result.credits_reference {
        store.save_reference(
            &season.series_id,
            season.season_number,
            FingerprintKind::Credits,
            r,
        )?;
    }
    Ok(result)
}

struct AnalyzedWindow {
    region: AudioRegion,
    fp: Fingerprint,
}

struct EpisodeAnalysis {
    episode_id: String,
    episode_number: u32,
    intro: Option<AnalyzedWindow>,
    credits: Option<AnalyzedWindow>,
}

fn analyze_episode(ep: &EpisodeFile, config: &Config) -> Result<EpisodeAnalysis> {
    debug!(id = %ep.id, path = %ep.path.display(), "fingerprinting");
    let intro = decode_and_fingerprint(&ep.path, FingerprintKind::Intro, config).ok();
    let credits = decode_and_fingerprint(&ep.path, FingerprintKind::Credits, config).ok();
    if intro.is_none() && credits.is_none() {
        warn!(id = %ep.id, "no decodable audio in either window");
    }
    Ok(EpisodeAnalysis {
        episode_id: ep.id.clone(),
        episode_number: ep.episode_number,
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

fn build_reference(
    fingerprints: &[&Fingerprint],
    config: &Config,
    label: &str,
) -> Option<ReferenceFingerprint> {
    let take = fingerprints.iter().take(BOOTSTRAP_SIZE).copied().collect::<Vec<_>>();
    if take.len() < BOOTSTRAP_SIZE {
        return None;
    }
    let r = matching::build_reference(&take, config)?;
    info!(
        kind = label,
        hashes = r.len(),
        bootstrap = take.len(),
        "built reference fingerprint"
    );
    Some(r)
}

fn match_to_segment(
    reference: &ReferenceFingerprint,
    window: &AnalyzedWindow,
    config: &Config,
) -> Option<Segment> {
    let m = matching::match_against_reference(reference, &window.fp, config)?;
    let (start, end) = boundary::refine(&window.region, m.start_seconds, m.end_seconds);
    if end - start < config.min_segment_seconds as f64 {
        return None;
    }
    Some(Segment {
        start,
        end,
        confidence: m.confidence,
    })
}

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
