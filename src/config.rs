//! Runtime configuration structs and their defaults.
//!
//! The CLI (`main.rs`) maps its flags onto these structs; the defaults here are
//! also the CLI defaults, so they live in one place.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use clap::ValueEnum;
use serde::{Deserialize, Serialize};

use crate::detector::{Baseline, ScoreKind};

/// Sliding window used for the motion score.
pub const DEFAULT_WINDOW_MS: u64 = 2000;
/// `k` in `threshold = baseline_mean + k * baseline_std`.
pub const DEFAULT_SENSITIVITY: f32 = 4.0;
/// The score must stay above the threshold this long before MOTION starts.
pub const DEFAULT_ON_MS: u64 = 600;
/// The score must stay below the release level this long before MOTION ends.
pub const DEFAULT_OFF_MS: u64 = 3000;
/// MOTION ends only below `OFF_RATIO * threshold` (hysteresis).
pub const DEFAULT_OFF_RATIO: f32 = 0.7;
/// How often the detector emits a `Status` event per link.
pub const DEFAULT_STATUS_MS: u64 = 500;
/// Length of the automatic calibration when no baseline is available.
pub const DEFAULT_AUTO_CALIBRATION_MS: u64 = 20_000;
/// The threshold never goes below this, whatever the baseline says. Guards
/// against a perfectly flat calibration producing a hair trigger.
pub const DEFAULT_MIN_THRESHOLD: f32 = 0.05;
/// The threshold is never below this multiple of the idle mean score, which
/// keeps the release level (`off_ratio * threshold`) above normal idle scores.
pub const DEFAULT_MIN_THRESHOLD_RATIO: f32 = 2.0;

/// How often `/proc/net/wireless` is read.
pub const DEFAULT_POLL_HZ: f32 = 20.0;
/// Seconds between keepalive pings. Older ping versions refuse intervals
/// below 0.2 s without root; current iputils allows down to 0.002 s.
pub const DEFAULT_PING_INTERVAL_S: f64 = 0.2;

/// Default length of `wisense calibrate`.
pub const DEFAULT_CALIBRATE_SECONDS: u64 = 60;
/// ESP32-CSI-Tool's recommended serial speed.
pub const DEFAULT_BAUD: u32 = 921_600;
/// A new zone must stay on top this long before the zone switches.
pub const DEFAULT_ZONE_HOLD_MS: u64 = 1000;

/// Which statistic of the windowed signal is used as the motion score.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, ValueEnum, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Metric {
    /// Mean absolute first difference. Handles 1 dBm quantization well and
    /// ignores slow level changes.
    #[default]
    Diff,
    /// Standard deviation of the window.
    Std,
}

/// Everything the detector needs to know. See `detector.rs`.
#[derive(Debug, Clone, PartialEq)]
pub struct DetectorConfig {
    pub window: Duration,
    pub metric: Metric,
    pub sensitivity: f32,
    pub on: Duration,
    pub off: Duration,
    pub off_ratio: f32,
    pub status_every: Duration,
    pub auto_calibration: Duration,
    pub min_threshold: f32,
    pub min_threshold_ratio: f32,
}

impl Default for DetectorConfig {
    fn default() -> Self {
        Self {
            window: Duration::from_millis(DEFAULT_WINDOW_MS),
            metric: Metric::default(),
            sensitivity: DEFAULT_SENSITIVITY,
            on: Duration::from_millis(DEFAULT_ON_MS),
            off: Duration::from_millis(DEFAULT_OFF_MS),
            off_ratio: DEFAULT_OFF_RATIO,
            status_every: Duration::from_millis(DEFAULT_STATUS_MS),
            auto_calibration: Duration::from_millis(DEFAULT_AUTO_CALIBRATION_MS),
            min_threshold: DEFAULT_MIN_THRESHOLD,
            min_threshold_ratio: DEFAULT_MIN_THRESHOLD_RATIO,
        }
    }
}

/// One link's saved calibration, plus the settings it was measured with. A
/// baseline only means something for the same metric, window and sampling.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BaselineEntry {
    pub mean: f32,
    pub std: f32,
    pub kind: ScoreKind,
    pub metric: Metric,
    pub window_ms: u64,
    /// Poll rate of the laptop link (`None` for ESP32 links).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub poll_hz: Option<f32>,
    /// Keepalive ping interval (`None` if keepalive was off or not relevant).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ping_interval_s: Option<f64>,
    /// How long the calibration ran.
    pub seconds: f64,
    pub calibrated_unix_ms: u64,
}

/// How the current run samples a link; compared against a saved baseline.
#[derive(Debug, Clone, PartialEq)]
pub struct LinkSetup {
    pub kind: ScoreKind,
    pub metric: Metric,
    pub window_ms: u64,
    pub poll_hz: Option<f32>,
    pub ping_interval_s: Option<f64>,
}

impl BaselineEntry {
    pub fn baseline(&self) -> Baseline {
        Baseline {
            mean: self.mean,
            std: self.std,
        }
    }

    /// `Ok(warnings)` if this baseline can be used for `setup`, `Err(reason)`
    /// if it can't. The metric, window, kind and poll rate all change the
    /// scale of the score; the ping interval changes it less, so a different
    /// one only warns.
    pub fn check(&self, setup: &LinkSetup) -> Result<Vec<String>, String> {
        if self.kind != setup.kind {
            return Err(format!(
                "it was measured from {:?} but this link gives {:?}",
                self.kind, setup.kind
            ));
        }
        // The metric only applies to RSSI links; CSI has its own score.
        if self.kind == ScoreKind::Rssi && self.metric != setup.metric {
            return Err(format!(
                "it was made with --metric {:?}, now {:?}",
                self.metric, setup.metric
            ));
        }
        if self.window_ms != setup.window_ms {
            return Err(format!(
                "it was made with --window-ms {}, now {}",
                self.window_ms, setup.window_ms
            ));
        }
        if !same(self.poll_hz.map(f64::from), setup.poll_hz.map(f64::from)) {
            return Err(format!(
                "it was made with --poll-hz {}, now {}",
                show(self.poll_hz, " Hz"),
                show(setup.poll_hz, " Hz")
            ));
        }
        let mut warnings = Vec::new();
        if !same(self.ping_interval_s, setup.ping_interval_s) {
            warnings.push(format!(
                "baseline was made with keepalive ping interval {}, now {}; \
                 recalibrate if you get false alarms or miss motion",
                show(self.ping_interval_s, " s"),
                show(setup.ping_interval_s, " s")
            ));
        }
        Ok(warnings)
    }
}

fn same(a: Option<f64>, b: Option<f64>) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => (a - b).abs() < 1e-6,
        (None, None) => true,
        _ => false,
    }
}

fn show<T: std::fmt::Display>(v: Option<T>, unit: &str) -> String {
    v.map_or_else(|| "off".to_string(), |v| format!("{v}{unit}"))
}

/// The baseline file: `link_id -> entry`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BaselineFile {
    pub version: u32,
    pub links: BTreeMap<String, BaselineEntry>,
}

impl Default for BaselineFile {
    fn default() -> Self {
        Self {
            version: 1,
            links: BTreeMap::new(),
        }
    }
}

/// `~/.config/wisense/baseline.json` (or the XDG equivalent).
pub fn default_baseline_path() -> Result<PathBuf> {
    let dir = dirs::config_dir().context("can't find a config directory (is $HOME set?)")?;
    Ok(dir.join("wisense").join("baseline.json"))
}

impl BaselineFile {
    /// `None` if the file doesn't exist.
    pub fn load(path: &Path) -> Result<Option<Self>> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        let file = serde_json::from_str(&text)
            .with_context(|| format!("parsing baseline file {}", path.display()))?;
        Ok(Some(file))
    }

    /// Write atomically (temp file + rename), creating the directory.
    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        let json = serde_json::to_string_pretty(self).context("serializing baseline")?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, json + "\n").with_context(|| format!("writing {}", tmp.display()))?;
        std::fs::rename(&tmp, path)
            .with_context(|| format!("moving {} to {}", tmp.display(), path.display()))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry() -> BaselineEntry {
        BaselineEntry {
            mean: 1.2,
            std: 0.3,
            kind: ScoreKind::Rssi,
            metric: Metric::Diff,
            window_ms: 2000,
            poll_hz: Some(20.0),
            ping_interval_s: Some(0.2),
            seconds: 60.0,
            calibrated_unix_ms: 1_790_000_000_000,
        }
    }

    fn setup() -> LinkSetup {
        LinkSetup {
            kind: ScoreKind::Rssi,
            metric: Metric::Diff,
            window_ms: 2000,
            poll_hz: Some(20.0),
            ping_interval_s: Some(0.2),
        }
    }

    #[test]
    fn matching_setup_is_usable() {
        assert_eq!(entry().check(&setup()), Ok(vec![]));
    }

    #[test]
    fn scale_changing_settings_make_it_unusable() {
        let e = entry();
        for s in [
            LinkSetup {
                metric: Metric::Std,
                ..setup()
            },
            LinkSetup {
                window_ms: 3000,
                ..setup()
            },
            LinkSetup {
                poll_hz: Some(50.0),
                ..setup()
            },
            LinkSetup {
                kind: ScoreKind::Csi,
                ..setup()
            },
        ] {
            assert!(e.check(&s).is_err(), "{s:?}");
        }
    }

    #[test]
    fn different_ping_interval_only_warns() {
        let warnings = entry()
            .check(&LinkSetup {
                ping_interval_s: None,
                ..setup()
            })
            .unwrap();
        assert_eq!(warnings.len(), 1);
        assert!(
            warnings[0].contains("interval 0.2 s, now off"),
            "{warnings:?}"
        );
    }

    #[test]
    fn save_and_load_roundtrip() {
        let dir = std::env::temp_dir().join(format!("wisense-test-{}", std::process::id()));
        let path = dir.join("nested").join("baseline.json");
        assert_eq!(BaselineFile::load(&path).unwrap(), None);
        let mut file = BaselineFile::default();
        file.links.insert("wlp2s0".into(), entry());
        file.links.insert(
            "kitchen".into(),
            BaselineEntry {
                kind: ScoreKind::Csi,
                poll_hz: None,
                ping_interval_s: None,
                ..entry()
            },
        );
        file.save(&path).unwrap();
        assert_eq!(BaselineFile::load(&path).unwrap(), Some(file));
        std::fs::write(&path, "{not json").unwrap();
        assert!(BaselineFile::load(&path).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
