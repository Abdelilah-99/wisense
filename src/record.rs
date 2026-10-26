//! CSV recording of raw samples. The same row format is read back by
//! `source::replay`, so a recording can be fed through the detector again
//! with different settings.
//!
//! Columns: `ts_unix_ms,link_id,rssi_dbm,csi`, where `csi` is the
//! per-subcarrier amplitudes joined with `;` (empty for RSSI-only links).

use std::fs::File;
use std::io::Write;
use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::sample::Sample;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Row {
    pub ts_unix_ms: u64,
    pub link_id: String,
    pub rssi_dbm: Option<f32>,
    pub csi: String,
}

impl Row {
    pub fn from_sample(s: &Sample) -> Self {
        let csi = s
            .csi_amplitudes
            .as_ref()
            .map(|a| {
                a.iter()
                    .map(|v| format!("{v:.3}"))
                    .collect::<Vec<_>>()
                    .join(";")
            })
            .unwrap_or_default();
        Self {
            ts_unix_ms: s.ts_unix_ms,
            link_id: s.link_id.clone(),
            rssi_dbm: s.rssi_dbm,
            csi,
        }
    }

    /// The `csi` column parsed back into amplitudes.
    pub fn csi_amplitudes(&self) -> Result<Option<Vec<f32>>> {
        if self.csi.trim().is_empty() {
            return Ok(None);
        }
        self.csi
            .split(';')
            .map(|v| {
                v.trim()
                    .parse::<f32>()
                    .with_context(|| format!("bad CSI amplitude {v:?}"))
            })
            .collect::<Result<Vec<_>>>()
            .map(Some)
    }
}

/// Writes samples as CSV rows.
pub struct Recorder<W: Write> {
    writer: csv::Writer<W>,
    rows: u64,
}

impl Recorder<File> {
    pub fn create(path: &Path) -> Result<Self> {
        let file = File::create(path).with_context(|| format!("creating {}", path.display()))?;
        Ok(Self::new(file))
    }
}

impl<W: Write> Recorder<W> {
    pub fn new(out: W) -> Self {
        Self {
            writer: csv::Writer::from_writer(out),
            rows: 0,
        }
    }

    pub fn write(&mut self, sample: &Sample) -> Result<()> {
        self.writer
            .serialize(Row::from_sample(sample))
            .context("writing CSV row")?;
        self.rows += 1;
        Ok(())
    }

    pub fn rows(&self) -> u64 {
        self.rows
    }

    pub fn flush(&mut self) -> Result<()> {
        self.writer.flush().context("flushing recording")
    }
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use super::*;

    #[test]
    fn rows_roundtrip_through_csv() {
        let t = Instant::now();
        let samples = [
            Sample::rssi("wlp2s0", t, 1_000, -48.0),
            Sample {
                link_id: "kitchen".into(),
                t,
                ts_unix_ms: 1_050,
                rssi_dbm: Some(-61.0),
                csi_amplitudes: Some(vec![0.0, 5.0, 12.649_111]),
            },
        ];
        let mut rec = Recorder::new(Vec::new());
        for s in &samples {
            rec.write(s).unwrap();
        }
        rec.flush().unwrap();
        assert_eq!(rec.rows(), 2);
        let text = String::from_utf8(rec.writer.into_inner().unwrap()).unwrap();
        assert_eq!(
            text,
            "ts_unix_ms,link_id,rssi_dbm,csi\n\
             1000,wlp2s0,-48.0,\n\
             1050,kitchen,-61.0,0.000;5.000;12.649\n"
        );

        let rows: Vec<Row> = csv::Reader::from_reader(text.as_bytes())
            .deserialize()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(rows[0].rssi_dbm, Some(-48.0));
        assert_eq!(rows[0].csi_amplitudes().unwrap(), None);
        assert_eq!(
            rows[1].csi_amplitudes().unwrap(),
            Some(vec![0.0, 5.0, 12.649])
        );
    }

    /// Regenerates `tests/fixtures/synthetic_walk.csv`, the input of the
    /// replay integration test: 60 s of synthetic RSSI with one burst of
    /// motion from 35 s to 42 s. Run with
    /// `cargo test regenerate_replay_fixture -- --ignored`.
    #[test]
    #[ignore]
    fn regenerate_replay_fixture() {
        use std::time::Duration;

        use crate::source::synthetic::{Burst, SyntheticConfig, SyntheticSignal};

        let cfg = SyntheticConfig {
            link_id: "wlan0".into(),
            seed: 7,
            duration: Some(Duration::from_secs(60)),
            bursts: vec![Burst {
                start: Duration::from_secs(35),
                duration: Duration::from_secs(7),
                std_db: 4.0,
            }],
            ..SyntheticConfig::default()
        };
        let mut signal = SyntheticSignal::new(cfg, Instant::now(), 1_790_000_000_000);
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/synthetic_walk.csv"
        );
        let mut rec = Recorder::create(Path::new(path)).unwrap();
        while let Some(s) = signal.next_sample() {
            rec.write(&s).unwrap();
        }
        rec.flush().unwrap();
        assert_eq!(rec.rows(), 1200);
    }

    #[test]
    fn missing_rssi_and_bad_csi() {
        let text = "ts_unix_ms,link_id,rssi_dbm,csi\n5,x,,1;oops\n";
        let row: Row = csv::Reader::from_reader(text.as_bytes())
            .deserialize()
            .next()
            .unwrap()
            .unwrap();
        assert_eq!(row.rssi_dbm, None);
        assert!(row.csi_amplitudes().is_err());
    }
}
