//! Subprocess-based audio decoding via ffmpeg.
//!
//! Used as a fallback when symphonia can't handle a file's codec (HE-AAC,
//! E-AC3/Atmos, PCM-in-Matroska, etc.). Spawns `ffmpeg` with stream args that
//! pipe mono f32le PCM at the target sample rate, then reads it into a Vec.
//!
//! Total duration is read separately via `ffprobe` — much cheaper than
//! decoding the whole file just to learn how long it is.

use anyhow::{anyhow, Context, Result};
use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::thread;

use crate::audio::AudioRegion;
use crate::Config;

/// Cache the result of "is ffmpeg on PATH?" — checked once per process.
static FFMPEG_AVAILABLE: OnceLock<bool> = OnceLock::new();
static FFPROBE_AVAILABLE: OnceLock<bool> = OnceLock::new();

/// Whether `ffmpeg` is callable via PATH.
pub fn is_available() -> bool {
    *FFMPEG_AVAILABLE.get_or_init(|| binary_exists("ffmpeg"))
}

fn ffprobe_available() -> bool {
    *FFPROBE_AVAILABLE.get_or_init(|| binary_exists("ffprobe"))
}

fn binary_exists(name: &str) -> bool {
    Command::new(name)
        .arg("-version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Decode `[start_secs, end_secs)` of `path` to mono f32 PCM at `config.sample_rate`
/// via ffmpeg subprocess. `end_secs` may be `None` to read to EOF.
pub fn decode_region(
    path: &Path,
    config: &Config,
    start_secs: f64,
    end_secs: Option<f64>,
) -> Result<AudioRegion> {
    if !is_available() {
        return Err(anyhow!("ffmpeg not found on PATH"));
    }

    let total_duration = probe_duration(path).ok();

    let mut cmd = Command::new("ffmpeg");
    cmd.arg("-nostdin")
        .arg("-loglevel")
        .arg("error")
        .arg("-ss")
        .arg(format!("{start_secs}"));
    if let Some(end) = end_secs {
        // Duration of output, computed in input timeline (post-`-ss`).
        let duration = (end - start_secs).max(0.0);
        cmd.arg("-t").arg(format!("{duration}"));
    }
    cmd.arg("-i")
        .arg(path)
        .arg("-vn")
        .arg("-map")
        .arg("a:0")
        .arg("-ac")
        .arg("1")
        .arg("-ar")
        .arg(config.sample_rate.to_string())
        .arg("-f")
        .arg("f32le")
        .arg("pipe:1")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = cmd.spawn().context("spawning ffmpeg")?;
    let mut stdout = child.stdout.take().expect("piped");
    let stderr = child.stderr.take().expect("piped");

    // Drain stderr on a separate thread so a chatty ffmpeg doesn't deadlock by
    // filling the pipe buffer while we read stdout. Without this, decoding any
    // file that emits warnings to stderr (HE-AAC, noisy containers) will hang.
    let stderr_handle = thread::spawn(move || {
        let mut buf = String::new();
        let mut reader = stderr;
        let _ = reader.read_to_string(&mut buf);
        buf
    });

    let mut bytes = Vec::with_capacity(1 << 20);
    stdout.read_to_end(&mut bytes).context("reading ffmpeg stdout")?;
    let status = child.wait().context("waiting on ffmpeg")?;
    let stderr_buf = stderr_handle.join().unwrap_or_default();

    if !status.success() {
        return Err(anyhow!(
            "ffmpeg exited with {}: {}",
            status,
            stderr_buf.trim().lines().last().unwrap_or("(no stderr)")
        ));
    }

    // f32le → Vec<f32>. ffmpeg outputs little-endian which matches every CPU we run on.
    let samples = bytes
        .chunks_exact(4)
        .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        .collect::<Vec<_>>();

    Ok(AudioRegion {
        samples,
        sample_rate: config.sample_rate,
        offset_seconds: start_secs,
        total_duration,
    })
}

/// Probe just the file duration in seconds via `ffprobe`. Cheap — reads only
/// the container header.
pub fn probe_duration(path: &Path) -> Result<f64> {
    if !ffprobe_available() {
        return Err(anyhow!("ffprobe not found on PATH"));
    }
    let output = Command::new("ffprobe")
        .arg("-v")
        .arg("error")
        .arg("-show_entries")
        .arg("format=duration")
        .arg("-of")
        .arg("csv=p=0")
        .arg(path)
        .output()
        .context("running ffprobe")?;
    if !output.status.success() {
        return Err(anyhow!(
            "ffprobe failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let s = String::from_utf8_lossy(&output.stdout);
    let trimmed = s.trim();
    let duration: f64 = trimmed
        .parse()
        .with_context(|| format!("parsing ffprobe duration {:?}", trimmed))?;
    Ok(duration)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binary_exists_detects_a_real_tool() {
        // `sh` is on every Unix; we just want to verify the probe works.
        #[cfg(unix)]
        assert!(binary_exists("sh"));
    }

    #[test]
    fn binary_exists_returns_false_for_nonsense() {
        assert!(!binary_exists("definitely-not-a-real-binary-xyz123"));
    }
}
