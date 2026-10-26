//! ESP32 boards running ESP32-CSI-Tool
//! (<https://github.com/StevenMHernandez/ESP32-CSI-Tool>), read over USB serial.
//!
//! Line format, checked against the firmware's `_components/csi_component.h`
//! (`_wifi_csi_cb` and its CSV header):
//!
//! ```text
//! type,role,mac,rssi,rate,sig_mode,mcs,bandwidth,smoothing,not_sounding,
//! aggregation,stbc,fec_coding,sgi,noise_floor,ampdu_cnt,channel,
//! secondary_channel,local_timestamp,ant,sig_len,rx_state,real_time_set,
//! real_timestamp,len,CSI_DATA
//! ```
//!
//! i.e. 25 comma-separated fields starting with the literal `CSI_DATA`, then
//! the raw CSI buffer as `[v0 v1 v2 ... ]`: space-separated signed bytes in
//! (imaginary, real) pairs, as the repo's `python_utils/parse_csi.py` reads
//! them. Amplitude per subcarrier = sqrt(re² + im²). With the firmware's
//! "LLTF only" option the list has 128 values regardless of `len`, so the
//! list itself is what counts.
//!
//! Guard and null subcarriers are reported as zeros; the detector ignores
//! subcarriers that stay zero, so they are not filtered here.

use std::io::{ErrorKind, Read};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use async_trait::async_trait;
use thiserror::Error;
use tokio::sync::mpsc;

use super::SampleSource;
use crate::sample::{unix_ms_now, Sample};

/// Fields before the CSI list.
const HEADER_FIELDS: usize = 25;
const RSSI_FIELD: usize = 3;
/// A line longer than this without a newline is garbage; drop it.
const MAX_LINE: usize = 16 * 1024;
/// Log the malformed-line count at most this often.
const MALFORMED_LOG_EVERY: u64 = 100;

#[derive(Debug, Error, PartialEq)]
pub enum CsiParseError {
    #[error("not a CSI_DATA line")]
    NotCsi,
    #[error("expected {HEADER_FIELDS} fields before the CSI list, got {0}")]
    TooFewFields(usize),
    #[error("bad RSSI {0:?}")]
    BadRssi(String),
    #[error("CSI list is not enclosed in [ ]")]
    Unbracketed,
    #[error("bad CSI value {0:?}")]
    BadValue(String),
    #[error("CSI list has {0} values; expected a non-zero even number")]
    BadLength(usize),
}

/// The parts of a `CSI_DATA` line wisense uses.
#[derive(Debug, Clone, PartialEq)]
pub struct CsiRecord {
    pub rssi_dbm: f32,
    pub amplitudes: Vec<f32>,
}

/// Parse one serial line. Never panics, whatever the input.
pub fn parse_csi_line(line: &str) -> Result<CsiRecord, CsiParseError> {
    let line = line.trim();
    if !line.starts_with("CSI_DATA,") {
        return Err(CsiParseError::NotCsi);
    }
    let fields: Vec<&str> = line.splitn(HEADER_FIELDS + 1, ',').collect();
    if fields.len() != HEADER_FIELDS + 1 {
        return Err(CsiParseError::TooFewFields(fields.len().saturating_sub(1)));
    }
    let rssi_text = fields[RSSI_FIELD].trim();
    let rssi: i32 = rssi_text
        .parse()
        .ok()
        .filter(|r| (-128..=0).contains(r))
        .ok_or_else(|| CsiParseError::BadRssi(rssi_text.to_string()))?;

    let list = fields[HEADER_FIELDS]
        .trim()
        .strip_prefix('[')
        .and_then(|l| l.strip_suffix(']'))
        .ok_or(CsiParseError::Unbracketed)?;
    let raw = list
        .split_whitespace()
        .map(|v| {
            v.parse::<i16>()
                .ok()
                .filter(|v| (-128..=127).contains(v))
                .ok_or_else(|| CsiParseError::BadValue(v.to_string()))
        })
        .collect::<Result<Vec<i16>, _>>()?;
    if raw.is_empty() || raw.len() % 2 != 0 {
        return Err(CsiParseError::BadLength(raw.len()));
    }
    // The length is even (checked above), so there is no remainder.
    let (pairs, _) = raw.as_chunks::<2>();
    let amplitudes = pairs
        .iter()
        .map(|&[im, re]| {
            let (im, re) = (f32::from(im), f32::from(re));
            (re * re + im * im).sqrt()
        })
        .collect();
    Ok(CsiRecord {
        rssi_dbm: rssi as f32,
        amplitudes,
    })
}

/// `--esp32 PORT=NAME`.
#[derive(Debug, Clone, PartialEq)]
pub struct Esp32Config {
    pub port: String,
    pub name: String,
    pub baud: u32,
}

/// Reads one board. The blocking serial port lives in its own thread, which
/// hands parsed samples to the async side through a channel.
pub struct Esp32Source {
    name: String,
    rx: mpsc::Receiver<Result<Sample>>,
}

impl Esp32Source {
    pub fn open(cfg: Esp32Config) -> Result<Self> {
        let port = serialport::new(&cfg.port, cfg.baud)
            .timeout(Duration::from_millis(500))
            .open()
            .with_context(|| {
                format!(
                    "opening {} for ESP32 '{}' (permission denied? add yourself to the \
                     dialout group: sudo usermod -aG dialout $USER, then log out and in)",
                    cfg.port, cfg.name
                )
            })?;
        let (tx, rx) = mpsc::channel(1024);
        let name = cfg.name.clone();
        std::thread::Builder::new()
            .name(format!("esp32-{}", cfg.name))
            .spawn(move || read_port(port, cfg, tx))
            .context("starting serial reader thread")?;
        Ok(Self { name, rx })
    }
}

#[async_trait]
impl SampleSource for Esp32Source {
    fn link_id(&self) -> &str {
        &self.name
    }

    async fn next(&mut self) -> Result<Option<Sample>> {
        self.rx.recv().await.transpose()
    }
}

/// Blocking loop: read bytes, split lines, parse, forward. Ends when the
/// port fails or the receiver is gone.
fn read_port(
    mut port: Box<dyn serialport::SerialPort>,
    cfg: Esp32Config,
    tx: mpsc::Sender<Result<Sample>>,
) {
    let mut lines = LineSplitter::default();
    let mut buf = [0u8; 4096];
    let mut stats = LineStats::default();
    loop {
        if tx.is_closed() {
            return;
        }
        let n = match port.read(&mut buf) {
            Ok(0) => continue,
            Ok(n) => n,
            Err(e) if matches!(e.kind(), ErrorKind::TimedOut | ErrorKind::Interrupted) => continue,
            Err(e) => {
                let err = anyhow::Error::new(e)
                    .context(format!("reading {} (ESP32 '{}')", cfg.port, cfg.name));
                // Nobody left to tell if this fails.
                let _ = tx.blocking_send(Err(err));
                return;
            }
        };
        for line in lines.push(&buf[..n]) {
            let text = String::from_utf8_lossy(&line);
            match parse_csi_line(&text) {
                Ok(record) => {
                    stats.good += 1;
                    let sample = Sample {
                        link_id: cfg.name.clone(),
                        t: Instant::now(),
                        ts_unix_ms: unix_ms_now(),
                        rssi_dbm: Some(record.rssi_dbm),
                        csi_amplitudes: Some(record.amplitudes),
                    };
                    if tx.blocking_send(Ok(sample)).is_err() {
                        return;
                    }
                }
                // Boot messages, the CSV header, log lines: not ours.
                Err(CsiParseError::NotCsi) => stats.other += 1,
                Err(e) => {
                    stats.malformed += 1;
                    tracing::debug!("{}: skipping malformed line: {e}", cfg.name);
                    if stats.malformed % MALFORMED_LOG_EVERY == 1 {
                        tracing::warn!(
                            "{}: {} malformed CSI lines skipped so far ({} good, {} other); \
                             last: {e}. A wrong --baud often causes this",
                            cfg.name,
                            stats.malformed,
                            stats.good,
                            stats.other
                        );
                    }
                }
            }
        }
    }
}

#[derive(Debug, Default)]
struct LineStats {
    good: u64,
    malformed: u64,
    other: u64,
}

/// Splits a byte stream into lines, tolerating `\r\n`, partial reads and
/// garbage without newlines (dropped once it exceeds `MAX_LINE`).
#[derive(Debug, Default)]
struct LineSplitter {
    pending: Vec<u8>,
    overflowed: bool,
}

impl LineSplitter {
    fn push(&mut self, bytes: &[u8]) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        for &b in bytes {
            if b == b'\n' {
                let line = std::mem::take(&mut self.pending);
                if !std::mem::take(&mut self.overflowed) {
                    out.push(line);
                }
            } else if self.pending.len() < MAX_LINE {
                self.pending.push(b);
            } else {
                self.overflowed = true;
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A line shaped like the firmware's output (values from a real capture
    /// layout; the CSI list is shortened).
    fn line(rssi: &str, csi: &str) -> String {
        format!(
            "CSI_DATA,STA,AA:BB:CC:DD:EE:FF,{rssi},11,1,7,0,1,1,1,0,0,0,-93,0,6,0,\
             3517632,0,48,0,0,3517.632000,8,{csi}"
        )
    }

    #[test]
    fn parses_a_line_and_computes_amplitudes() {
        // Pairs are (imaginary, real): (3, 4) -> 5, (0, 0) -> 0,
        // (-6, 8) -> 10, (1, -1) -> sqrt 2.
        let rec = parse_csi_line(&line("-57", "[3 4 0 0 -6 8 1 -1 ]")).unwrap();
        assert_eq!(rec.rssi_dbm, -57.0);
        assert_eq!(rec.amplitudes.len(), 4);
        assert_eq!(&rec.amplitudes[..3], &[5.0, 0.0, 10.0]);
        assert!((rec.amplitudes[3] - 2f32.sqrt()).abs() < 1e-6);
    }

    #[test]
    fn tolerates_crlf_and_missing_trailing_space() {
        let rec = parse_csi_line(&(line("-40", "[3 4]") + "\r")).unwrap();
        assert_eq!(rec.amplitudes, vec![5.0]);
    }

    #[test]
    fn rejects_malformed_lines() {
        use CsiParseError::*;
        let cases = [
            ("I (512) wifi: connected".to_string(), NotCsi),
            (
                "type,role,mac,rssi,rate,sig_mode,mcs,bandwidth,smoothing,not_sounding,\
                 aggregation,stbc,fec_coding,sgi,noise_floor,ampdu_cnt,channel,\
                 secondary_channel,local_timestamp,ant,sig_len,rx_state,real_time_set,\
                 real_timestamp,len,CSI_DATA"
                    .to_string(),
                NotCsi,
            ),
            ("CSI_DATA,STA,AA:BB,-50,[1 2]".to_string(), TooFewFields(4)),
            (line("abc", "[1 2]"), BadRssi("abc".into())),
            (line("12", "[1 2]"), BadRssi("12".into())),
            (line("-50", "1 2 3 4"), Unbracketed),
            // Cut off mid-transfer, as happens on serial.
            (line("-50", "[1 2 3 4 5"), Unbracketed),
            (line("-50", "[1 x 3 4]"), BadValue("x".into())),
            (line("-50", "[1 200 3 4]"), BadValue("200".into())),
            (line("-50", "[1 2 3]"), BadLength(3)),
            (line("-50", "[]"), BadLength(0)),
        ];
        for (input, expected) in cases {
            assert_eq!(parse_csi_line(&input), Err(expected), "{input}");
        }
    }

    #[test]
    fn never_panics_on_garbage() {
        let mut rng = crate::source::synthetic::Rng::new(9);
        let good = line("-50", "[1 2 3 4 ]");
        for _ in 0..2000 {
            let mut bytes = good.clone().into_bytes();
            let i = (rng.next_u64() as usize) % bytes.len();
            bytes[i] = (rng.next_u64() & 0xff) as u8;
            bytes.truncate((rng.next_u64() as usize) % (bytes.len() + 1));
            let _ = parse_csi_line(&String::from_utf8_lossy(&bytes));
        }
    }

    #[test]
    fn line_splitter_handles_partial_reads_and_overflow() {
        let mut s = LineSplitter::default();
        assert!(s.push(b"CSI_DA").is_empty());
        assert_eq!(s.push(b"TA,1\nab"), vec![b"CSI_DATA,1".to_vec()]);
        assert_eq!(s.push(b"c\n"), vec![b"abc".to_vec()]);
        let garbage = vec![b'x'; MAX_LINE + 10];
        assert!(s.push(&garbage).is_empty());
        // The overflowing line is dropped, the next one is fine.
        assert_eq!(s.push(b"\nok\n"), vec![b"ok".to_vec()]);
    }
}
