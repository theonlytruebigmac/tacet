//! Diagnose why match_against_reference returns None on real episodes.
//!
//! Usage: cargo run --release --example diagnose_match -- <ep1.mkv> <ep2.mkv> <ep3.mkv>
//!
//! Fingerprints the intros of the three episodes, builds a reference, then
//! reports detailed match statistics for each episode against the reference.

use std::collections::HashMap;
use std::path::PathBuf;

use tacet::audio;
use tacet::fingerprint::{self, Fingerprint};
use tacet::matching::{self, ReferenceFingerprint};
use tacet::Config;

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() < 3 {
        eprintln!("usage: diagnose_match <ep1> <ep2> <ep3> [more...]");
        std::process::exit(2);
    }

    let scan_min: f32 = std::env::var("INTRO_SCAN_MINUTES")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(10.0);
    let config = Config {
        intro_scan_minutes: scan_min,
        ..Config::default()
    };
    println!("config: intro_scan_minutes={} threshold={} min_seg={}s frame_dur={}s",
        scan_min,
        config.match_threshold,
        config.min_segment_seconds,
        config.hop_size as f64 / config.sample_rate as f64,
    );

    let mut fps: Vec<Fingerprint> = Vec::new();
    for (i, p) in args.iter().enumerate() {
        let path = PathBuf::from(p);
        println!("\n[{}] decoding {}", i, path.display());
        let region = audio::decode_intro_region(&path, &config)?;
        println!("    samples={} duration={:.1}s",
            region.samples.len(),
            region.samples.len() as f64 / region.sample_rate as f64,
        );
        let fp = fingerprint::fingerprint(&region, &config);
        println!("    hashes={} frame_dur={}s", fp.hashes.len(), fp.frame_duration);
        fps.push(fp);
    }

    let refs: Vec<&Fingerprint> = fps.iter().take(3).collect();
    let reference = matching::build_reference(&refs, &config)
        .expect("reference build returned None");
    println!("\nreference: hashes={} support={}", reference.len(), reference.support);

    for (i, fp) in fps.iter().enumerate() {
        println!("\n--- match episode {} ---", i);
        diagnose(&reference, fp, &config);
    }

    Ok(())
}

fn diagnose(reference: &ReferenceFingerprint, query: &Fingerprint, config: &Config) {
    let mut delta_to_frames: HashMap<i32, Vec<u32>> = HashMap::new();
    for h in &query.hashes {
        if let Some(&ref_frame) = reference.hash_to_frame.get(&h.hash) {
            let delta = h.frame as i32 - ref_frame as i32;
            delta_to_frames.entry(delta).or_default().push(h.frame);
        }
    }

    let total_hits: usize = delta_to_frames.values().map(|v| v.len()).sum();
    println!("    query hashes: {}, total hits in reference: {}", query.hashes.len(), total_hits);
    println!("    distinct deltas: {}", delta_to_frames.len());

    let mut sorted: Vec<_> = delta_to_frames.iter().collect();
    sorted.sort_by_key(|(_, v)| std::cmp::Reverse(v.len()));
    println!("    top 5 deltas:");
    for (delta, frames) in sorted.iter().take(5) {
        let confidence = frames.len() as f64 / query.hashes.len() as f64;
        let frame_min = *frames.iter().min().unwrap();
        let frame_max = *frames.iter().max().unwrap();
        let sec_min = query.frame_to_seconds(frame_min);
        let sec_max = query.frame_to_seconds(frame_max);
        println!("      delta={:6}  votes={:5}  conf={:.4}  span={:.1}s..{:.1}s",
            delta, frames.len(), confidence, sec_min, sec_max);
    }

    let m = matching::match_against_reference(reference, query, config);
    match m {
        Some(r) => println!(
            "    MATCH: {:.1}s..{:.1}s conf={:.2} offset_frames={}",
            r.start_seconds, r.end_seconds, r.confidence, r.offset_frames
        ),
        None => println!("    NO MATCH"),
    }
}
