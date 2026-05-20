mod cli;

use anyhow::Result;
use clap::Parser;
use cli::{Cli, Command, OutputFormat};
use tacet::detection::{self, EpisodeFile, Season};
use tacet::storage::Store;
use tacet::Config;
use std::path::{Path, PathBuf};
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

            let intro_refs =
                store.load_references(&series, season, tacet::storage::FingerprintKind::Intro)?;
            let credits_refs =
                store.load_references(&series, season, tacet::storage::FingerprintKind::Credits)?;

            let markers = detection::detect_single_episode(
                &file,
                &episode_id,
                &intro_refs,
                &credits_refs,
                &config,
            )?;

            store.save_markers(&markers)?;
            print_marker(&markers);
        }

        #[cfg(feature = "api")]
        Command::Serve { listen } => {
            let store = Store::open(data_path)?;
            let config = Config::default();

            let state = std::sync::Arc::new(tacet_api::AppState { store, config });

            let app = tacet_api::router(state);
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
    let media_paths: Vec<PathBuf> = std::fs::read_dir(dir)?
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| {
            matches!(
                path.extension().and_then(|e| e.to_str()),
                Some("mkv" | "mp4" | "avi" | "m4v" | "ts" | "flac" | "mp3" | "ogg" | "wav")
            )
        })
        .collect();

    // Parse the SxxExx pattern from each filename so episode numbers and IDs
    // line up with reality, even when the directory has gaps (missing E05, E11..)
    // or when the filesystem returns entries out of alphabetical order.
    let mut parsed: Vec<(Option<u32>, PathBuf)> = media_paths
        .into_iter()
        .map(|p| {
            let name = p.file_stem().and_then(|s| s.to_str()).unwrap_or("");
            (parse_episode_number(name, season), p)
        })
        .collect();

    parsed.sort_by(|a, b| match (a.0, b.0) {
        (Some(ax), Some(bx)) => ax.cmp(&bx),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => a.1.cmp(&b.1),
    });

    let episodes = parsed
        .into_iter()
        .enumerate()
        .map(|(i, (ep_num, path))| {
            let number = ep_num.unwrap_or((i + 1) as u32);
            EpisodeFile {
                id: format!("{series_id}-s{season:02}e{number:02}"),
                path,
                episode_number: number,
            }
        })
        .collect();

    Ok(episodes)
}

/// Extract the episode number from a filename containing `S##E##` or `s##e##`.
///
/// If a season number is given, only matches whose season component equals it
/// are accepted; this avoids `S02E01` getting picked up during a Season 1 scan
/// of a mixed directory.
fn parse_episode_number(stem: &str, expected_season: u32) -> Option<u32> {
    let bytes = stem.as_bytes();
    let mut i = 0;
    while i + 1 < bytes.len() {
        if (bytes[i] == b'S' || bytes[i] == b's') && bytes[i + 1].is_ascii_digit() {
            let (season, after_season) = read_uint(bytes, i + 1)?;
            if after_season < bytes.len()
                && (bytes[after_season] == b'E' || bytes[after_season] == b'e')
                && after_season + 1 < bytes.len()
                && bytes[after_season + 1].is_ascii_digit()
            {
                let (episode, _) = read_uint(bytes, after_season + 1)?;
                if season == expected_season {
                    return Some(episode);
                }
            }
        }
        i += 1;
    }
    None
}

fn read_uint(bytes: &[u8], start: usize) -> Option<(u32, usize)> {
    let mut end = start;
    while end < bytes.len() && bytes[end].is_ascii_digit() {
        end += 1;
    }
    if end == start {
        return None;
    }
    let s = std::str::from_utf8(&bytes[start..end]).ok()?;
    let n: u32 = s.parse().ok()?;
    Some((n, end))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_episode_number_matches_common_formats() {
        assert_eq!(parse_episode_number("Golden Time - S01E07 - Masquerade", 1), Some(7));
        assert_eq!(parse_episode_number("show.s01e07.title", 1), Some(7));
        assert_eq!(parse_episode_number("Show - S2E12 - Foo", 2), Some(12));
    }

    #[test]
    fn parse_episode_number_rejects_wrong_season() {
        // S02E01 must not match a Season 1 scan.
        assert_eq!(parse_episode_number("Show - S02E01 - Foo", 1), None);
    }

    #[test]
    fn parse_episode_number_returns_none_for_no_match() {
        assert_eq!(parse_episode_number("random video clip", 1), None);
        assert_eq!(parse_episode_number("Episode 7 of something", 1), None);
    }
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
