//! Persistent storage for markers and fingerprints.
//!
//! Layout under `data_dir`:
//! - `tacet.db`           — SQLite database of detected segment markers
//! - `fp/<series>/s<NN>/e<NN>.intro.bin`     — per-episode intro fingerprint (bincode)
//! - `fp/<series>/s<NN>/e<NN>.credits.bin`   — per-episode credits fingerprint (bincode)
//! - `fp/<series>/s<NN>/intro.ref.<N>.bin`   — bootstrapped intro reference, cluster N
//! - `fp/<series>/s<NN>/credits.ref.<N>.bin` — bootstrapped credits reference, cluster N
//!
//! A season may have multiple references per kind — e.g. an anime that swaps
//! its OP mid-season produces two intro clusters, each with its own reference.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension};

use crate::fingerprint::Fingerprint;
use crate::matching::ReferenceFingerprint;
use crate::{Segment, SegmentMarkers};

pub struct Store {
    conn: Mutex<Connection>,
    fp_dir: PathBuf,
}

impl Store {
    /// Open (and create if needed) the data store at `data_dir`.
    pub fn open(data_dir: &Path) -> Result<Self> {
        fs::create_dir_all(data_dir)
            .with_context(|| format!("creating data dir {}", data_dir.display()))?;
        let fp_dir = data_dir.join("fp");
        fs::create_dir_all(&fp_dir)?;

        let db_path = data_dir.join("tacet.db");
        let conn = Connection::open(&db_path)
            .with_context(|| format!("opening {}", db_path.display()))?;

        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS markers (
                episode_id        TEXT PRIMARY KEY,
                series_id         TEXT NOT NULL,
                season_number     INTEGER NOT NULL,
                intro_start       REAL,
                intro_end         REAL,
                intro_confidence  REAL,
                credits_start     REAL,
                credits_end       REAL,
                credits_confidence REAL,
                updated_at        INTEGER NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_series_season
                ON markers(series_id, season_number);
            "#,
        )?;

        Ok(Self {
            conn: Mutex::new(conn),
            fp_dir,
        })
    }

    /// Insert or update markers for a single episode.
    ///
    /// The episode_id is expected to be globally unique; series + season are
    /// inferred from it when callers query by season.
    pub fn save_markers(&self, m: &SegmentMarkers) -> Result<()> {
        let (series_id, season) = parse_episode_id(&m.episode_id);
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);

        let conn = self.conn.lock().unwrap();
        conn.execute(
            r#"
            INSERT INTO markers (
                episode_id, series_id, season_number,
                intro_start, intro_end, intro_confidence,
                credits_start, credits_end, credits_confidence,
                updated_at
            )
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
            ON CONFLICT(episode_id) DO UPDATE SET
                series_id=excluded.series_id,
                season_number=excluded.season_number,
                intro_start=excluded.intro_start,
                intro_end=excluded.intro_end,
                intro_confidence=excluded.intro_confidence,
                credits_start=excluded.credits_start,
                credits_end=excluded.credits_end,
                credits_confidence=excluded.credits_confidence,
                updated_at=excluded.updated_at
            "#,
            params![
                m.episode_id,
                series_id,
                season,
                m.intro.as_ref().map(|s| s.start),
                m.intro.as_ref().map(|s| s.end),
                m.intro.as_ref().map(|s| s.confidence),
                m.credits.as_ref().map(|s| s.start),
                m.credits.as_ref().map(|s| s.end),
                m.credits.as_ref().map(|s| s.confidence),
                now,
            ],
        )?;
        Ok(())
    }

    /// Fetch markers for every episode in a given series + season.
    pub fn get_season_markers(&self, series_id: &str, season: u32) -> Result<Vec<SegmentMarkers>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            r#"
            SELECT episode_id,
                   intro_start, intro_end, intro_confidence,
                   credits_start, credits_end, credits_confidence
            FROM markers
            WHERE series_id = ?1 AND season_number = ?2
            ORDER BY episode_id
            "#,
        )?;

        let rows = stmt.query_map(params![series_id, season], |row| {
            let episode_id: String = row.get(0)?;
            let intro = build_segment(row.get(1)?, row.get(2)?, row.get(3)?);
            let credits = build_segment(row.get(4)?, row.get(5)?, row.get(6)?);
            Ok(SegmentMarkers {
                episode_id,
                intro,
                credits,
            })
        })?;

        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    /// Lookup markers for a single episode by id.
    pub fn get_markers(&self, episode_id: &str) -> Result<Option<SegmentMarkers>> {
        let conn = self.conn.lock().unwrap();
        let row = conn
            .query_row(
                r#"
                SELECT episode_id,
                       intro_start, intro_end, intro_confidence,
                       credits_start, credits_end, credits_confidence
                FROM markers WHERE episode_id = ?1
                "#,
                params![episode_id],
                |row| {
                    let id: String = row.get(0)?;
                    let intro = build_segment(row.get(1)?, row.get(2)?, row.get(3)?);
                    let credits = build_segment(row.get(4)?, row.get(5)?, row.get(6)?);
                    Ok(SegmentMarkers {
                        episode_id: id,
                        intro,
                        credits,
                    })
                },
            )
            .optional()?;
        Ok(row)
    }

    pub fn save_episode_fingerprint(
        &self,
        series_id: &str,
        season: u32,
        episode_index: u32,
        kind: FingerprintKind,
        fp: &Fingerprint,
    ) -> Result<()> {
        let path = self.episode_fp_path(series_id, season, episode_index, kind);
        write_bincode(&path, fp)
    }

    pub fn load_episode_fingerprint(
        &self,
        series_id: &str,
        season: u32,
        episode_index: u32,
        kind: FingerprintKind,
    ) -> Result<Option<Fingerprint>> {
        let path = self.episode_fp_path(series_id, season, episode_index, kind);
        read_bincode(&path)
    }

    /// Persist the full set of references for a (series, season, kind) tuple,
    /// overwriting any previous set. Cluster indices in the filename match the
    /// position in `references`.
    pub fn save_references(
        &self,
        series_id: &str,
        season: u32,
        kind: FingerprintKind,
        references: &[ReferenceFingerprint],
    ) -> Result<()> {
        let dir = self.season_dir(series_id, season);
        fs::create_dir_all(&dir)?;

        // Sweep any stale cluster files from a previous run with more clusters
        // than this one, so reads don't pick up obsolete data.
        let prefix = format!("{}.ref.", kind.suffix());
        if let Ok(entries) = fs::read_dir(&dir) {
            for entry in entries.flatten() {
                if let Some(name) = entry.file_name().to_str() {
                    if name.starts_with(&prefix) && name.ends_with(".bin") {
                        let _ = fs::remove_file(entry.path());
                    }
                }
            }
        }

        for (idx, reference) in references.iter().enumerate() {
            let path = self.reference_path(series_id, season, kind, idx);
            write_bincode(&path, reference)?;
        }
        Ok(())
    }

    /// Load all references for a (series, season, kind) tuple. Returns an
    /// empty Vec when none exist (i.e. the season has not been bootstrapped).
    pub fn load_references(
        &self,
        series_id: &str,
        season: u32,
        kind: FingerprintKind,
    ) -> Result<Vec<ReferenceFingerprint>> {
        let mut out = Vec::new();
        let mut idx = 0usize;
        loop {
            let path = self.reference_path(series_id, season, kind, idx);
            match read_bincode::<ReferenceFingerprint>(&path)? {
                Some(r) => {
                    out.push(r);
                    idx += 1;
                }
                None => break,
            }
        }
        Ok(out)
    }

    fn season_dir(&self, series_id: &str, season: u32) -> PathBuf {
        self.fp_dir
            .join(sanitize(series_id))
            .join(format!("s{:02}", season))
    }

    fn episode_fp_path(
        &self,
        series_id: &str,
        season: u32,
        episode_index: u32,
        kind: FingerprintKind,
    ) -> PathBuf {
        self.season_dir(series_id, season)
            .join(format!("e{:02}.{}.bin", episode_index, kind.suffix()))
    }

    fn reference_path(
        &self,
        series_id: &str,
        season: u32,
        kind: FingerprintKind,
        cluster_idx: usize,
    ) -> PathBuf {
        self.season_dir(series_id, season)
            .join(format!("{}.ref.{}.bin", kind.suffix(), cluster_idx))
    }
}

// `FingerprintKind` moved to `crate::fingerprint::FingerprintKind` so
// detection paths compile when the `store` feature is disabled. Re-exported
// from this module for back-compat with the v0.1 storage API.
pub use crate::fingerprint::FingerprintKind;

trait FingerprintKindExt {
    fn suffix(self) -> &'static str;
}

impl FingerprintKindExt for FingerprintKind {
    fn suffix(self) -> &'static str {
        match self {
            FingerprintKind::Intro => "intro",
            FingerprintKind::Credits => "credits",
        }
    }
}

fn build_segment(start: Option<f64>, end: Option<f64>, conf: Option<f64>) -> Option<Segment> {
    match (start, end, conf) {
        (Some(s), Some(e), Some(c)) => Some(Segment {
            start: s,
            end: e,
            confidence: c,
            // v0.1 didn't persist the source — assume fingerprint match on
            // load. Forward-compat: add a `source` column in a follow-up
            // migration when consumers actually need to round-trip it.
            source: crate::SegmentSource::AudioFingerprint,
        }),
        _ => None,
    }
}

fn write_bincode<T: serde::Serialize>(path: &Path, value: &T) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let bytes = bincode::serialize(value)?;
    fs::write(path, bytes)
        .with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

fn read_bincode<T: serde::de::DeserializeOwned>(path: &Path) -> Result<Option<T>> {
    if !path.exists() {
        return Ok(None);
    }
    let bytes = fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let value = bincode::deserialize(&bytes)?;
    Ok(Some(value))
}

/// Best-effort split of "series-id-sNNeNN" → (series_id, season).
/// Falls back to ("unknown", 0) when the id doesn't match the expected pattern.
fn parse_episode_id(episode_id: &str) -> (String, u32) {
    if let Some(idx) = episode_id.rfind("-s") {
        let (series, rest) = episode_id.split_at(idx);
        let rest = &rest[2..]; // skip "-s"
        if let Some(e_idx) = rest.find('e') {
            let season_str = &rest[..e_idx];
            if let Ok(season) = season_str.parse::<u32>() {
                return (series.to_string(), season);
            }
        }
    }
    (episode_id.to_string(), 0)
}

fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '\0' => '_',
            other => other,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn save_and_get_markers_roundtrip() {
        let dir = tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();

        let m = SegmentMarkers {
            episode_id: "breaking-bad-s01e02".to_string(),
            intro: Some(Segment {
                start: 5.0,
                end: 65.0,
                confidence: 0.94,
                source: crate::SegmentSource::AudioFingerprint,
            }),
            credits: Some(Segment {
                start: 2800.0,
                end: 2840.0,
                confidence: 0.87,
                source: crate::SegmentSource::AudioFingerprint,
            }),
        };
        store.save_markers(&m).unwrap();

        let got = store.get_markers("breaking-bad-s01e02").unwrap().unwrap();
        assert_eq!(got.episode_id, m.episode_id);
        assert_eq!(got.intro.as_ref().unwrap().start, 5.0);
        assert_eq!(got.credits.as_ref().unwrap().end, 2840.0);

        let season = store.get_season_markers("breaking-bad", 1).unwrap();
        assert_eq!(season.len(), 1);
    }

    #[test]
    fn episode_id_parsing() {
        assert_eq!(
            parse_episode_id("breaking-bad-s01e02"),
            ("breaking-bad".to_string(), 1)
        );
        assert_eq!(
            parse_episode_id("the-wire-s03e10"),
            ("the-wire".to_string(), 3)
        );
        assert_eq!(parse_episode_id("garbled"), ("garbled".to_string(), 0));
    }
}
