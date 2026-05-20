//! Audio decoding: media file → mono f32 PCM at target sample rate.
//!
//! Key design decisions vs Plex:
//! - Uses symphonia (pure Rust) — no ffmpeg/transcoder dependency
//! - Streaming decode: never buffers the entire file in memory
//! - Only decodes the time windows we care about (first N / last N minutes)
//! - Automatic resampling via rubato (high-quality sinc interpolation)

use anyhow::{Context, Result};
use rubato::{Resampler, SincFixedIn, SincInterpolationParameters, SincInterpolationType, WindowFunction};
use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::DecoderOptions;
use symphonia::core::formats::{FormatOptions, SeekMode, SeekTo};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;
use symphonia::core::units::Time;
use std::fs::File;
use std::path::Path;

use crate::Config;

/// Decoded audio region ready for fingerprinting
#[derive(Debug)]
pub struct AudioRegion {
    /// Mono PCM samples at config.sample_rate
    pub samples: Vec<f32>,
    /// Sample rate of the output
    pub sample_rate: u32,
    /// Time offset in seconds from the start of the file
    pub offset_seconds: f64,
    /// Total duration of the source file in seconds (if known)
    pub total_duration: Option<f64>,
}

/// Decode the intro window (first N minutes) of a media file.
pub fn decode_intro_region(path: &Path, config: &Config) -> Result<AudioRegion> {
    decode_region_internal(path, config, RegionSpec::Intro)
}

/// Decode the credits window (last N minutes) of a media file.
///
/// Uses the same single open as the intro path: the duration is read from the
/// track header without a second file open.
pub fn decode_credits_region(path: &Path, config: &Config) -> Result<AudioRegion> {
    decode_region_internal(path, config, RegionSpec::Credits)
}

/// Decode a specific time region to mono f32 PCM at the configured sample rate.
pub fn decode_region(
    path: &Path,
    config: &Config,
    start_secs: f64,
    end_secs: f64,
) -> Result<AudioRegion> {
    decode_region_internal(path, config, RegionSpec::Absolute { start_secs, end_secs })
}

#[derive(Clone, Copy)]
enum RegionSpec {
    Intro,
    Credits,
    Absolute { start_secs: f64, end_secs: f64 },
}

fn decode_region_internal(
    path: &Path,
    config: &Config,
    spec: RegionSpec,
) -> Result<AudioRegion> {
    let file = File::open(path).context("Failed to open media file")?;
    let mss = MediaSourceStream::new(Box::new(file), Default::default());

    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }

    let probed = symphonia::default::get_probe()
        .format(&hint, mss, &FormatOptions::default(), &MetadataOptions::default())
        .context("Unsupported audio format")?;

    let mut format = probed.format;
    let track = format
        .tracks()
        .iter()
        .find(|t| t.codec_params.codec != symphonia::core::codecs::CODEC_TYPE_NULL)
        .context("No audio track found")?;

    let track_id = track.id;
    let native_rate = track.codec_params.sample_rate.unwrap_or(44100);
    let channels = track.codec_params.channels.map(|c| c.count()).unwrap_or(2);
    let total_duration = track
        .codec_params
        .n_frames
        .map(|n| n as f64 / native_rate as f64);

    let (start_secs, end_secs) = match spec {
        RegionSpec::Intro => (0.0, config.intro_scan_minutes as f64 * 60.0),
        RegionSpec::Credits => {
            let duration = total_duration.context("Unknown duration; cannot scan credits")?;
            let start = (duration - config.credits_scan_minutes as f64 * 60.0).max(0.0);
            (start, duration)
        }
        RegionSpec::Absolute { start_secs, end_secs } => (start_secs, end_secs),
    };

    let mut decoder = symphonia::default::get_codecs()
        .make(&track.codec_params, &DecoderOptions::default())
        .context("Failed to create decoder")?;

    if start_secs > 0.0 {
        format.seek(
            SeekMode::Coarse,
            SeekTo::Time {
                time: Time::new(start_secs as u64, start_secs.fract()),
                track_id: Some(track_id),
            },
        )?;
    }

    let max_samples = ((end_secs - start_secs) * native_rate as f64) as usize * channels;
    let mut raw_samples: Vec<f32> = Vec::with_capacity(max_samples);
    let mut sample_buf = None;

    loop {
        let packet = match format.next_packet() {
            Ok(p) => p,
            Err(symphonia::core::errors::Error::IoError(_)) => break, // EOF
            Err(e) => return Err(e.into()),
        };

        if packet.track_id() != track_id {
            continue;
        }

        let decoded = decoder.decode(&packet)?;
        let spec = *decoded.spec();

        if sample_buf.is_none() {
            sample_buf = Some(SampleBuffer::<f32>::new(decoded.capacity() as u64, spec));
        }

        let buf = sample_buf.as_mut().unwrap();
        buf.copy_interleaved_ref(decoded);

        raw_samples.extend_from_slice(buf.samples());

        if raw_samples.len() >= max_samples {
            raw_samples.truncate(max_samples);
            break;
        }
    }

    // Drop any trailing partial frame so downmix doesn't silently discard it.
    let frame_aligned_len = raw_samples.len() - (raw_samples.len() % channels);
    raw_samples.truncate(frame_aligned_len);

    let mono = downmix_to_mono(&raw_samples, channels);

    let samples = if native_rate != config.sample_rate {
        resample(&mono, native_rate, config.sample_rate)?
    } else {
        mono
    };

    Ok(AudioRegion {
        samples,
        sample_rate: config.sample_rate,
        offset_seconds: start_secs,
        total_duration,
    })
}

/// Downmix interleaved multi-channel audio to mono by averaging channels.
fn downmix_to_mono(interleaved: &[f32], channels: usize) -> Vec<f32> {
    if channels == 1 {
        return interleaved.to_vec();
    }

    let scale = 1.0 / channels as f32;
    interleaved
        .chunks_exact(channels)
        .map(|frame| frame.iter().sum::<f32>() * scale)
        .collect()
}

/// Resample mono audio using high-quality sinc interpolation.
///
/// `SincFixedIn` requires every call to be exactly `chunk_size` samples, so the
/// final partial input chunk is zero-padded. After resampling we trim the
/// output back to the expected length so the synthetic silence at the tail
/// doesn't leak into the fingerprint.
fn resample(input: &[f32], from_rate: u32, to_rate: u32) -> Result<Vec<f32>> {
    let params = SincInterpolationParameters {
        sinc_len: 64,
        f_cutoff: 0.95,
        interpolation: SincInterpolationType::Cubic,
        oversampling_factor: 128,
        window: WindowFunction::BlackmanHarris2,
    };

    let ratio = to_rate as f64 / from_rate as f64;
    let chunk_size = 1024;
    let mut resampler = SincFixedIn::<f32>::new(
        ratio,
        2.0,
        params,
        chunk_size,
        1, // mono
    )?;

    let expected_output_len = (input.len() as f64 * ratio).round() as usize;
    let mut output = Vec::with_capacity(expected_output_len + chunk_size);

    for chunk in input.chunks(chunk_size) {
        let mut padded = chunk.to_vec();
        if padded.len() < chunk_size {
            padded.resize(chunk_size, 0.0);
        }
        let result = resampler.process(&[padded], None)?;
        output.extend_from_slice(&result[0]);
    }

    output.truncate(expected_output_len);
    Ok(output)
}
