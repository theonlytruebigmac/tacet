//! End-to-end test of the fingerprint → reference → match → refine pipeline.
//!
//! No real media files are involved: we synthesize three "episodes" that share
//! a known "intro" chirp and differ in the body, then assert the matcher locates
//! the intro and the boundary refiner snaps the end near the chirp's offset.

use tacet::audio::AudioRegion;
use tacet::detection::{detect_season, EpisodeFile, Season};
use tacet::fingerprint;
use tacet::matching;
use tacet::storage::{FingerprintKind, Store};
use tacet::{Config, detection};

use std::f32::consts::TAU;

fn synth_region(seed: u32, intro_seconds: f32, body_seconds: f32, sample_rate: u32) -> AudioRegion {
    let total = ((intro_seconds + body_seconds) * sample_rate as f32) as usize;
    let intro_len = (intro_seconds * sample_rate as f32) as usize;
    let mut samples = Vec::with_capacity(total);

    // Identical "intro" content for every episode: a multi-tone chord whose
    // harmonics give the constellation hasher something to lock onto.
    for i in 0..intro_len {
        let t = i as f32 / sample_rate as f32;
        let s = (TAU * 440.0 * t).sin() * 0.4
            + (TAU * 660.0 * t).sin() * 0.25
            + (TAU * 990.0 * t).sin() * 0.15;
        samples.push(s);
    }

    // Per-episode "body": random-ish noise driven by seed so no two episodes
    // share body content. We use a deterministic PRNG so tests are reproducible.
    let mut state = seed.wrapping_mul(2654435761);
    for _ in 0..(total - intro_len) {
        state = state.wrapping_mul(1664525).wrapping_add(1013904223);
        let n = ((state >> 8) as f32 / u32::MAX as f32 - 0.5) * 0.8;
        samples.push(n);
    }

    AudioRegion {
        samples,
        sample_rate,
        offset_seconds: 0.0,
        total_duration: Some((total as f32 / sample_rate as f32) as f64),
    }
}

#[test]
fn matcher_finds_shared_intro_across_synthetic_episodes() {
    let config = Config {
        match_threshold: 0.02,
        min_segment_seconds: 1.0,
        ..Config::default()
    };

    let regions: Vec<AudioRegion> = (0..3)
        .map(|seed| synth_region(seed, 6.0, 8.0, config.sample_rate))
        .collect();
    let fps: Vec<_> = regions
        .iter()
        .map(|r| fingerprint::fingerprint(r, &config))
        .collect();

    let reference =
        matching::build_reference(&fps.iter().collect::<Vec<_>>(), &config).expect("reference");

    // The reference should contain a meaningful fraction of the input hashes.
    assert!(reference.len() > 50, "reference too sparse: {}", reference.len());

    for (i, fp) in fps.iter().enumerate() {
        let m = matching::match_against_reference(&reference, fp, &config)
            .unwrap_or_else(|| panic!("episode {i}: no match found"));
        assert!(
            m.start_seconds < 1.5,
            "episode {i}: start {:.2}s should be near 0",
            m.start_seconds
        );
        assert!(
            m.end_seconds > 4.0 && m.end_seconds < 8.0,
            "episode {i}: end {:.2}s should be near the intro/body boundary (~6s)",
            m.end_seconds
        );
    }
}

#[test]
fn detect_and_persist_writes_markers_and_references() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();

    // Three short fake media files would require encoders we don't have, so we
    // exercise the persistence path by fingerprinting synthetic regions
    // directly and writing them through the store, then assert reads back.
    let config = Config::default();
    let regions: Vec<AudioRegion> = (0..3)
        .map(|seed| synth_region(seed, 6.0, 8.0, config.sample_rate))
        .collect();
    let fps: Vec<_> = regions
        .iter()
        .map(|r| fingerprint::fingerprint(r, &config))
        .collect();

    let reference = matching::build_reference(&fps.iter().collect::<Vec<_>>(), &config).unwrap();
    store
        .save_references("synthetic", 1, FingerprintKind::Intro, std::slice::from_ref(&reference))
        .unwrap();
    for (i, fp) in fps.iter().enumerate() {
        store
            .save_episode_fingerprint("synthetic", 1, (i + 1) as u32, FingerprintKind::Intro, fp)
            .unwrap();
    }

    let loaded_refs = store
        .load_references("synthetic", 1, FingerprintKind::Intro)
        .unwrap();
    assert_eq!(loaded_refs.len(), 1);
    assert_eq!(loaded_refs[0].len(), reference.len());

    let loaded_fp = store
        .load_episode_fingerprint("synthetic", 1, 1, FingerprintKind::Intro)
        .unwrap()
        .expect("episode fingerprint should be present");
    assert_eq!(loaded_fp.len(), fps[0].len());
}

#[test]
fn empty_season_yields_empty_result() {
    let season = Season {
        series_id: "nothing".to_string(),
        season_number: 1,
        episodes: vec![],
    };
    let result = detect_season(&season, &Config::default()).unwrap();
    assert!(result.markers.is_empty());
    assert!(result.intro_references.is_empty());
    assert!(result.credits_references.is_empty());
}

#[test]
fn missing_files_do_not_panic_just_warn() {
    // detect_and_persist over a "season" whose files don't exist should
    // surface as a normal error, not a panic.
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let season = Season {
        series_id: "ghost".to_string(),
        season_number: 1,
        episodes: vec![EpisodeFile {
            id: "ghost-s01e01".to_string(),
            path: tmp.path().join("does-not-exist.mkv"),
            episode_number: 1,
        }],
    };
    // analyze_episode tolerates failed decodes (returns None windows), so
    // detect_and_persist should succeed with empty markers, not error.
    let result = detection::detect_and_persist(&season, &store, &Config::default()).unwrap();
    assert_eq!(result.markers.len(), 1);
    assert!(result.markers[0].intro.is_none());
    assert!(result.markers[0].credits.is_none());
}
