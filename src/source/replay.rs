//! Replays a CSV recording (see `record.rs`) as a sample source.
//!
//! Sample times are rebuilt from the recorded timestamps, so the detector sees
//! the original timing whatever the playback speed.

use std::fs::File;
use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use async_trait::async_trait;

use super::SampleSource;
use crate::record::Row;
use crate::sample::Sample;

pub struct ReplaySource {
    path: PathBuf,
    name: String,
    rows: csv::DeserializeRecordsIntoIter<BufReader<File>, Row>,
    /// `Some(speed)` paces playback at `speed` times real time; `None` plays
    /// as fast as the detector consumes.
    speed: Option<f64>,
    t0: Instant,
    started: tokio::time::Instant,
    first_ts: Option<u64>,
    row: u64,
}

impl ReplaySource {
    pub fn open(path: &Path, speed: Option<f64>) -> Result<Self> {
        let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
        let rows = csv::Reader::from_reader(BufReader::new(file)).into_deserialize();
        Ok(Self {
            path: path.to_path_buf(),
            name: format!("replay:{}", path.display()),
            rows,
            speed,
            t0: Instant::now(),
            started: tokio::time::Instant::now(),
            first_ts: None,
            row: 0,
        })
    }
}

#[async_trait]
impl SampleSource for ReplaySource {
    fn link_id(&self) -> &str {
        &self.name
    }

    async fn next(&mut self) -> Result<Option<Sample>> {
        // Reads are small and local; blocking briefly here is fine.
        let Some(row) = self.rows.next() else {
            return Ok(None);
        };
        self.row += 1;
        let row = row.with_context(|| format!("{}: row {}", self.path.display(), self.row))?;
        let csi = row
            .csi_amplitudes()
            .with_context(|| format!("{}: row {}", self.path.display(), self.row))?;
        let first = *self.first_ts.get_or_insert(row.ts_unix_ms);
        let offset = Duration::from_millis(row.ts_unix_ms.saturating_sub(first));
        if let Some(speed) = self.speed {
            tokio::time::sleep_until(self.started + offset.div_f64(speed)).await;
        }
        Ok(Some(Sample {
            link_id: row.link_id,
            t: self.t0 + offset,
            ts_unix_ms: row.ts_unix_ms,
            rssi_dbm: row.rssi_dbm,
            csi_amplitudes: csi,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn rebuilds_timing_from_timestamps() {
        let path = std::env::temp_dir().join(format!("wisense-replay-{}.csv", std::process::id()));
        std::fs::write(
            &path,
            "ts_unix_ms,link_id,rssi_dbm,csi\n\
             1000,wlp2s0,-48.0,\n\
             1050,kitchen,-61.0,1.000;2.000\n\
             1250,wlp2s0,-49.0,\n",
        )
        .unwrap();
        let mut src = ReplaySource::open(&path, None).unwrap();
        let mut samples = Vec::new();
        while let Some(s) = src.next().await.unwrap() {
            samples.push(s);
        }
        std::fs::remove_file(&path).unwrap();

        assert_eq!(samples.len(), 3);
        assert_eq!(samples[1].link_id, "kitchen");
        assert_eq!(samples[1].csi_amplitudes, Some(vec![1.0, 2.0]));
        assert_eq!(samples[2].rssi_dbm, Some(-49.0));
        assert_eq!(
            samples[2].t.duration_since(samples[0].t),
            Duration::from_millis(250)
        );
    }

    #[tokio::test]
    async fn malformed_row_is_an_error_with_position() {
        let path = std::env::temp_dir().join(format!("wisense-bad-{}.csv", std::process::id()));
        std::fs::write(&path, "ts_unix_ms,link_id,rssi_dbm,csi\nnot-a-number,x,,\n").unwrap();
        let mut src = ReplaySource::open(&path, None).unwrap();
        let err = src.next().await.unwrap_err();
        std::fs::remove_file(&path).unwrap();
        assert!(format!("{err:#}").contains("row 1"), "{err:#}");
    }
}
