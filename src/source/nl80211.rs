//! RSSI via nl80211 (`NL80211_CMD_GET_STATION`): the station entry of the
//! access point we're connected to, read at the poll rate.
//!
//! Compared with `/proc/net/wireless` this also gives the averaged signal,
//! per-antenna (chain) signals and the received-packet counter, a real
//! "has anything arrived" counter. Drivers that don't report station signal
//! through nl80211 (Broadcom's `wl`, for one) are detected at start-up with a
//! hint to use the procfs source instead.
//!
//! Sampling policy is the same as procfs: one sample per poll. Staleness is
//! judged on the packet counter: no new packets for `STALE_AFTER` means the
//! RSSI cannot have been refreshed.

use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use async_trait::async_trait;
use futures_util::TryStreamExt;
use tokio::time::{Interval, MissedTickBehavior};
use wl_nl80211::{Nl80211Attr, Nl80211Handle, Nl80211StationInfo};

use super::procfs::{Staleness, StalenessTracker, STALE_AFTER};
use super::SampleSource;
use crate::sample::{unix_ms_now, Sample};

/// How often the extra station details are logged (debug level).
const DETAIL_LOG_EVERY: Duration = Duration::from_secs(10);

/// What we read from one station entry.
#[derive(Debug, Clone, PartialEq)]
pub struct StationReading {
    /// Signal of the last received frame, dBm.
    pub signal: i8,
    pub signal_avg: Option<i8>,
    pub chain_signal: Vec<i8>,
    pub rx_packets: Option<u32>,
}

/// Pick the station info out of one GET_STATION reply.
pub fn station_reading(attrs: &[Nl80211Attr]) -> Option<StationReading> {
    let info = attrs.iter().find_map(|a| match a {
        Nl80211Attr::StationInfo(info) => Some(info),
        _ => None,
    })?;
    let mut reading = StationReading {
        signal: 0,
        signal_avg: None,
        chain_signal: Vec::new(),
        rx_packets: None,
    };
    let mut have_signal = false;
    for item in info {
        match item {
            Nl80211StationInfo::Signal(s) => {
                reading.signal = *s;
                have_signal = true;
            }
            Nl80211StationInfo::SignalAvg(s) => reading.signal_avg = Some(*s),
            Nl80211StationInfo::ChainSignal(c) => reading.chain_signal = decode_chain_signal(c),
            Nl80211StationInfo::RxPackets(p) => reading.rx_packets = Some(*p),
            _ => {}
        }
    }
    have_signal.then_some(reading)
}

pub struct Nl80211Config {
    pub iface: String,
    pub poll_hz: f32,
    pub keepalive: bool,
}

pub struct Nl80211Source {
    cfg: Nl80211Config,
    if_index: u32,
    handle: Nl80211Handle,
    ticker: Option<Interval>,
    stale: StalenessTracker<u32>,
    last_detail_log: Option<Instant>,
}

impl Nl80211Source {
    /// Connect and check once that the driver reports a signal.
    pub async fn open(cfg: Nl80211Config) -> Result<Self> {
        if !(cfg.poll_hz.is_finite() && cfg.poll_hz > 0.0 && cfg.poll_hz <= 1000.0) {
            bail!(
                "poll rate must be between 0 and 1000 Hz, got {}",
                cfg.poll_hz
            );
        }
        let if_index = if_index(&cfg.iface)?;
        let (connection, handle, _) =
            wl_nl80211::new_connection().context("opening an nl80211 netlink socket")?;
        tokio::spawn(connection);
        let mut source = Self {
            cfg,
            if_index,
            handle,
            ticker: None,
            stale: StalenessTracker::new(STALE_AFTER),
            last_detail_log: None,
        };
        match source.query().await {
            Ok(Some(_)) => Ok(source),
            Ok(None) => bail!(
                "nl80211 reports no station signal for {} (not connected, or the driver \
                 doesn't support it, as with Broadcom's wl); use --rssi-source procfs",
                source.cfg.iface
            ),
            Err(e) => Err(e.context(format!(
                "nl80211 GET_STATION failed on {}; use --rssi-source procfs",
                source.cfg.iface
            ))),
        }
    }

    async fn query(&mut self) -> Result<Option<StationReading>> {
        let mut replies = self.handle.station().dump(self.if_index).execute().await;
        let mut found = None;
        while let Some(msg) = replies
            .try_next()
            .await
            .map_err(|e| anyhow!("nl80211: {e}"))?
        {
            // In station (client) mode the only entry is the access point.
            if found.is_none() {
                found = station_reading(&msg.payload.attributes);
            }
        }
        Ok(found)
    }

    fn log_details(&mut self, r: &StationReading, now: Instant) {
        if self
            .last_detail_log
            .is_some_and(|t| now.duration_since(t) < DETAIL_LOG_EVERY)
        {
            return;
        }
        self.last_detail_log = Some(now);
        tracing::debug!(
            "{}: signal {} dBm, avg {:?}, chains {:?}, rx packets {:?}",
            self.cfg.iface,
            r.signal,
            r.signal_avg,
            r.chain_signal,
            r.rx_packets
        );
    }
}

/// wl-nl80211 0.8 hands `NL80211_STA_INFO_CHAIN_SIGNAL` over as the raw bytes
/// of its nested attributes (`len u16, type u16, value u8, padding` per
/// chain) instead of the values. Decode them; if the bytes don't parse as
/// nested attributes (e.g. a fixed crate version), they are already values.
pub fn decode_chain_signal(raw: &[i8]) -> Vec<i8> {
    let bytes: Vec<u8> = raw.iter().map(|b| b.to_ne_bytes()[0]).collect();
    let mut values = Vec::new();
    let mut rest = bytes.as_slice();
    while !rest.is_empty() {
        let [l0, l1, _, _, ..] = *rest else {
            return raw.to_vec();
        };
        let len = usize::from(u16::from_le_bytes([l0, l1]));
        let Some(&value) = rest.get(4).filter(|_| len == 5 && len <= rest.len()) else {
            return raw.to_vec();
        };
        values.push(i8::from_ne_bytes([value]));
        let padded = (len + 3) & !3;
        rest = rest.get(padded..).unwrap_or_default();
    }
    if values.is_empty() {
        raw.to_vec()
    } else {
        values
    }
}

fn if_index(iface: &str) -> Result<u32> {
    let path = Path::new("/sys/class/net").join(iface).join("ifindex");
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("reading {} (does {iface} exist?)", path.display()))?;
    text.trim()
        .parse()
        .with_context(|| format!("bad interface index in {}", path.display()))
}

#[async_trait]
impl SampleSource for Nl80211Source {
    fn link_id(&self) -> &str {
        &self.cfg.iface
    }

    async fn next(&mut self) -> Result<Option<Sample>> {
        loop {
            let period = Duration::from_secs_f64(1.0 / f64::from(self.cfg.poll_hz));
            let ticker = self.ticker.get_or_insert_with(|| {
                let mut t = tokio::time::interval(period);
                t.set_missed_tick_behavior(MissedTickBehavior::Skip);
                t
            });
            ticker.tick().await;
            let Some(reading) = self.query().await? else {
                // Briefly disconnected; procfs logs this case, keep quiet here.
                continue;
            };
            let now = Instant::now();
            if let Some(packets) = reading.rx_packets {
                match self.stale.observe(packets, now) {
                    Some(Staleness::BecameStale(d)) => tracing::warn!(
                        "{}: no packets received for {:.1} s{}, so RSSI isn't updating",
                        self.cfg.iface,
                        d.as_secs_f64(),
                        if self.cfg.keepalive {
                            " although keepalive pings are running"
                        } else {
                            " (keepalive is off)"
                        }
                    ),
                    Some(Staleness::Recovered(_)) => {
                        tracing::info!("{}: packets arriving again", self.cfg.iface)
                    }
                    None => {}
                }
            }
            self.log_details(&reading, now);
            return Ok(Some(Sample::rssi(
                self.cfg.iface.clone(),
                now,
                unix_ms_now(),
                f32::from(reading.signal),
            )));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_signal_fields_from_station_info() {
        let attrs = vec![
            Nl80211Attr::IfIndex(3),
            Nl80211Attr::StationInfo(vec![
                Nl80211StationInfo::RxPackets(48_945),
                Nl80211StationInfo::Signal(-56),
                Nl80211StationInfo::SignalAvg(-55),
                // As wl-nl80211 0.8 delivers it on this laptop: two nested
                // attributes (chain 0: -60 dBm, chain 1: -56 dBm).
                Nl80211StationInfo::ChainSignal(vec![
                    5, 0, 0, 0, -60, 0, 0, 0, 5, 0, 1, 0, -56, 0, 0, 0,
                ]),
            ]),
        ];
        assert_eq!(
            station_reading(&attrs),
            Some(StationReading {
                signal: -56,
                signal_avg: Some(-55),
                chain_signal: vec![-60, -56],
                rx_packets: Some(48_945),
            })
        );
    }

    #[test]
    fn chain_signal_decoding() {
        assert_eq!(decode_chain_signal(&[5, 0, 0, 0, -54, 0, 0, 0]), vec![-54]);
        // Already plain values: left alone.
        assert_eq!(decode_chain_signal(&[-60, -56]), vec![-60, -56]);
        assert_eq!(
            decode_chain_signal(&[-60, -56, -58, -57]),
            vec![-60, -56, -58, -57]
        );
        assert_eq!(decode_chain_signal(&[]), Vec::<i8>::new());
        // Truncated nested attribute: left alone rather than misread.
        assert_eq!(decode_chain_signal(&[5, 0, 0, 0]), vec![5, 0, 0, 0]);
    }

    #[test]
    fn no_signal_means_unsupported() {
        let attrs = vec![Nl80211Attr::StationInfo(vec![
            Nl80211StationInfo::RxPackets(1),
        ])];
        assert_eq!(station_reading(&attrs), None);
        assert_eq!(station_reading(&[Nl80211Attr::IfIndex(3)]), None);
    }
}
