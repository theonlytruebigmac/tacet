//! Reference-based fingerprint matching.
//!
//! Instead of Plex's O(n²) pairwise comparison, we bootstrap a *reference*
//! fingerprint from the first few episodes, then every other episode is a
//! single O(n) lookup against that reference.
//!
//! The match works via offset-histogram alignment (Shazam-style): for each
//! hash shared between reference and query, accumulate `query_frame -
//! reference_frame`. The mode of that distribution is the true alignment;
//! anything else is noise.

use std::collections::HashMap;

use crate::fingerprint::{FpHash, Fingerprint};
use crate::Config;

/// Compact reference fingerprint: each hash maps to its frame in the canonical timeline.
///
/// Plain `HashMap` because we only store hashes that survived bootstrap, so collisions
/// are not expected. If a hash recurs at multiple anchor frames we keep the *earliest*
/// occurrence deterministically (see `intersect_bootstrap`).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ReferenceFingerprint {
    pub hash_to_frame: HashMap<u32, u32>,
    pub frame_duration: f64,
    /// Number of source fingerprints that contributed to this reference.
    pub support: usize,
}

impl ReferenceFingerprint {
    pub fn is_empty(&self) -> bool {
        self.hash_to_frame.is_empty()
    }

    pub fn len(&self) -> usize {
        self.hash_to_frame.len()
    }
}

/// A match between a reference and a query fingerprint.
#[derive(Debug, Clone)]
pub struct MatchResult {
    /// Start time (seconds) of the matched span in the *query* audio.
    pub start_seconds: f64,
    /// End time (seconds) of the matched span in the *query* audio.
    pub end_seconds: f64,
    /// Fraction of query hashes that agreed with the dominant alignment.
    pub confidence: f64,
    /// Frame offset (query - reference) of the dominant alignment.
    pub offset_frames: i32,
}

/// Match a query fingerprint against a reference.
///
/// Returns `None` if no alignment crosses `config.match_threshold` or the
/// resulting span is shorter than `config.min_segment_seconds`.
///
/// Confidence is the fraction of *reference* hashes that voted at the
/// dominant alignment. This is independent of how long the query scan window
/// is — matching a 90 s intro inside a 10 min window can still hit ~100 %
/// because the denominator is the intro-only reference, not the whole window.
///
/// The matched span is the *longest contiguous run* of votes at the dominant
/// delta (within `MAX_GAP_FRAMES`). Taking the global min/max would let one
/// stray late vote stretch the reported intro by tens of seconds.
pub fn match_against_reference(
    reference: &ReferenceFingerprint,
    query: &Fingerprint,
    config: &Config,
) -> Option<MatchResult> {
    if reference.is_empty() || query.is_empty() {
        return None;
    }

    // Per delta: which reference hashes voted (set; each ref hash counts once)
    // and which query frames voted (vec; used for longest-run span).
    let mut delta_to_ref_hashes: HashMap<i32, std::collections::HashSet<u32>> = HashMap::new();
    let mut delta_to_frames: HashMap<i32, Vec<u32>> = HashMap::new();

    for h in &query.hashes {
        if let Some(&ref_frame) = reference.hash_to_frame.get(&h.hash) {
            let delta = h.frame as i32 - ref_frame as i32;
            delta_to_ref_hashes.entry(delta).or_default().insert(h.hash);
            delta_to_frames.entry(delta).or_default().push(h.frame);
        }
    }

    let (best_delta, ref_hashes_hit) = delta_to_ref_hashes
        .iter()
        .max_by_key(|(_, set)| set.len())
        .map(|(d, set)| (*d, set.len()))?;

    let confidence = ref_hashes_hit as f64 / reference.len() as f64;
    if confidence < config.match_threshold {
        return None;
    }

    let mut best_frames = delta_to_frames.remove(&best_delta)?;
    best_frames.sort_unstable();
    let (run_start, run_end) = longest_run(&best_frames, MAX_GAP_FRAMES);
    let start = query.frame_to_seconds(run_start);
    let end = query.frame_to_seconds(run_end);

    if end - start < config.min_segment_seconds as f64 {
        return None;
    }

    Some(MatchResult {
        start_seconds: start,
        end_seconds: end,
        confidence: confidence.min(1.0),
        offset_frames: best_delta,
    })
}

/// Match a query against several references and return the best match, if any.
///
/// Used when a season has multiple reference fingerprints — e.g., an anime with
/// an OP/ED swap mid-season produces two clusters of episodes with different
/// intro audio. Each cluster gets its own reference; per-episode matching tries
/// all references and keeps the one with the highest confidence.
pub fn match_against_references(
    references: &[ReferenceFingerprint],
    query: &Fingerprint,
    config: &Config,
) -> Option<MatchResult> {
    references
        .iter()
        .filter_map(|r| match_against_reference(r, query, config))
        .max_by(|a, b| {
            a.confidence
                .partial_cmp(&b.confidence)
                .unwrap_or(std::cmp::Ordering::Equal)
        })
}

/// Max frame gap (≈1.3s at 16 kHz/2048 hop) considered "still inside the run".
const MAX_GAP_FRAMES: u32 = 10;

fn longest_run(frames: &[u32], max_gap: u32) -> (u32, u32) {
    debug_assert!(!frames.is_empty(), "longest_run requires at least one frame");
    let mut best = (frames[0], frames[0], 1usize);
    let mut current_start = frames[0];
    let mut current_count = 1usize;

    for w in frames.windows(2) {
        let (prev, curr) = (w[0], w[1]);
        if curr - prev <= max_gap {
            current_count += 1;
        } else {
            current_start = curr;
            current_count = 1;
        }
        if current_count > best.2 {
            best = (current_start, curr, current_count);
        }
    }

    (best.0, best.1)
}

/// Build a reference fingerprint by intersecting hashes across the bootstrap set.
///
/// Algorithm:
/// 1. Use the first fingerprint as the canonical timeline.
/// 2. For each other fingerprint, find the dominant `frame - anchor_frame` delta.
/// 3. A hash "survives" if it appears (at the agreed delta) in at least
///    `ceil(N/2)` of the inputs.
///
/// With a single input we fall back to using it verbatim — useful for
/// incremental detection when only one episode is available.
pub fn build_reference(
    fingerprints: &[&Fingerprint],
    _config: &Config,
) -> Option<ReferenceFingerprint> {
    match fingerprints.len() {
        0 => None,
        1 => Some(ReferenceFingerprint {
            hash_to_frame: fingerprints[0]
                .hashes
                .iter()
                .map(|h| (h.hash, h.frame))
                .collect(),
            frame_duration: fingerprints[0].frame_duration,
            support: 1,
        }),
        _ => Some(intersect_bootstrap(fingerprints)),
    }
}

fn intersect_bootstrap(fingerprints: &[&Fingerprint]) -> ReferenceFingerprint {
    let anchor = fingerprints[0];
    let anchor_index: HashMap<u32, Vec<u32>> = group_by_hash(&anchor.hashes);

    // (hash, anchor_frame) -> number of bootstrap fingerprints that confirmed it.
    let mut confirmations: HashMap<(u32, u32), usize> = HashMap::new();
    for h in &anchor.hashes {
        confirmations.insert((h.hash, h.frame), 1);
    }

    for fp in &fingerprints[1..] {
        let Some(best_delta) = dominant_delta(fp, &anchor_index) else {
            continue;
        };

        for h in &fp.hashes {
            let Some(anchor_frames) = anchor_index.get(&h.hash) else {
                continue;
            };
            for &af in anchor_frames {
                if h.frame as i32 - af as i32 == best_delta {
                    *confirmations.entry((h.hash, af)).or_insert(0) += 1;
                }
            }
        }
    }

    let min_support = fingerprints.len().div_ceil(2);
    let mut hash_to_frame: HashMap<u32, u32> = HashMap::new();
    for ((hash, frame), count) in confirmations {
        if count < min_support {
            continue;
        }
        // If a hash appears at multiple anchor frames, keep the earliest one
        // so the choice is deterministic regardless of HashMap iteration order.
        hash_to_frame
            .entry(hash)
            .and_modify(|f| {
                if frame < *f {
                    *f = frame;
                }
            })
            .or_insert(frame);
    }

    ReferenceFingerprint {
        hash_to_frame,
        frame_duration: anchor.frame_duration,
        support: fingerprints.len(),
    }
}

fn dominant_delta(fp: &Fingerprint, anchor_index: &HashMap<u32, Vec<u32>>) -> Option<i32> {
    let mut deltas: HashMap<i32, usize> = HashMap::new();
    for h in &fp.hashes {
        if let Some(anchor_frames) = anchor_index.get(&h.hash) {
            for &af in anchor_frames {
                *deltas.entry(h.frame as i32 - af as i32).or_insert(0) += 1;
            }
        }
    }
    deltas.into_iter().max_by_key(|(_, c)| *c).map(|(d, _)| d)
}

fn group_by_hash(hashes: &[FpHash]) -> HashMap<u32, Vec<u32>> {
    let mut out: HashMap<u32, Vec<u32>> = HashMap::with_capacity(hashes.len());
    for h in hashes {
        out.entry(h.hash).or_default().push(h.frame);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fp_with(hashes: Vec<(u32, u32)>) -> Fingerprint {
        Fingerprint {
            hashes: hashes
                .into_iter()
                .map(|(hash, frame)| FpHash { hash, frame })
                .collect(),
            time_offset: 0.0,
            frame_duration: 0.1,
        }
    }

    #[test]
    fn single_fingerprint_passes_through() {
        let fp = fp_with(vec![(1, 0), (2, 5), (3, 10)]);
        let r = build_reference(&[&fp], &Config::default()).unwrap();
        assert_eq!(r.len(), 3);
        assert_eq!(r.support, 1);
    }

    #[test]
    fn intersection_keeps_shared_hashes() {
        let a = fp_with(vec![(10, 0), (20, 5), (30, 10)]);
        let b = fp_with(vec![(10, 2), (20, 7), (99, 8)]); // shifted by +2, plus noise
        let c = fp_with(vec![(10, 4), (20, 9), (88, 1)]); // shifted by +4, plus noise

        let r = build_reference(&[&a, &b, &c], &Config::default()).unwrap();
        // 10 and 20 are confirmed by all three; 30 only by anchor.
        assert!(r.hash_to_frame.contains_key(&10));
        assert!(r.hash_to_frame.contains_key(&20));
        assert!(!r.hash_to_frame.contains_key(&99));
        assert!(!r.hash_to_frame.contains_key(&88));
    }

    fn lenient_config() -> Config {
        Config {
            match_threshold: 0.5,
            min_segment_seconds: 0.5,
            ..Config::default()
        }
    }

    #[test]
    fn match_finds_offset_and_span() {
        let reference = ReferenceFingerprint {
            hash_to_frame: [(10, 0), (20, 5), (30, 10), (40, 15)]
                .into_iter()
                .collect(),
            frame_duration: 0.1,
            support: 3,
        };
        let query = fp_with(vec![(10, 100), (20, 105), (30, 110), (40, 115), (99, 200)]);
        let config = lenient_config();

        let m = match_against_reference(&reference, &query, &config).unwrap();
        assert_eq!(m.offset_frames, 100);
        assert!((m.start_seconds - 10.0).abs() < 1e-9);
        assert!((m.end_seconds - 11.5).abs() < 1e-9);
        assert!(m.confidence >= 0.8);
    }

    #[test]
    fn match_ignores_stray_votes_far_from_run() {
        // Reference and query share a contiguous span at offset +100, plus one
        // stray hash at frame 900 that also happens to land on the same delta.
        // Without longest-run filtering the reported end would be ~90s instead
        // of the real ~1.5s.
        let reference = ReferenceFingerprint {
            hash_to_frame: [(10, 0), (20, 5), (30, 10), (40, 15), (50, 800)]
                .into_iter()
                .collect(),
            frame_duration: 0.1,
            support: 3,
        };
        let query = fp_with(vec![
            (10, 100),
            (20, 105),
            (30, 110),
            (40, 115),
            (50, 900), // delta = +100, but 785 frames away from the rest
        ]);
        let config = lenient_config();

        let m = match_against_reference(&reference, &query, &config).unwrap();
        assert_eq!(m.offset_frames, 100);
        assert!(m.end_seconds < 20.0, "end={} should be near the contiguous run", m.end_seconds);
    }

    #[test]
    fn intersect_is_deterministic_under_repeated_anchor_frames() {
        // Same hash at multiple anchor frames — earliest should always win.
        let a = fp_with(vec![(7, 50), (7, 5), (7, 20)]);
        let b = fp_with(vec![(7, 50), (7, 5), (7, 20)]);
        let c = fp_with(vec![(7, 50), (7, 5), (7, 20)]);

        let r = build_reference(&[&a, &b, &c], &Config::default()).unwrap();
        assert_eq!(r.hash_to_frame.get(&7).copied(), Some(5));
    }
}
