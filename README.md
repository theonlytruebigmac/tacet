# Tacet

Ultra-fast audio fingerprinting + blackframe credits detection for TV shows, anime, and movies. Built in Rust for maximum throughput; pure-Rust on the fast path (symphonia), with ffmpeg as an automatic fallback for codecs symphonia rejects.

## Why Tacet over Plex's built-in detection?

||Plex|Tacet|
|---|---|---|
|**Intro matching**|O(n²) pairwise per season|O(n) reference-based, parallel|
|**24-episode season**|276 comparisons|3-episode bootstrap + 21 lookups|
|**Audio decoding**|Requires ffmpeg/transcoder enabled|Symphonia (pure Rust) + ffmpeg fallback|
|**Minimum intro length**|20 seconds|5 seconds (configurable)|
|**OP/ED change mid-season**|May re-analyze entire season|Adaptive multi-anchor bootstrap|
|**Credits detection**|Separate cloud-dependent system|Unified local pipeline (audio + blackframe hybrid)|
|**HE-AAC / E-AC3 / PCM-MKV**|n/a|Auto-fallback to ffmpeg subprocess|
|**Parallelism**|Single-threaded analysis|Rayon work-stealing across all cores|
|**Memory**|Decodes full audio track|Streams only the scan window|

Measured on the test corpus in this repo: **8 seconds** for a 15-episode anime season (FLAC), **18-23 seconds** for a 10-episode live-action season (HE-AAC + E-AC3 Atmos). Comparable Plex/Jellyfin runs are minutes to hours per season.

## Architecture

```text
┌─────────────────────────────────────────────────────────────┐
│                        CLI / HTTP API                        │
├──────────────┬──────────────────────────────┬────────────────┤
│   Detection  │         Matching             │    Storage     │
│  Orchestrator│  (Adaptive multi-anchor      │  (Optional;    │
│              │   bootstrap + best-match)    │   SQLite +     │
├──────────────┼──────────────────────────────┤   bincode)     │
│ Fingerprint  │  Constellation Map Builder   │                │
│    Engine    │  (Peak Pairing + Hashing)    │                │
├──────────────┼──────────────────────────────┤                │
│    Audio     │  Peak Extraction             │                │
│   Decoder    │  (Band-partitioned picking)  │                │
├──────────────┼──────────────────────────────┤                │
│              │  Boundary Refinement         │                │
│              │  (RMS energy snap)           │                │
├──────────────┼──────────────────────────────┘                │
│  symphonia   │  rustfft (STFT)                               │
│  rubato      │  (auto-selects AVX2/NEON)                     │
│  ↓ on error  │  ffmpeg blackdetect (credits fallback for     │
│   ffmpeg     │   shows without shared ED audio)              │
└──────────────┴───────────────────────────────────────────────┘
```

See [docs/tacet_architecture_pipeline.svg](docs/tacet_architecture_pipeline.svg) for the data-flow diagram.

## Repo layout

```text
tacet/
├── Cargo.toml                 # workspace manifest
└── crates/
    ├── tacet-core/            # library (depend on this from your media server)
    ├── tacet-api/             # axum HTTP service
    └── tacet-cli/             # `tacet` binary (scan/detect/serve/bench)
```

## CLI usage

### Scan a season

```bash
tacet scan --dir /media/tv/breaking-bad/season-01/ \
           --series breaking-bad \
           --season 1

# Output:
#   breaking-bad-s01e01 → intro: 2.3s–63.8s (94%) | credits: 2812.0s–2855.2s (87%)
#   breaking-bad-s01e02 → intro: 0.0s–61.5s (96%) | credits: 2798.1s–2843.0s (91%)
#   ...
#   Processed 7 episodes in 4.2s (600ms/episode)
```

### Detect a single new episode

```bash
tacet detect /media/tv/breaking-bad/season-01/episode-08.mkv \
             --series breaking-bad --season 1
```

### Override the scan windows

```bash
tacet scan --dir /media/tv/breaking-bad/season-01/ \
           --series breaking-bad --season 1 \
           --intro-scan-window 6.0 \
           --credits-scan-window 10.0
```

### Run as a service

```bash
tacet serve --listen 0.0.0.0:9320

# Then from your media server:
# POST /api/v1/detect {"episode_id": "...", "file_path": "...", "series": "...", "season": 1}
#   → {"markers": {...}, "reference_available": true}
#     reference_available=false means the season has not been bootstrapped yet —
#     run `tacet scan` (or equivalent) on at least 3 episodes first.
# GET  /api/v1/series/breaking-bad/season/1/markers
```

### Benchmark

```bash
tacet bench /path/to/any/media/file.mkv
#   Decode:       142.3ms (9600000 samples)
#   Fingerprint:   38.7ms (24531 hashes)
#   Total:        181.0ms
```

## Library usage

`tacet-core` is the engine. Default-features-off pulls in only the decoder + fingerprint + matching + blackframe modules — no CLI, no axum, no SQLite.

```toml
[dependencies]
tacet-core = { git = "https://github.com/theonlytruebigmac/tacet", default-features = false }
# Add the `store` feature only if you want the bundled SQLite persistence.
# tacet-core = { ..., features = ["store"] }
```

```rust
use std::path::Path;
use tacet::{Config, detection};

// One-time per season — fingerprint + reference build:
let paths = [
    Path::new("/media/show/s01e01.mkv"),
    Path::new("/media/show/s01e02.mkv"),
    Path::new("/media/show/s01e03.mkv"),
];
let refs = detection::bootstrap_season(&paths, &Config::default())?;

// Persist `refs.intro` / `refs.credits` (each `Vec<ReferenceFingerprint>`,
// both `serde::Serialize`) in your DB of choice.

// Per-episode detection — fast O(n) hashtable lookups against the refs:
let markers = detection::detect_single_episode(
    Path::new("/media/show/s01e04.mkv"),
    "show-s01e04",
    &refs.intro,
    &refs.credits,
    &Config::default(),
)?;
// markers.intro / markers.credits are Option<Segment>; each Segment carries
// .start_ms() / .end_ms() / .duration() / .source for the SegmentSource enum
// (AudioFingerprint vs Blackframe).
```

Tacet's CPU-bound work uses Rayon. Async callers should `tokio::task::spawn_blocking(|| detect_single_episode(...))` to keep the runtime responsive.

## How it works

1. **Decode** — symphonia extracts audio to mono PCM at 16kHz. Only the first and last N minutes are decoded (configurable scan window). If symphonia can't handle the codec (HE-AAC, E-AC3/Atmos, PCM-in-MKV…), tacet falls back to an ffmpeg subprocess automatically.
2. **STFT** — 4096-point FFT with Hann window, 2048-sample hop. rustfft auto-selects AVX2/SSE4/NEON at runtime.
3. **Peak extraction** — Spectrum divided into 6 logarithmic bands. Strongest local maximum per band per frame.
4. **Constellation hashing** — Each peak paired with its 5 nearest future peaks. Hash = (freq1, freq2, time_delta) packed into 32 bits.
5. **Adaptive bootstrap** — First 3 episodes' fingerprints intersect into a reference. Any remaining episodes that *don't* match that reference are clustered and used to bootstrap a second reference (handles OP/ED swaps mid-season). Repeated until no more references can be built.
6. **Fast match** — Remaining episodes matched against the reference set via offset histogram. Best-scoring reference wins. Sub-second per episode.
7. **Boundary refinement** — Match edges snapped to RMS energy transitions for clean skip points.
8. **Blackframe credits fallback** — When audio fingerprinting can't find credits (live-action shows where each episode's credits are unique), ffmpeg's `blackdetect` filter scans the last few minutes at 160×90/2fps. The longest qualifying black segment marks the credits start; credits_end = file end. Tagged `SegmentSource::Blackframe` so consumers can distinguish heuristic from fingerprint matches.

## Building

```bash
# Release build with native CPU optimizations
cargo build --release --workspace

# Run tests
cargo test --release --workspace

# Run benchmarks (per crate)
cargo bench -p tacet-core
```

## Configuration

All parameters are tunable via `Config`:

```rust
Config {
    sample_rate: 16_000,         // Lower = faster. 16kHz captures speech + music.
    fft_size: 4096,              // Frequency resolution
    hop_size: 2048,              // ~128ms per frame at 16kHz
    num_bands: 6,                // Spectral bands for peak picking
    intro_scan_minutes: 18.0,    // How far into episode to scan (catches long cold opens)
    credits_scan_minutes: 8.0,   // How far from end to scan
    min_segment_seconds: 5.0,    // Minimum reportable segment (Plex: 20s)
    min_credits_seconds: 30.0,   // Credits-specific floor (rejects short stings)
    max_credits_tail_gap: 30.0,  // Credits must end within N s of file end
    blackframe_fallback: true,        // Enable blackdetect-based credits fallback
    blackframe_scan_minutes: 3.0,
    blackframe_fps: 2.0,              // Frame sampling for blackdetect (faster)
    blackframe_min_seconds: 3.0,      // Min consecutive black to count
    blackframe_pix_threshold: 0.10,
    blackframe_hwaccel: None,         // Some("auto") for serial workflows (see below)
    blackframe_timeout_seconds: 60,   // Watchdog — kills hung hwaccel decodes
    match_threshold: 0.08,       // Fraction of reference hashes voting
    fan_out: 5,                  // Peaks paired per anchor
    max_target_delta: 50,        // Max frame gap for pairing
}
```

### Hardware acceleration for the blackframe pass

`blackframe_hwaccel` defaults to `None` (software decode). Counter-intuitively, this is the right default for **batch / parallel** workflows like `tacet scan` — 10+ episodes processed in parallel saturate all CPU cores efficiently, but serialize on the GPU's 1–4 decoder slots when hwaccel is on. We measured **30% slower** wall time with `hwaccel=auto` on a 10-episode Silo scan vs software.

For **serial / per-file** workflows (a media server's discovery-pipeline worker that runs detection on one file at a time), turn it on:

```rust
Config {
    blackframe_hwaccel: Some("auto".to_string()), // or "cuda", "vaapi", "qsv", "videotoolbox", …
    ..Config::default()
}
```

Per-call wall time drops modestly (3.2s → 2.8s on our test bench) and CPU usage drops ~7×, freeing cores for transcoding or other work. If the chosen accelerator hangs (broken VAAPI driver is the canonical case — we saw an 8-minute hang in the wild), the watchdog kills the child after `blackframe_timeout_seconds` and retries with software automatically.

## License

MIT
