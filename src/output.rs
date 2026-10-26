//! Event output: human-readable log lines, JSON lines, or (feature `tui`) a
//! live terminal dashboard.
//!
//! Events go to stdout; diagnostics (tracing) go to stderr. On a terminal the
//! human output shows the latest `Status` of every link as a single live line
//! that is redrawn in place; event lines and log messages are printed above
//! it.

#[cfg(feature = "tui")]
pub mod tui;

use std::collections::{BTreeMap, VecDeque};
use std::io::{self, IsTerminal, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use anyhow::{Context, Result};
use chrono::TimeZone;
use tracing_subscriber::fmt::MakeWriter;

use crate::detector::{Event, MotionState, ScoreKind};

/// Clears the current terminal line.
const CLEAR_LINE: &str = "\r\x1b[2K";

/// Whether a live status line is currently drawn on the terminal. Shared with
/// [`LogWriter`] so log messages can clear it first.
static LIVE_LINE: AtomicBool = AtomicBool::new(false);
/// While set, log messages are kept in [`LOG_BUFFER`] instead of going to
/// stderr (the TUI owns the terminal and shows them itself).
static CAPTURE_LOGS: AtomicBool = AtomicBool::new(false);
static LOG_BUFFER: Mutex<VecDeque<String>> = Mutex::new(VecDeque::new());
const LOG_BUFFER_LINES: usize = 200;

pub trait EventSink {
    fn handle(&mut self, event: &Event) -> Result<()>;
    /// Called about ten times a second, also when no samples arrive.
    /// Returns `true` when the user asked to quit.
    fn tick(&mut self) -> Result<bool> {
        Ok(false)
    }
    /// Called once at the end, to leave the terminal tidy.
    fn finish(&mut self) -> Result<()>;
}

/// One JSON object per event and line, for piping into other tools.
pub struct JsonSink<W: Write> {
    out: W,
}

impl<W: Write> JsonSink<W> {
    pub fn new(out: W) -> Self {
        Self { out }
    }
}

impl<W: Write> EventSink for JsonSink<W> {
    fn handle(&mut self, event: &Event) -> Result<()> {
        if let Event::CalibrationStarted { .. } = event {
            // The JSON consumer may not be a person; make sure one sees it.
            if let Some(text) = format_event(event) {
                let text = text.replace('!', "");
                tracing::warn!("{}", text.split_whitespace().collect::<Vec<_>>().join(" "));
            }
        }
        serde_json::to_writer(&mut self.out, event).context("writing JSON event")?;
        writeln!(self.out)?;
        self.out.flush()?;
        Ok(())
    }

    fn finish(&mut self) -> Result<()> {
        self.out.flush()?;
        Ok(())
    }
}

/// Human-readable log lines, plus a live status line on terminals.
pub struct HumanSink<W: Write> {
    out: W,
    live: bool,
    status: BTreeMap<String, StatusView>,
}

#[derive(Debug, Clone)]
struct StatusView {
    ts_unix_ms: u64,
    kind: ScoreKind,
    score: Option<f32>,
    threshold: Option<f32>,
    state: MotionState,
}

impl HumanSink<io::Stdout> {
    /// Writes to stdout, with a live status line if stdout is a terminal.
    pub fn stdout() -> Self {
        let live = io::stdout().is_terminal();
        Self::new(io::stdout(), live)
    }
}

impl<W: Write> HumanSink<W> {
    pub fn new(out: W, live: bool) -> Self {
        Self {
            out,
            live,
            status: BTreeMap::new(),
        }
    }

    fn redraw_status(&mut self) -> Result<()> {
        let Some(line) = status_line(&self.status) else {
            return Ok(());
        };
        write!(self.out, "{CLEAR_LINE}{line}")?;
        self.out.flush()?;
        LIVE_LINE.store(true, Ordering::Relaxed);
        Ok(())
    }
}

impl<W: Write> EventSink for HumanSink<W> {
    fn handle(&mut self, event: &Event) -> Result<()> {
        if let Event::Status {
            link_id,
            ts_unix_ms,
            kind,
            score,
            threshold,
            state,
        } = event
        {
            self.status.insert(
                link_id.clone(),
                StatusView {
                    ts_unix_ms: *ts_unix_ms,
                    kind: *kind,
                    score: *score,
                    threshold: *threshold,
                    state: *state,
                },
            );
            if self.live {
                self.redraw_status()?;
            }
            return Ok(());
        }
        let Some(text) = format_event(event) else {
            return Ok(());
        };
        if self.live && LIVE_LINE.swap(false, Ordering::Relaxed) {
            write!(self.out, "{CLEAR_LINE}")?;
        }
        writeln!(self.out, "{text}")?;
        if self.live {
            self.redraw_status()?;
        }
        self.out.flush()?;
        Ok(())
    }

    fn finish(&mut self) -> Result<()> {
        if self.live && LIVE_LINE.swap(false, Ordering::Relaxed) {
            writeln!(self.out)?;
        }
        self.out.flush()?;
        Ok(())
    }
}

/// Local wall-clock `HH:MM:SS`.
pub fn clock(ts_unix_ms: u64) -> String {
    i64::try_from(ts_unix_ms)
        .ok()
        .and_then(|ms| chrono::Local.timestamp_millis_opt(ms).single())
        .map(|t| t.format("%H:%M:%S").to_string())
        .unwrap_or_else(|| "--:--:--".to_string())
}

fn unit(kind: ScoreKind) -> &'static str {
    match kind {
        ScoreKind::Rssi => " dB",
        ScoreKind::Csi => "",
    }
}

/// One log line per event; `None` for `Status`, which only feeds the live line.
pub fn format_event(event: &Event) -> Option<String> {
    let line = match event {
        Event::CalibrationStarted {
            link_id,
            ts_unix_ms,
            duration_ms,
        } => {
            let secs = *duration_ms as f64 / 1000.0;
            let bar = "!".repeat(72);
            format!(
                "{bar}\n{} {link_id}: CALIBRATING for the next {secs:.0} s.\n\
                 Put the laptop down and don't move it while sensing; it detects people\n\
                 moving between it and the router, not the laptop itself.\n\
                 STAY STILL or, better, LEAVE THE ROOM until \"calibrated\" is printed.\n\
                 Motion during this time makes the detector less sensitive.\n{bar}",
                clock(*ts_unix_ms)
            )
        }
        Event::Calibrated {
            link_id,
            ts_unix_ms,
            kind,
            baseline,
            threshold,
        } => {
            let u = unit(*kind);
            format!(
                "{} {link_id} calibrated: idle score {:.3} ± {:.3}{u}, threshold {threshold:.3}{u}",
                clock(*ts_unix_ms),
                baseline.mean,
                baseline.std
            )
        }
        Event::MotionStarted {
            link_id,
            ts_unix_ms,
            kind,
            score,
            threshold,
        } => format!(
            "{} {link_id} MOTION started (score {score:.2}{}, thr {threshold:.2})",
            clock(*ts_unix_ms),
            unit(*kind)
        ),
        Event::MotionEnded {
            link_id,
            ts_unix_ms,
            kind,
            duration_ms,
            peak_score,
        } => format!(
            "{} {link_id} motion ended after {:.1} s (peak {peak_score:.2}{})",
            clock(*ts_unix_ms),
            *duration_ms as f64 / 1000.0,
            unit(*kind)
        ),
        Event::ZoneChanged {
            ts_unix_ms,
            zone: Some(zone),
            confidence,
        } => format!(
            "{} zone: {zone} (most disturbed link, lead {confidence:.2})",
            clock(*ts_unix_ms)
        ),
        Event::ZoneChanged {
            ts_unix_ms,
            zone: None,
            ..
        } => format!("{} zone: none (no motion)", clock(*ts_unix_ms)),
        Event::Status { .. } => return None,
    };
    Some(line)
}

/// The live line: one compact segment per link.
fn status_line(status: &BTreeMap<String, StatusView>) -> Option<String> {
    let latest = status.values().map(|s| s.ts_unix_ms).max()?;
    let segments: Vec<String> = status
        .iter()
        .map(|(link, s)| {
            let score = s
                .score
                .map_or_else(|| "--".to_string(), |v| format!("{v:.3}"));
            match (s.state, s.threshold) {
                (MotionState::Calibrating, _) | (_, None) => {
                    format!("{link} calibrating, stay still (score {score})")
                }
                (state, Some(thr)) => {
                    let label = if state == MotionState::Motion {
                        "MOTION"
                    } else {
                        "idle"
                    };
                    format!(
                        "{link} {label:<6} {score}/{thr:.3}{} {}",
                        unit(s.kind),
                        bar(s.score, thr)
                    )
                }
            }
        })
        .collect();
    Some(format!("{} {}", clock(latest), segments.join("  |  ")))
}

/// `[#####|    ]`: score relative to the threshold, which sits in the middle.
fn bar(score: Option<f32>, threshold: f32) -> String {
    const HALF: usize = 8;
    let filled = match score {
        Some(s) if threshold > 0.0 => ((s / threshold) * HALF as f32).round() as usize,
        _ => 0,
    }
    .min(2 * HALF);
    let cells: String = (0..2 * HALF)
        .map(|i| match (i < filled, i == HALF) {
            (true, _) => '#',
            (false, true) => '|',
            (false, false) => ' ',
        })
        .collect();
    format!("[{cells}]")
}

/// Tracing writer. Normally writes to stderr, first clearing the live status
/// line so messages don't get glued onto it. While the TUI runs, messages
/// are captured instead (see [`set_log_capture`]).
pub struct LogWriter;

impl<'a> MakeWriter<'a> for LogWriter {
    type Writer = LogTarget;

    fn make_writer(&'a self) -> Self::Writer {
        if CAPTURE_LOGS.load(Ordering::Relaxed) {
            return LogTarget::Capture(Vec::new());
        }
        let mut stderr = io::stderr();
        if LIVE_LINE.swap(false, Ordering::Relaxed) && stderr.is_terminal() {
            // Best effort: losing the escape only costs a cosmetic glitch.
            let _ = stderr.write_all(CLEAR_LINE.as_bytes());
        }
        LogTarget::Stderr(stderr)
    }
}

pub enum LogTarget {
    Stderr(io::Stderr),
    /// Collects one message; stored in the log buffer when dropped.
    Capture(Vec<u8>),
}

impl Write for LogTarget {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            LogTarget::Stderr(e) => e.write(buf),
            LogTarget::Capture(v) => {
                v.extend_from_slice(buf);
                Ok(buf.len())
            }
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            LogTarget::Stderr(e) => e.flush(),
            LogTarget::Capture(_) => Ok(()),
        }
    }
}

impl Drop for LogTarget {
    fn drop(&mut self) {
        let LogTarget::Capture(bytes) = self else {
            return;
        };
        let text = strip_ansi(&String::from_utf8_lossy(bytes));
        let mut buffer = LOG_BUFFER.lock().unwrap_or_else(|e| e.into_inner());
        for line in text.lines().filter(|l| !l.trim().is_empty()) {
            if buffer.len() == LOG_BUFFER_LINES {
                buffer.pop_front();
            }
            buffer.push_back(line.trim().to_string());
        }
    }
}

/// Route log messages into the buffer (`true`) or back to stderr (`false`).
#[cfg(feature = "tui")]
pub fn set_log_capture(on: bool) {
    CAPTURE_LOGS.store(on, Ordering::Relaxed);
}

/// Take the log messages captured so far.
#[cfg(feature = "tui")]
pub fn take_captured_logs() -> Vec<String> {
    let mut buffer = LOG_BUFFER.lock().unwrap_or_else(|e| e.into_inner());
    buffer.drain(..).collect()
}

/// Remove terminal colour codes (`ESC [ ... letter`).
fn strip_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' && chars.peek() == Some(&'[') {
            for c in chars.by_ref() {
                if c.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::detector::Baseline;

    #[test]
    fn formats_motion_events() {
        let started = Event::MotionStarted {
            link_id: "wlp2s0".into(),
            ts_unix_ms: 0,
            kind: ScoreKind::Rssi,
            score: 2.8,
            threshold: 1.1,
        };
        let line = format_event(&started).unwrap();
        assert!(
            line.ends_with(" wlp2s0 MOTION started (score 2.80 dB, thr 1.10)"),
            "{line}"
        );
        let ended = Event::MotionEnded {
            link_id: "kitchen".into(),
            ts_unix_ms: 0,
            kind: ScoreKind::Csi,
            duration_ms: 8400,
            peak_score: 3.4,
        };
        let line = format_event(&ended).unwrap();
        assert!(
            line.ends_with(" kitchen motion ended after 8.4 s (peak 3.40)"),
            "{line}"
        );
        let calibrated = Event::Calibrated {
            link_id: "wlp2s0".into(),
            ts_unix_ms: 0,
            kind: ScoreKind::Rssi,
            baseline: Baseline {
                mean: 0.12,
                std: 0.04,
            },
            threshold: 0.28,
        };
        assert!(format_event(&calibrated)
            .unwrap()
            .contains("idle score 0.120 ± 0.040 dB, threshold 0.280 dB"));
    }

    #[test]
    fn non_terminal_output_has_events_only() {
        let mut sink = HumanSink::new(Vec::new(), false);
        let status = Event::Status {
            link_id: "wlp2s0".into(),
            ts_unix_ms: 0,
            kind: ScoreKind::Rssi,
            score: Some(0.1),
            threshold: Some(0.3),
            state: MotionState::Idle,
        };
        sink.handle(&status).unwrap();
        sink.handle(&Event::MotionEnded {
            link_id: "wlp2s0".into(),
            ts_unix_ms: 0,
            kind: ScoreKind::Rssi,
            duration_ms: 1000,
            peak_score: 1.0,
        })
        .unwrap();
        sink.finish().unwrap();
        let text = String::from_utf8(sink.out).unwrap();
        assert_eq!(text.lines().count(), 1, "{text:?}");
        assert!(!text.contains('\x1b'));
    }

    #[test]
    fn json_lines_are_tagged_objects() {
        let mut sink = JsonSink::new(Vec::new());
        sink.handle(&Event::MotionStarted {
            link_id: "wlp2s0".into(),
            ts_unix_ms: 1_700_000_000_000,
            kind: ScoreKind::Rssi,
            score: 2.5,
            threshold: 1.25,
        })
        .unwrap();
        sink.handle(&Event::Status {
            link_id: "wlp2s0".into(),
            ts_unix_ms: 1_700_000_000_500,
            kind: ScoreKind::Rssi,
            score: None,
            threshold: Some(1.25),
            state: MotionState::Idle,
        })
        .unwrap();
        sink.handle(&Event::ZoneChanged {
            ts_unix_ms: 1_700_000_001_000,
            zone: Some("kitchen".into()),
            confidence: 0.5,
        })
        .unwrap();
        let text = String::from_utf8(sink.out).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(
            lines,
            [
                r#"{"event":"MotionStarted","link_id":"wlp2s0","ts_unix_ms":1700000000000,"kind":"rssi","score":2.5,"threshold":1.25}"#,
                r#"{"event":"Status","link_id":"wlp2s0","ts_unix_ms":1700000000500,"kind":"rssi","score":null,"threshold":1.25,"state":"idle"}"#,
                r#"{"event":"ZoneChanged","ts_unix_ms":1700000001000,"zone":"kitchen","confidence":0.5}"#,
            ]
        );
    }

    #[test]
    fn formats_zone_changes() {
        let set = Event::ZoneChanged {
            ts_unix_ms: 0,
            zone: Some("hallway".into()),
            confidence: 1.234,
        };
        assert!(format_event(&set)
            .unwrap()
            .ends_with(" zone: hallway (most disturbed link, lead 1.23)"));
        let cleared = Event::ZoneChanged {
            ts_unix_ms: 0,
            zone: None,
            confidence: 0.0,
        };
        assert!(format_event(&cleared)
            .unwrap()
            .ends_with(" zone: none (no motion)"));
    }

    #[test]
    fn strips_colour_codes() {
        assert_eq!(strip_ansi("\x1b[33m WARN\x1b[0m stale"), " WARN stale");
    }

    #[test]
    fn bar_puts_threshold_in_the_middle() {
        assert_eq!(bar(Some(0.0), 1.0), "[        |       ]");
        assert_eq!(bar(Some(0.5), 1.0), "[####    |       ]");
        assert_eq!(bar(Some(1.0), 1.0), "[########|       ]");
        assert_eq!(bar(Some(9.0), 1.0), "[################]");
        assert_eq!(bar(None, 1.0), "[        |       ]");
    }
}
