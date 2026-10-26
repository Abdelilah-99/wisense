//! The unit of data that every source produces.

use std::time::{Instant, SystemTime, UNIX_EPOCH};

/// One measurement from one radio link.
#[derive(Debug, Clone, PartialEq)]
pub struct Sample {
    /// Which link this came from, e.g. `wlp2s0` or `esp32-kitchen`.
    pub link_id: String,
    /// Monotonic time, used for windowing.
    pub t: Instant,
    /// Wall-clock time, used for recording and display.
    pub ts_unix_ms: u64,
    /// Received signal strength. dBm for almost every driver; a few report a
    /// relative 0-100/0-255 scale instead (see `source::procfs`).
    pub rssi_dbm: Option<f32>,
    /// Per-subcarrier CSI amplitude. Only CSI sources (ESP32) fill this in.
    pub csi_amplitudes: Option<Vec<f32>>,
}

impl Sample {
    /// An RSSI-only sample.
    pub fn rssi(link_id: impl Into<String>, t: Instant, ts_unix_ms: u64, rssi_dbm: f32) -> Self {
        Self {
            link_id: link_id.into(),
            t,
            ts_unix_ms,
            rssi_dbm: Some(rssi_dbm),
            csi_amplitudes: None,
        }
    }
}

/// Current wall-clock time in milliseconds since the Unix epoch.
pub fn unix_ms_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}
