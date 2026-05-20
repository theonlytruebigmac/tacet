use criterion::{criterion_group, criterion_main, Criterion};
use tacet::fingerprint;
use tacet::Config;

fn bench_stft(c: &mut Criterion) {
    let config = Config::default();
    // Simulate 10 minutes of 16kHz mono audio
    let samples: Vec<f32> = (0..16_000 * 600)
        .map(|i| (i as f32 * 0.01).sin() * 0.5)
        .collect();

    c.bench_function("stft_10min_16khz", |b| {
        b.iter(|| fingerprint::compute_stft(&samples, &config))
    });
}

fn bench_fingerprint(c: &mut Criterion) {
    let config = Config::default();
    // Simulate 60 seconds of audio (quicker per iteration)
    let samples: Vec<f32> = (0..16_000 * 60)
        .map(|i| (i as f32 * 0.01).sin() * 0.5)
        .collect();

    let region = tacet::audio::AudioRegion {
        samples,
        sample_rate: 16_000,
        offset_seconds: 0.0,
        total_duration: Some(60.0),
    };

    c.bench_function("fingerprint_60s", |b| {
        b.iter(|| fingerprint::fingerprint(&region, &config))
    });
}

criterion_group!(benches, bench_stft, bench_fingerprint);
criterion_main!(benches);
