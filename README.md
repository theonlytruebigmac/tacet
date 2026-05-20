# Tacet

Ultra-fast audio fingerprinting for intro and credit detection in TV series and Movies. Built in Rust for maximum throughput and zero external dependencies (no ffmpeg).

## Why Tacet over Plex's built-in detection?

| | Plex | Tacet |
|---|---|---|
| **Matching strategy** | O(n²) pairwise per season | O(n) reference-based |
| **24-episode season** | 276 comparisons | 3 bootstrap + 21 lookups |
| **Audio decoding** | Requires ffmpeg/transcoder enabled | Pure Rust (symphonia) |
| **Minimum intro length** | 20 seconds | 5 seconds (configurable) |
| **Credits detection** | Separate cloud-dependent system | Unified local pipeline |
| **New episode** | May re-analyze entire season | Single episode vs cached reference |
| **Parallelism** | Single-threaded analysis | Rayon work-stealing across all cores |
| **Memory** | Decodes full audio track | Streams only the scan window |

## Architecture

```
┌─────────────────────────────────────────────────────────────┐
│                        CLI / HTTP API                        │
├──────────────┬──────────────────────────────┬────────────────┤
│   Detection  │         Matching             │    Storage     │
│  Orchestrator│  (Reference + longest-run)   │  (SQLite +     │
│              │                              │  bincode blobs)│
├──────────────┼──────────────────────────────┤                │
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
└──────────────┴───────────────────────────────────────────────┘
```

See [docs/tacet_architecture_pipeline.svg](docs/tacet_architecture_pipeline.svg) for the data-flow diagram.

## Usage

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

## How it works

1. **Decode** — symphonia extracts audio to mono PCM at 16kHz. Only the first and last N minutes are decoded (configurable scan window).

2. **STFT** — 4096-point FFT with Hann window, 2048-sample hop. rustfft auto-selects AVX2/SSE4/NEON at runtime.

3. **Peak extraction** — Spectrum divided into 6 logarithmic bands. Strongest local maximum per band per frame.

4. **Constellation hashing** — Each peak paired with its 5 nearest future peaks. Hash = (freq1, freq2, time_delta) packed into 32 bits.

5. **Bootstrap** — First 3 episodes cross-matched to build a reference fingerprint. Only hashes confirmed across multiple episodes survive.

6. **Fast match** — Remaining episodes matched against reference via offset histogram. Sub-second per episode.

7. **Boundary refinement** — Match edges snapped to energy transitions for clean skip points.

## Building

```bash
# Release build with native CPU optimizations
cargo build --release

# Run tests
cargo test

# Run benchmarks
cargo bench
```

## Configuration

All parameters are tunable via `Config`:

```rust
Config {
    sample_rate: 16_000,        // Lower = faster. 16kHz captures speech + music.
    fft_size: 4096,             // Frequency resolution
    hop_size: 2048,             // ~128ms per frame at 16kHz
    num_bands: 6,               // Spectral bands for peak picking
    intro_scan_minutes: 10.0,   // How far into episode to scan
    credits_scan_minutes: 8.0,  // How far from end to scan
    min_segment_seconds: 5.0,   // Minimum reportable segment (Plex: 20s)
    match_threshold: 0.08,      // Hash overlap ratio to confirm match
    fan_out: 5,                 // Peaks paired per anchor
    max_target_delta: 50,       // Max frame gap for pairing
}
```

## License

MIT
