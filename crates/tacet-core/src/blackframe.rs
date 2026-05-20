//! Black-frame detection for end-credits boundary refinement.
//!
//! Audio fingerprinting works great for shows whose end-credits share a song
//! across episodes (anime ED themes). It does *not* work for shows whose
//! credits are scored differently per episode (most live-action streaming
//! series) — there's nothing repeated across episodes to fingerprint.
//!
//! For those shows, the credits transition is reliably marked by a *long
//! fade-to-black* into the credits roll. This module wraps ffmpeg's
//! `blackdetect` filter and reports the earliest long black segment in the
//! credits scan window. The caller treats that as `credits_start`; the segment
//! end is the file end.

use anyhow::{anyhow, Context, Result};
use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use crate::audio::ffmpeg as ffmpeg_helper;

/// One black region as reported by ffmpeg.
#[derive(Debug, Clone, Copy)]
pub struct BlackSegment {
    pub start: f64,
    pub end: f64,
    pub duration: f64,
}

/// Bag of knobs for a blackframe scan. Kept as a struct so the signature
/// doesn't grow positional-arg by positional-arg every time we add a flag.
#[derive(Debug, Clone)]
pub struct ScanOptions {
    pub scan_seconds: f64,
    pub min_black_seconds: f64,
    pub pix_threshold: f64,
    pub sample_fps: f64,
    /// Optional ffmpeg `-hwaccel` value (e.g. "auto", "cuda"). `None` forces
    /// software decode; on broken systems set this to `None` if `"auto"` hangs.
    pub hwaccel: Option<String>,
    /// Wall-clock deadline for the ffmpeg invocation. On timeout we kill the
    /// child and return an error so the caller can fall back to software.
    pub timeout: Duration,
}

/// Scan the last `opts.scan_seconds` of `path` for black frames using ffmpeg.
///
/// Returns segments in chronological order, with absolute timestamps relative
/// to the file's start (not the seek offset).
pub fn scan_tail(
    path: &Path,
    total_duration: f64,
    opts: &ScanOptions,
) -> Result<Vec<BlackSegment>> {
    if !ffmpeg_helper::is_available() {
        return Err(anyhow!("ffmpeg not found on PATH"));
    }
    let start_secs = (total_duration - opts.scan_seconds).max(0.0);

    // Sub-sample the video (`fps=N`) and downscale aggressively (160x90)
    // before blackdetect. Credits transitions are seconds long, so 2 fps is
    // plenty to find them and ~12x cheaper than decoding at full rate.
    let vf = format!(
        "fps={fps},scale=160:90,blackdetect=d={d}:pix_th={pix}",
        fps = opts.sample_fps,
        d = opts.min_black_seconds,
        pix = opts.pix_threshold,
    );

    let mut cmd = Command::new("ffmpeg");
    cmd.arg("-nostdin").arg("-loglevel").arg("info");
    if let Some(h) = &opts.hwaccel {
        // `-hwaccel <name>` must precede `-i`. ffmpeg's `auto` value falls
        // back to software automatically if no accelerator initialises.
        cmd.arg("-hwaccel").arg(h);
    }
    let mut child = cmd
        .arg("-ss")
        .arg(format!("{start_secs}"))
        .arg("-i")
        .arg(path)
        .arg("-vf")
        .arg(&vf)
        .arg("-an")
        .arg("-sn")
        .arg("-f")
        .arg("null")
        .arg("-")
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .context("spawning ffmpeg for blackdetect")?;

    let stderr = child.stderr.take().expect("piped");
    let stderr_handle = thread::spawn(move || {
        let mut buf = String::new();
        let mut reader = stderr;
        let _ = reader.read_to_string(&mut buf);
        buf
    });

    // Wall-clock watchdog. Some hwaccels (notably broken VAAPI on this
    // machine) silently hang for minutes; without a timeout, one bad file
    // would block the whole detection pass.
    let start = Instant::now();
    let status = loop {
        match child.try_wait().context("polling ffmpeg blackdetect")? {
            Some(s) => break s,
            None => {
                if start.elapsed() >= opts.timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    let _ = stderr_handle.join();
                    return Err(anyhow!(
                        "ffmpeg blackdetect exceeded {:?} timeout; killed",
                        opts.timeout,
                    ));
                }
                thread::sleep(Duration::from_millis(50));
            }
        }
    };
    let log = stderr_handle.join().unwrap_or_default();

    if !status.success() {
        return Err(anyhow!(
            "ffmpeg blackdetect exited with {}: {}",
            status,
            log.lines().last().unwrap_or("(no stderr)")
        ));
    }

    Ok(parse_blackdetect(&log, start_secs))
}

/// Parse blackdetect log lines like:
///   [blackdetect @ ...] black_start:141.706 black_end:168.232 black_duration:26.526
fn parse_blackdetect(log: &str, seek_offset: f64) -> Vec<BlackSegment> {
    let mut out = Vec::new();
    for line in log.lines() {
        let Some(idx) = line.find("black_start:") else {
            continue;
        };
        let rest = &line[idx..];
        let start = extract_field(rest, "black_start:");
        let end = extract_field(rest, "black_end:");
        let duration = extract_field(rest, "black_duration:");
        if let (Some(s), Some(e), Some(d)) = (start, end, duration) {
            out.push(BlackSegment {
                start: s + seek_offset,
                end: e + seek_offset,
                duration: d,
            });
        }
    }
    out
}

fn extract_field(s: &str, key: &str) -> Option<f64> {
    let idx = s.find(key)?;
    let after = &s[idx + key.len()..];
    let end = after.find(|c: char| c != '.' && !c.is_ascii_digit()).unwrap_or(after.len());
    after[..end].parse().ok()
}

/// Pick the credits-start time from a tail-scan worth of black segments.
///
/// Heuristic: the credits roll opens with the *longest* black fade in the
/// scan window, typically a continuous 5-30s of dark frames before the text
/// fades in. We pick the longest segment that's at least
/// `min_significant_seconds` long. If none qualify, return `None`.
pub fn pick_credits_start(
    segments: &[BlackSegment],
    min_significant_seconds: f64,
) -> Option<f64> {
    segments
        .iter()
        .filter(|s| s.duration >= min_significant_seconds)
        .max_by(|a, b| a.duration.partial_cmp(&b.duration).unwrap_or(std::cmp::Ordering::Equal))
        .map(|s| s.start)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_handles_real_blackdetect_output() {
        let log = "\
frame= 2964 fps=1482 q=-0.0 size=N/A
[Parsed_blackdetect_0 @ 0x7f] black_start:134.031 black_end:135.074 black_duration:1.043
[Parsed_blackdetect_0 @ 0x7f] black_start:141.706 black_end:168.232 black_duration:26.526
";
        let out = parse_blackdetect(log, 2400.0);
        assert_eq!(out.len(), 2);
        assert!((out[0].start - 2534.031).abs() < 1e-3);
        assert!((out[1].duration - 26.526).abs() < 1e-3);
        assert!((out[1].start - 2541.706).abs() < 1e-3);
    }

    #[test]
    fn pick_credits_start_returns_longest_above_threshold() {
        let segs = vec![
            BlackSegment { start: 100.0, end: 101.0, duration: 1.0 },
            BlackSegment { start: 200.0, end: 226.5, duration: 26.5 },
            BlackSegment { start: 250.0, end: 252.0, duration: 2.0 },
        ];
        assert_eq!(pick_credits_start(&segs, 3.0), Some(200.0));
        // None qualify when the threshold is too high.
        assert_eq!(pick_credits_start(&segs, 30.0), None);
    }

    #[test]
    fn pick_credits_start_ignores_short_segments() {
        let segs = vec![
            BlackSegment { start: 100.0, end: 100.5, duration: 0.5 },
            BlackSegment { start: 200.0, end: 200.5, duration: 0.5 },
        ];
        assert_eq!(pick_credits_start(&segs, 3.0), None);
    }
}
