//! Command-line interface for the `tacet` binary.

use std::path::PathBuf;

use clap::{Parser, Subcommand, ValueEnum};

#[derive(Parser, Debug)]
#[command(
    name = "tacet",
    version,
    about = "Ultra-fast audio fingerprinting for intro/credit detection"
)]
pub struct Cli {
    /// Directory for the SQLite database and fingerprint cache.
    #[arg(long, global = true, default_value = "~/.local/share/tacet")]
    pub data_dir: PathBuf,

    /// Maximum parallel jobs (defaults to one per CPU core).
    #[arg(long, short = 'j', global = true)]
    pub jobs: Option<usize>,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Fingerprint every episode in a directory and persist markers.
    Scan {
        /// Directory containing the season's media files.
        #[arg(long)]
        dir: PathBuf,
        /// Series identifier (used in storage and the episode id scheme).
        #[arg(long)]
        series: String,
        /// Season number.
        #[arg(long)]
        season: u32,
        /// Minutes from the start to scan for intros. Defaults to the Config default (10.0).
        #[arg(long)]
        intro_scan_window: Option<f32>,
        /// Minutes from the end to scan for credits. Defaults to the Config default (8.0).
        #[arg(long)]
        credits_scan_window: Option<f32>,
    },

    /// Detect markers for a single episode against the season's saved reference.
    Detect {
        /// Path to the media file.
        file: PathBuf,
        /// Series identifier (must match a previous scan).
        #[arg(long)]
        series: String,
        /// Season number (must match a previous scan).
        #[arg(long)]
        season: u32,
    },

    /// Run as an HTTP service.
    #[cfg(feature = "api")]
    Serve {
        /// Address to bind to.
        #[arg(long, default_value = "0.0.0.0:9320")]
        listen: String,
    },

    /// Print previously-detected markers for a series + season.
    Show {
        #[arg(long)]
        series: String,
        #[arg(long)]
        season: u32,
        #[arg(long, value_enum, default_value_t = OutputFormat::Table)]
        format: OutputFormat,
    },

    /// Benchmark the decode + fingerprint pipeline on a single file.
    Bench {
        /// Path to the media file.
        file: PathBuf,
    },
}

#[derive(ValueEnum, Clone, Debug)]
pub enum OutputFormat {
    Json,
    Csv,
    Table,
}
