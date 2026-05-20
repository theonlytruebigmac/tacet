use anyhow::Result;
use clap::Parser;
use tacet::cli::{Cli, Command, OutputFormat};
use tacet::detection::{self, EpisodeFile, Season};
use tacet::storage::Store;
use tacet::Config;
use std::path::Path;
use tracing::info;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "tacet=info".into()),
        )
        .init();

    let cli = Cli::parse();

    // Configure rayon thread pool
    if let Some(jobs) = cli.jobs {
        rayon::ThreadPoolBuilder::new()
            .num_threads(jobs)
            .build_global()?;
    }

    let data_dir = shellexpand::tilde(&cli.data_dir.to_string_lossy()).to_string();
    let data_path = Path::new(&data_dir);

    match cli.command {
        Command::Scan {
            dir,
            series,
            season,
            intro_scan_window,
            credits_scan_window,
        } => {
            let store = Store::open(data_path)?;
            let defaults = Config::default();
            let config = Config {
                intro_scan_minutes: intro_scan_window.unwrap_or(defaults.intro_scan_minutes),
                credits_scan_minutes: credits_scan_window.unwrap_or(defaults.credits_scan_minutes),
                ..defaults
            };

            let episodes = discover_episodes(&dir, &series, season)?;
            info!("Found {} episodes", episodes.len());

            let season_data = Season {
                series_id: series.clone(),
                season_number: season,
                episodes,
            };

            let start = std::time::Instant::now();
            let result = detection::detect_season(&season_data, &config)?;
            let elapsed = start.elapsed();

            for m in &result.markers {
                store.save_markers(m)?;
                print_marker(m);
            }

            info!(
                "Processed {} episodes in {:.1}s ({:.0}ms/episode)",
                result.markers.len(),
                elapsed.as_secs_f64(),
                elapsed.as_millis() as f64 / result.markers.len().max(1) as f64,
            );
        }

        Command::Detect {
            file,
            series,
            season,
        } => {
            let store = Store::open(data_path)?;
            let config = Config::default();
            let episode_id = file
                .file_stem()
                .unwrap_or_default()
                .to_string_lossy()
                .to_string();

            let intro_ref =
                store.load_reference(&series, season, tacet::storage::FingerprintKind::Intro)?;
            let credits_ref =
                store.load_reference(&series, season, tacet::storage::FingerprintKind::Credits)?;

            let markers = detection::detect_single_episode(
                &file,
                &episode_id,
                intro_ref.as_ref(),
                credits_ref.as_ref(),
                &config,
            )?;

            store.save_markers(&markers)?;
            print_marker(&markers);
        }

        #[cfg(feature = "api")]
        Command::Serve { listen } => {
            let store = Store::open(data_path)?;
            let config = Config::default();

            let state = std::sync::Arc::new(tacet::api::AppState { store, config });

            let app = tacet::api::router(state);
            let listener = tokio::net::TcpListener::bind(&listen).await?;
            info!("Listening on {listen}");
            axum::serve(listener, app).await?;
        }

        Command::Show {
            series,
            season,
            format,
        } => {
            let store = Store::open(data_path)?;
            let markers = store.get_season_markers(&series, season)?;

            match format {
                OutputFormat::Json => {
                    println!("{}", serde_json::to_string_pretty(&markers)?);
                }
                OutputFormat::Csv => {
                    println!("episode_id,intro_start,intro_end,credits_start,credits_end");
                    for m in &markers {
                        println!(
                            "{},{},{},{},{}",
                            m.episode_id,
                            m.intro.as_ref().map(|s| s.start).unwrap_or(-1.0),
                            m.intro.as_ref().map(|s| s.end).unwrap_or(-1.0),
                            m.credits.as_ref().map(|s| s.start).unwrap_or(-1.0),
                            m.credits.as_ref().map(|s| s.end).unwrap_or(-1.0),
                        );
                    }
                }
                OutputFormat::Table => {
                    println!("{:<30} {:>10} {:>10} {:>10} {:>10}", "Episode", "Intro↦", "↤Intro", "Cred↦", "↤Cred");
                    println!("{}", "-".repeat(74));
                    for m in &markers {
                        println!(
                            "{:<30} {:>10} {:>10} {:>10} {:>10}",
                            m.episode_id,
                            m.intro.as_ref().map(|s| format!("{:.1}s", s.start)).unwrap_or_else(|| "—".to_string()),
                            m.intro.as_ref().map(|s| format!("{:.1}s", s.end)).unwrap_or_else(|| "—".to_string()),
                            m.credits.as_ref().map(|s| format!("{:.1}s", s.start)).unwrap_or_else(|| "—".to_string()),
                            m.credits.as_ref().map(|s| format!("{:.1}s", s.end)).unwrap_or_else(|| "—".to_string()),
                        );
                    }
                }
            }
        }

        Command::Bench { file } => {
            let config = Config::default();
            println!("Benchmarking: {}", file.display());

            let start = std::time::Instant::now();
            let region = tacet::audio::decode_intro_region(&file, &config)?;
            let decode_time = start.elapsed();

            let start = std::time::Instant::now();
            let fp = tacet::fingerprint::fingerprint(&region, &config);
            let fp_time = start.elapsed();

            println!("  Decode:      {:>8.1}ms ({} samples)", decode_time.as_secs_f64() * 1000.0, region.samples.len());
            println!("  Fingerprint: {:>8.1}ms ({} hashes)", fp_time.as_secs_f64() * 1000.0, fp.len());
            println!("  Total:       {:>8.1}ms", (decode_time + fp_time).as_secs_f64() * 1000.0);
        }
    }

    Ok(())
}

fn discover_episodes(dir: &Path, series_id: &str, season: u32) -> Result<Vec<EpisodeFile>> {
    let mut episodes: Vec<EpisodeFile> = std::fs::read_dir(dir)?
        .filter_map(|entry| entry.ok())
        .filter(|entry| {
            let path = entry.path();
            matches!(
                path.extension().and_then(|e| e.to_str()),
                Some("mkv" | "mp4" | "avi" | "m4v" | "ts" | "flac" | "mp3" | "ogg" | "wav")
            )
        })
        .enumerate()
        .map(|(i, entry)| EpisodeFile {
            id: format!("{series_id}-s{season:02}e{:02}", i + 1),
            path: entry.path(),
            episode_number: (i + 1) as u32,
        })
        .collect();

    episodes.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(episodes)
}

fn print_marker(m: &tacet::SegmentMarkers) {
    let intro_str = m
        .intro
        .as_ref()
        .map(|s| format!("{:.1}s–{:.1}s ({:.0}%)", s.start, s.end, s.confidence * 100.0))
        .unwrap_or_else(|| "not found".to_string());

    let credits_str = m
        .credits
        .as_ref()
        .map(|s| format!("{:.1}s–{:.1}s ({:.0}%)", s.start, s.end, s.confidence * 100.0))
        .unwrap_or_else(|| "not found".to_string());

    println!("  {} → intro: {} | credits: {}", m.episode_id, intro_str, credits_str);
}
