//! RSSI from `/proc/net/wireless`.
//!
//! Sampling policy: a sample is emitted on *every* poll, even when the value
//! hasn't changed. `/proc/net/wireless` has no per-frame counter, so "same
//! value" can't be told apart from "no new frame"; dropping repeats would
//! throw away real "nothing changed" readings and bias the scores upwards.
//! Polling at a fixed rate gives the detector a uniformly sampled
//! (zero-order-hold) series, so calibration and monitoring see the same
//! statistics as long as the poll rate stays the same. Staleness is tracked
//! separately: if the value doesn't change for [`STALE_AFTER`] we warn.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{bail, Context};
use async_trait::async_trait;
use thiserror::Error;
use tokio::time::{Interval, MissedTickBehavior};

use super::SampleSource;
use crate::sample::{unix_ms_now, Sample};

pub const WIRELESS_PATH: &str = "/proc/net/wireless";
/// Warn when the value hasn't changed for this long.
pub const STALE_AFTER: Duration = Duration::from_secs(3);
/// How often the value-change rate is logged (at debug level).
const RATE_LOG_EVERY: Duration = Duration::from_secs(10);
/// Signal levels at or below this are "no data" markers (e.g. -256).
const NO_SIGNAL_DBM: f32 = -200.0;

#[derive(Debug, Error, PartialEq)]
pub enum WirelessError {
    #[error("interface {0} is not listed in /proc/net/wireless (not wireless, or not connected)")]
    MissingInterface(String),
    #[error("malformed /proc/net/wireless line {line:?}: {reason}")]
    Malformed { line: String, reason: String },
}

/// One data line of `/proc/net/wireless`.
#[derive(Debug, Clone, PartialEq)]
pub struct WirelessEntry {
    pub iface: String,
    pub status: u16,
    pub link_quality: f32,
    pub level: f32,
    pub noise: f32,
}

/// The signal level, interpreted.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Signal {
    Dbm(f32),
    /// Driver-specific positive scale (0-100 or 0-255). Fine for variance
    /// based detection, which doesn't care about the unit.
    Relative(f32),
}

impl Signal {
    pub fn value(self) -> f32 {
        match self {
            Signal::Dbm(v) | Signal::Relative(v) => v,
        }
    }
}

impl WirelessEntry {
    /// Noise level in dBm, if the driver reports one.
    pub fn noise_dbm(&self) -> Option<f32> {
        (self.noise < 0.0 && self.noise > NO_SIGNAL_DBM).then_some(self.noise)
    }

    /// `None` when the driver reports no usable level.
    pub fn signal(&self) -> Option<Signal> {
        if self.level < 0.0 {
            (self.level > NO_SIGNAL_DBM).then_some(Signal::Dbm(self.level))
        } else if self.level > 0.0 {
            Some(Signal::Relative(self.level))
        } else {
            None
        }
    }
}

/// Parse one data line, e.g. `wlp2s0: 0000   54.  -56.  -256  0 0 0 0 0  0`.
/// Columns after the name: status (hex), link quality, signal level, noise
/// level, then discard/missed counters we don't need. A trailing `.` marks
/// a value as updated; it is stripped.
pub fn parse_line(line: &str) -> Result<WirelessEntry, WirelessError> {
    let malformed = |reason: &str| WirelessError::Malformed {
        line: line.to_string(),
        reason: reason.to_string(),
    };
    let (iface, rest) = line.split_once(':').ok_or_else(|| malformed("no ':'"))?;
    let iface = iface.trim();
    if iface.is_empty() {
        return Err(malformed("empty interface name"));
    }
    let mut fields = rest.split_whitespace().map(|f| f.trim_end_matches('.'));
    let mut next = |name: &str| {
        fields
            .next()
            .ok_or_else(|| malformed(&format!("missing {name}")))
    };
    let status = next("status")?;
    let quality = next("link quality")?;
    let level = next("signal level")?;
    let noise = next("noise level")?;
    let number = |name: &str, s: &str| {
        s.parse::<f32>()
            .map_err(|_| malformed(&format!("bad {name} {s:?}")))
    };
    Ok(WirelessEntry {
        iface: iface.to_string(),
        status: u16::from_str_radix(status, 16)
            .map_err(|_| malformed(&format!("bad status {status:?}")))?,
        link_quality: number("link quality", quality)?,
        level: number("signal level", level)?,
        noise: number("noise level", noise)?,
    })
}

/// Every data line of the file, skipping the two header lines.
pub fn parse_wireless(text: &str) -> Vec<Result<WirelessEntry, WirelessError>> {
    text.lines()
        .skip(2)
        .filter(|l| !l.trim().is_empty())
        .map(parse_line)
        .collect()
}

/// The entry for `iface`.
pub fn lookup(text: &str, iface: &str) -> Result<WirelessEntry, WirelessError> {
    text.lines()
        .skip(2)
        .find(|l| {
            l.split_once(':')
                .is_some_and(|(name, _)| name.trim() == iface)
        })
        .ok_or_else(|| WirelessError::MissingInterface(iface.to_string()))
        .and_then(parse_line)
}

/// Read and parse `/proc/net/wireless`.
pub fn read_entries(path: &Path) -> anyhow::Result<Vec<WirelessEntry>> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let mut entries = Vec::new();
    for entry in parse_wireless(&text) {
        match entry {
            Ok(e) => entries.push(e),
            Err(e) => tracing::warn!("{e}"),
        }
    }
    Ok(entries)
}

/// Wireless interfaces the kernel knows about, connected or not.
pub fn sysfs_wireless_ifaces() -> BTreeSet<String> {
    let Ok(dir) = std::fs::read_dir("/sys/class/net") else {
        return BTreeSet::new();
    };
    dir.filter_map(Result::ok)
        .filter(|e| e.path().join("wireless").exists() || e.path().join("phy80211").exists())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect()
}

/// `iface` if given and connected, otherwise the first interface listed.
pub fn resolve_iface(path: &Path, iface: Option<&str>) -> anyhow::Result<WirelessEntry> {
    let entries = read_entries(path)?;
    match iface {
        Some(name) => entries
            .into_iter()
            .find(|e| e.iface == name)
            .with_context(|| {
                if sysfs_wireless_ifaces().contains(name) {
                    format!(
                        "{name} is a wireless interface but is not connected to an access point"
                    )
                } else {
                    format!(
                        "{name} is not listed in {}; see `wisense list-ifaces`",
                        path.display()
                    )
                }
            }),
        None => match entries.into_iter().next() {
            Some(e) => Ok(e),
            None => bail!(
                "no connected wireless interface in {}; connect to your WiFi first \
                 (`wisense list-ifaces` shows what was found)",
                path.display()
            ),
        },
    }
}

/// What [`StalenessTracker::observe`] noticed.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Staleness {
    /// The value hasn't changed for this long.
    BecameStale(Duration),
    /// The value changed again after being stale for this long.
    Recovered(Duration),
}

/// Notices when a value stops changing. Pure: time is passed in.
#[derive(Debug, Clone)]
pub struct StalenessTracker<T = f32> {
    after: Duration,
    last: Option<(T, Instant)>,
    stale: bool,
}

impl<T: PartialEq + Copy> StalenessTracker<T> {
    pub fn new(after: Duration) -> Self {
        Self {
            after,
            last: None,
            stale: false,
        }
    }

    /// Reports each stale episode once, and its end once.
    pub fn observe(&mut self, value: T, now: Instant) -> Option<Staleness> {
        match self.last {
            Some((last, since)) if last == value => {
                let unchanged = now.duration_since(since);
                if !self.stale && unchanged > self.after {
                    self.stale = true;
                    return Some(Staleness::BecameStale(unchanged));
                }
                None
            }
            Some((_, since)) => {
                self.last = Some((value, now));
                if self.stale {
                    self.stale = false;
                    return Some(Staleness::Recovered(now.duration_since(since)));
                }
                None
            }
            None => {
                self.last = Some((value, now));
                None
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct ProcfsConfig {
    pub iface: String,
    pub poll_hz: f32,
    pub path: PathBuf,
    /// Whether keepalive traffic is running (changes the staleness hint).
    pub keepalive: bool,
}

/// Polls `/proc/net/wireless` and emits one RSSI sample per poll.
pub struct ProcfsSource {
    cfg: ProcfsConfig,
    ticker: Option<Interval>,
    stale: StalenessTracker,
    warned_relative: bool,
    missing: bool,
    bad_reads: u64,
    prev_value: Option<f32>,
    changes: u32,
    rate_since: Option<Instant>,
}

impl ProcfsSource {
    pub fn new(cfg: ProcfsConfig) -> anyhow::Result<Self> {
        if !(cfg.poll_hz.is_finite() && cfg.poll_hz > 0.0 && cfg.poll_hz <= 1000.0) {
            bail!(
                "poll rate must be between 0 and 1000 Hz, got {}",
                cfg.poll_hz
            );
        }
        Ok(Self {
            cfg,
            ticker: None,
            stale: StalenessTracker::new(STALE_AFTER),
            warned_relative: false,
            missing: false,
            bad_reads: 0,
            prev_value: None,
            changes: 0,
            rate_since: None,
        })
    }

    /// Bookkeeping for a new reading: staleness warnings and the debug log of
    /// how often the value actually changes.
    fn track(&mut self, value: f32, now: Instant) {
        let iface = &self.cfg.iface;
        match self.stale.observe(value, now) {
            Some(Staleness::BecameStale(d)) if self.cfg.keepalive => tracing::warn!(
                "{iface}: RSSI hasn't changed for {:.1} s although keepalive pings are \
                 running; the driver may not be updating /proc/net/wireless (or the link is \
                 unusually stable)",
                d.as_secs_f64()
            ),
            Some(Staleness::BecameStale(d)) => tracing::warn!(
                "{iface}: RSSI hasn't changed for {:.1} s; without keepalive traffic it only \
                 updates when frames arrive (drop --no-keepalive)",
                d.as_secs_f64()
            ),
            Some(Staleness::Recovered(d)) => tracing::info!(
                "{iface}: RSSI is updating again after {:.1} s",
                d.as_secs_f64()
            ),
            None => {}
        }

        if self.prev_value.is_some_and(|p| p != value) {
            self.changes += 1;
        }
        self.prev_value = Some(value);
        let since = *self.rate_since.get_or_insert(now);
        let elapsed = now.duration_since(since);
        if elapsed >= RATE_LOG_EVERY {
            tracing::debug!(
                "{iface}: RSSI changed {} times in {:.1} s ({:.2}/s), now {value}",
                self.changes,
                elapsed.as_secs_f64(),
                f64::from(self.changes) / elapsed.as_secs_f64()
            );
            self.changes = 0;
            self.rate_since = Some(now);
        }
    }
}

#[async_trait]
impl SampleSource for ProcfsSource {
    fn link_id(&self) -> &str {
        &self.cfg.iface
    }

    async fn next(&mut self) -> anyhow::Result<Option<Sample>> {
        loop {
            let period = Duration::from_secs_f64(1.0 / f64::from(self.cfg.poll_hz));
            let ticker = self.ticker.get_or_insert_with(|| {
                let mut t = tokio::time::interval(period);
                t.set_missed_tick_behavior(MissedTickBehavior::Skip);
                t
            });
            ticker.tick().await;

            let text = tokio::fs::read_to_string(&self.cfg.path)
                .await
                .with_context(|| format!("reading {}", self.cfg.path.display()))?;
            let now = Instant::now();
            let entry = match lookup(&text, &self.cfg.iface) {
                Ok(entry) => entry,
                Err(WirelessError::MissingInterface(_)) => {
                    if !self.missing {
                        self.missing = true;
                        tracing::warn!(
                            "{} disappeared from {} (disconnected?); waiting for it to return",
                            self.cfg.iface,
                            self.cfg.path.display()
                        );
                    }
                    continue;
                }
                Err(e) => {
                    // Don't give up on one odd read, but don't spam either.
                    self.bad_reads += 1;
                    if self.bad_reads == 1 || self.bad_reads.is_multiple_of(1000) {
                        tracing::warn!("{e} ({} bad reads so far)", self.bad_reads);
                    }
                    continue;
                }
            };
            if self.missing {
                self.missing = false;
                tracing::info!("{} is back", self.cfg.iface);
            }
            let Some(signal) = entry.signal() else {
                continue;
            };
            if let (Signal::Relative(v), false) = (signal, self.warned_relative) {
                self.warned_relative = true;
                tracing::warn!(
                    "{}: driver reports the signal level as {v} on a relative scale, not dBm; \
                     using it anyway (detection only looks at fluctuations)",
                    self.cfg.iface
                );
            }
            let value = signal.value();
            self.track(value, now);
            return Ok(Some(Sample::rssi(
                self.cfg.iface.clone(),
                now,
                unix_ms_now(),
                value,
            )));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HEADER: &str = "Inter-| sta-|   Quality        |   Discarded packets               | Missed | WE\n \
                          face | tus | link level noise |  nwid  crypt   frag  retry   misc | beacon | 22\n";

    #[test]
    fn parses_normal_line_with_trailing_dots() {
        let text = format!(
            "{HEADER}wlp2s0: 0000   62.  -48.  -256        0      0      0      0     32        0\n"
        );
        let entries = parse_wireless(&text);
        assert_eq!(
            entries,
            vec![Ok(WirelessEntry {
                iface: "wlp2s0".into(),
                status: 0,
                link_quality: 62.0,
                level: -48.0,
                noise: -256.0,
            })]
        );
        assert_eq!(
            entries[0].as_ref().unwrap().signal(),
            Some(Signal::Dbm(-48.0))
        );
        assert_eq!(entries[0].as_ref().unwrap().noise_dbm(), None);
    }

    #[test]
    fn parses_without_dots_and_multiple_interfaces() {
        let text = format!(
            "{HEADER}wlan0: 0001   54   -56   -95  0 0 0 0 0 0\n\
             wlx00c0ca: 0000   70.  -40.  -256  0 0 0 0 0 0\n"
        );
        assert_eq!(lookup(&text, "wlan0").unwrap().level, -56.0);
        assert_eq!(lookup(&text, "wlan0").unwrap().status, 1);
        assert_eq!(lookup(&text, "wlan0").unwrap().noise_dbm(), Some(-95.0));
        assert_eq!(lookup(&text, "wlx00c0ca").unwrap().link_quality, 70.0);
    }

    #[test]
    fn positive_signal_is_relative_scale() {
        let text = format!("{HEADER}eth1: 0000   70.  187.  0  0 0 0 0 0 0\n");
        let e = lookup(&text, "eth1").unwrap();
        assert_eq!(e.signal(), Some(Signal::Relative(187.0)));
        assert_eq!(e.signal().map(Signal::value), Some(187.0));
    }

    #[test]
    fn no_data_levels() {
        let text = format!("{HEADER}a: 0000 0 -256 -256 0 0 0 0 0 0\nb: 0000 0 0 0 0 0 0 0 0 0\n");
        assert_eq!(lookup(&text, "a").unwrap().signal(), None);
        assert_eq!(lookup(&text, "b").unwrap().signal(), None);
    }

    #[test]
    fn missing_interface() {
        let text = format!("{HEADER}wlp2s0: 0000   62.  -48.  -256  0 0 0 0 0 0\n");
        assert_eq!(
            lookup(&text, "wlan1"),
            Err(WirelessError::MissingInterface("wlan1".into()))
        );
        // Empty file (no wireless interface connected).
        assert_eq!(
            lookup(HEADER, "wlp2s0"),
            Err(WirelessError::MissingInterface("wlp2s0".into()))
        );
        assert!(parse_wireless(HEADER).is_empty());
    }

    #[test]
    fn malformed_lines() {
        assert!(matches!(
            parse_line("wlp2s0: 0000 62."),
            Err(WirelessError::Malformed { .. })
        ));
        assert!(matches!(
            parse_line("wlp2s0: zzzz 62. -48. -256"),
            Err(WirelessError::Malformed { .. })
        ));
        assert!(matches!(
            parse_line("wlp2s0: 0000 62. abc. -256"),
            Err(WirelessError::Malformed { .. })
        ));
        assert!(matches!(
            parse_line("no colon here"),
            Err(WirelessError::Malformed { .. })
        ));
        // A bad line for another interface doesn't hide ours.
        let text = format!("{HEADER}junk: x\nwlp2s0: 0000 62. -48. -256 0 0 0 0 0 0\n");
        assert_eq!(lookup(&text, "wlp2s0").unwrap().level, -48.0);
    }

    #[test]
    fn staleness_warns_once_per_episode() {
        let t0 = Instant::now();
        let at = |ms: u64| t0 + Duration::from_millis(ms);
        let mut s = StalenessTracker::new(STALE_AFTER);
        assert_eq!(s.observe(-50.0, at(0)), None);
        assert_eq!(s.observe(-50.0, at(2000)), None);
        assert_eq!(s.observe(-51.0, at(2500)), None);
        // Unchanged since 2.5 s: stale after 3 s, reported once.
        assert_eq!(s.observe(-51.0, at(5500)), None);
        assert_eq!(
            s.observe(-51.0, at(5550)),
            Some(Staleness::BecameStale(Duration::from_millis(3050)))
        );
        assert_eq!(s.observe(-51.0, at(9000)), None);
        assert_eq!(
            s.observe(-49.0, at(9500)),
            Some(Staleness::Recovered(Duration::from_millis(7000)))
        );
        assert_eq!(s.observe(-49.0, at(9550)), None);
    }

    fn temp_file(name: &str, contents: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!("wisense-{}-{name}", std::process::id()));
        std::fs::write(&path, contents).unwrap();
        path
    }

    #[tokio::test]
    async fn emits_every_poll_even_when_unchanged() {
        let path = temp_file(
            "wireless",
            &format!("{HEADER}wlp2s0: 0000   62.  -48.  -256  0 0 0 0 0 0\n"),
        );
        let mut src = ProcfsSource::new(ProcfsConfig {
            iface: "wlp2s0".into(),
            poll_hz: 200.0,
            path: path.clone(),
            keepalive: true,
        })
        .unwrap();
        let mut samples = Vec::new();
        for _ in 0..5 {
            samples.push(src.next().await.unwrap().unwrap());
        }
        std::fs::write(
            &path,
            format!("{HEADER}wlp2s0: 0000   60.  -51.  -256  0 0 0 0 0 0\n"),
        )
        .unwrap();
        samples.push(src.next().await.unwrap().unwrap());
        std::fs::remove_file(&path).unwrap();

        let values: Vec<_> = samples.iter().map(|s| s.rssi_dbm).collect();
        assert_eq!(values[..5], [Some(-48.0); 5]);
        assert_eq!(values[5], Some(-51.0));
        assert!(samples.iter().all(|s| s.link_id == "wlp2s0"));
        assert!(samples.windows(2).all(|w| w[1].t > w[0].t));
    }

    #[test]
    fn rejects_bad_poll_rate() {
        for hz in [0.0, -1.0, f32::NAN, 5000.0] {
            let cfg = ProcfsConfig {
                iface: "x".into(),
                poll_hz: hz,
                path: PathBuf::from(WIRELESS_PATH),
                keepalive: false,
            };
            assert!(ProcfsSource::new(cfg).is_err(), "{hz}");
        }
    }
}
