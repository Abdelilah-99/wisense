//! The wisense bridge: reads `wisense monitor --json` lines on stdin and
//! publishes them on `homeguard/wisense/<link_id>`.
//!
//! ```text
//! wisense monitor --json | homeguard-wisense-bridge --zone living-room --link wlp2s0
//! ```
//!
//! | wisense event | Published |
//! |---|---|
//! | `MotionStarted` | `motion` |
//! | `MotionEnded` | `motion_ended` (logged by homeguard, not an alarm input) |
//! | `ZoneChanged` with a zone | `motion` on the link named by the zone |
//! | `Status`, `CalibrationStarted`, `Calibrated` | `heartbeat`, at most every 10 s per link |
//! | no line for 30 s, or end of input | `offline` for every link seen |
//!
//! [`Bridge`] is pure (time comes in as a parameter); [`run`] does the I/O.

use std::collections::BTreeMap;
use std::time::Duration;

use anyhow::Context;
use serde::Deserialize;
use tokio::io::{AsyncBufReadExt, BufReader};
use tracing::{debug, info, warn};

use crate::core::Ts;
use crate::mqtt::{self, Connection};
use crate::parse::wisense::WisenseMessage;
use crate::parse::{ParseError, WISENSE_PREFIX};
use crate::publish::Outgoing;

/// The subset of wisense's `--json` output the bridge reads. Fields it
/// does not need are ignored, and unknown event types are skipped, so newer
/// wisense versions keep working.
#[derive(Debug, Deserialize)]
#[serde(tag = "event")]
enum WisenseLine {
    MotionStarted {
        link_id: String,
        #[serde(default)]
        ts_unix_ms: Option<u64>,
        #[serde(default)]
        score: Option<f32>,
    },
    MotionEnded {
        link_id: String,
        #[serde(default)]
        ts_unix_ms: Option<u64>,
        #[serde(default)]
        duration_ms: Option<u64>,
    },
    Status {
        link_id: String,
    },
    CalibrationStarted {
        link_id: String,
    },
    Calibrated {
        link_id: String,
    },
    ZoneChanged {
        #[serde(default)]
        zone: Option<String>,
        #[serde(default)]
        ts_unix_ms: Option<u64>,
    },
    #[serde(other)]
    Other,
}

#[derive(Debug, Clone)]
pub struct Bridge {
    zone: String,
    heartbeat_every: Duration,
    /// Every link seen, with when its last heartbeat was sent.
    links: BTreeMap<String, Option<Ts>>,
    /// Offline was published and no line has arrived since.
    silent: bool,
}

impl Bridge {
    pub fn new(zone: impl Into<String>, heartbeat_every: Duration) -> Self {
        Bridge {
            zone: zone.into(),
            heartbeat_every,
            links: BTreeMap::new(),
            silent: false,
        }
    }

    /// Links seen so far.
    pub fn links(&self) -> impl Iterator<Item = &str> {
        self.links.keys().map(String::as_str)
    }

    /// One line of wisense output → messages to publish, as
    /// `(link_id, message)`.
    pub fn on_line(
        &mut self,
        at: Ts,
        line: &str,
    ) -> Result<Vec<(String, WisenseMessage)>, ParseError> {
        let line = line.trim();
        if line.is_empty() {
            return Ok(Vec::new());
        }
        let parsed: WisenseLine =
            serde_json::from_str(line).map_err(|e| ParseError::Json(e.to_string()))?;
        let was_silent = std::mem::take(&mut self.silent);
        let mut out = Vec::new();
        let zone = self.zone.clone();
        match parsed {
            WisenseLine::MotionStarted {
                link_id,
                ts_unix_ms,
                score,
            } => {
                self.seen(&link_id);
                out.push((
                    link_id,
                    WisenseMessage::Motion {
                        zone,
                        score,
                        ts_unix_ms,
                    },
                ));
            }
            WisenseLine::MotionEnded {
                link_id,
                ts_unix_ms,
                duration_ms,
            } => {
                self.seen(&link_id);
                out.push((
                    link_id,
                    WisenseMessage::MotionEnded {
                        zone,
                        duration_ms,
                        ts_unix_ms,
                    },
                ));
            }
            WisenseLine::Status { link_id }
            | WisenseLine::CalibrationStarted { link_id }
            | WisenseLine::Calibrated { link_id } => {
                self.seen(&link_id);
                // Coming back from silence: say so at once.
                if was_silent {
                    self.links.insert(link_id.clone(), None);
                }
                out.extend(self.heartbeat(at, &link_id));
            }
            WisenseLine::ZoneChanged {
                zone: Some(link),
                ts_unix_ms,
            } => {
                self.seen(&link);
                out.push((
                    link,
                    WisenseMessage::Motion {
                        zone,
                        score: None,
                        ts_unix_ms,
                    },
                ));
            }
            WisenseLine::ZoneChanged { zone: None, .. } | WisenseLine::Other => {}
        }
        Ok(out)
    }

    fn seen(&mut self, link: &str) {
        self.links.entry(link.to_string()).or_insert(None);
    }

    fn heartbeat(&mut self, at: Ts, link: &str) -> Option<(String, WisenseMessage)> {
        let last = self.links.get(link).copied().flatten();
        if last.is_some_and(|last| at.since(last) < self.heartbeat_every) {
            return None;
        }
        self.links.insert(link.to_string(), Some(at));
        Some((
            link.to_string(),
            WisenseMessage::Heartbeat {
                zone: self.zone.clone(),
            },
        ))
    }

    /// No line for too long (or end of input): every link is offline. Sent
    /// once per silence.
    pub fn on_silence(&mut self) -> Vec<(String, WisenseMessage)> {
        if self.silent {
            return Vec::new();
        }
        self.silent = true;
        self.links
            .keys()
            .map(|link| {
                (
                    link.clone(),
                    WisenseMessage::Offline {
                        zone: self.zone.clone(),
                    },
                )
            })
            .collect()
    }
}

/// Command-line options, shared by `homeguard bridge-wisense` and the
/// `homeguard-wisense-bridge` binary.
#[derive(Debug, Clone, clap::Args)]
pub struct BridgeArgs {
    /// The homeguard zone the wisense links watch (for logs and humans; the
    /// zone homeguard uses is the one configured for the link's sensor).
    #[arg(long)]
    pub zone: String,
    /// The link id (WiFi interface, e.g. wlp2s0). Sets an MQTT last will so
    /// homeguard hears "offline" even if the bridge is killed.
    #[arg(long)]
    pub link: Option<String>,
    #[arg(long, default_value = "127.0.0.1")]
    pub mqtt_host: String,
    #[arg(long, default_value_t = 1883)]
    pub mqtt_port: u16,
    /// Environment variable holding the MQTT user name.
    #[arg(long, default_value = "MQTT_HOMEGUARD_USER")]
    pub username_env: String,
    /// Environment variable holding the MQTT password.
    #[arg(long, default_value = "MQTT_HOMEGUARD_PASSWORD")]
    pub password_env: String,
    /// Report wisense offline after this long without a line.
    #[arg(long, default_value = "30s", value_parser = crate::core::time::parse_duration)]
    pub silence_timeout: Duration,
    /// Minimum time between heartbeats per link.
    #[arg(long, default_value = "10s", value_parser = crate::core::time::parse_duration)]
    pub heartbeat: Duration,
}

impl BridgeArgs {
    pub fn into_options(
        self,
        env: impl Fn(&str) -> Option<String>,
    ) -> anyhow::Result<BridgeOptions> {
        let get = |name: &str| match env(name) {
            Some(value) if !value.is_empty() => Ok(value),
            _ => Err(anyhow::anyhow!(
                "environment variable {name} is not set (the MQTT login, see .env.example)"
            )),
        };
        Ok(BridgeOptions {
            username: get(&self.username_env)?,
            password: get(&self.password_env)?,
            zone: self.zone,
            link: self.link,
            silence_timeout: self.silence_timeout,
            heartbeat_every: self.heartbeat,
            mqtt_host: self.mqtt_host,
            mqtt_port: self.mqtt_port,
        })
    }
}

/// Settings for [`run`].
pub struct BridgeOptions {
    pub zone: String,
    /// The link id for the MQTT last will (published if the bridge dies).
    pub link: Option<String>,
    pub silence_timeout: Duration,
    pub heartbeat_every: Duration,
    pub mqtt_host: String,
    pub mqtt_port: u16,
    pub username: String,
    pub password: String,
}

fn topic(link: &str) -> String {
    format!("{WISENSE_PREFIX}/{link}")
}

fn message(link: &str, message: &WisenseMessage) -> anyhow::Result<Outgoing> {
    Ok(Outgoing {
        topic: topic(link),
        payload: serde_json::to_vec(message).context("encoding a wisense message")?,
        retain: false,
    })
}

/// Reads stdin until it ends, publishing as it goes. Returns an error at
/// end of input so a service manager restarts the pipeline.
pub async fn run(options: BridgeOptions) -> anyhow::Result<()> {
    let last_will = match &options.link {
        Some(link) => Some(mqtt::Will {
            topic: topic(link),
            payload: serde_json::to_vec(&WisenseMessage::Offline {
                zone: options.zone.clone(),
            })
            .context("encoding the last will")?,
            retain: false,
        }),
        None => None,
    };
    let (client, mut eventloop) = Connection {
        client_id: format!("homeguard-wisense-{}", options.zone),
        host: options.mqtt_host.clone(),
        port: options.mqtt_port,
        username: options.username,
        password: options.password,
        keep_alive: Duration::from_secs(30),
        last_will,
    }
    .open();
    // Drive the connection in the background; rumqttc reconnects on poll.
    tokio::spawn(async move {
        let mut backoff = mqtt::Backoff::new(Duration::from_secs(30));
        loop {
            match eventloop.poll().await {
                Ok(rumqttc::Event::Incoming(rumqttc::Packet::ConnAck(_))) => {
                    info!("connected to MQTT broker");
                    backoff.reset();
                }
                Ok(_) => {}
                Err(e) => {
                    let delay = backoff.next_delay();
                    warn!(error = %e, retry_in = ?delay, "MQTT connection lost");
                    tokio::time::sleep(delay).await;
                }
            }
        }
    });

    let clock = crate::run::Clock::start();
    let mut bridge = Bridge::new(options.zone.clone(), options.heartbeat_every);
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    info!(zone = %options.zone, "reading wisense JSON lines from stdin");
    loop {
        let messages = match tokio::time::timeout(options.silence_timeout, lines.next_line()).await
        {
            Ok(Ok(Some(line))) => match bridge.on_line(clock.now(), &line) {
                Ok(messages) => messages,
                Err(e) => {
                    warn!(error = %e, "ignoring a line that is not wisense JSON");
                    continue;
                }
            },
            Ok(Ok(None)) => break,
            Ok(Err(e)) => {
                warn!(error = %e, "reading stdin failed");
                break;
            }
            Err(_) => {
                let messages = bridge.on_silence();
                if !messages.is_empty() {
                    warn!(
                        timeout = ?options.silence_timeout,
                        "no line from wisense; reporting it offline"
                    );
                }
                messages
            }
        };
        for (link, msg) in messages {
            debug!(%link, ?msg, "publish");
            mqtt::publish(&client, message(&link, &msg)?);
        }
    }

    warn!("wisense output ended; reporting it offline");
    for (link, msg) in bridge.on_silence() {
        mqtt::publish(&client, message(&link, &msg)?);
    }
    tokio::time::sleep(Duration::from_millis(500)).await;
    let _ = client.try_disconnect();
    anyhow::bail!("wisense stopped (end of input)")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bridge() -> Bridge {
        Bridge::new("living-room", Duration::from_secs(10))
    }

    const STARTED: &str = r#"{"event":"MotionStarted","link_id":"wlp2s0","ts_unix_ms":1700000000000,"kind":"rssi","score":2.5,"threshold":1.25}"#;
    const ENDED: &str = r#"{"event":"MotionEnded","link_id":"wlp2s0","ts_unix_ms":1700000004000,"kind":"rssi","duration_ms":4000,"peak_score":3.1}"#;

    fn status(link: &str) -> String {
        format!(
            r#"{{"event":"Status","link_id":"{link}","ts_unix_ms":1700000000500,"kind":"rssi","score":null,"threshold":1.25,"state":"idle"}}"#
        )
    }

    #[test]
    fn motion_events_become_messages_for_their_link() {
        let mut b = bridge();
        assert_eq!(
            b.on_line(Ts::ZERO, STARTED).unwrap(),
            vec![(
                "wlp2s0".into(),
                WisenseMessage::Motion {
                    zone: "living-room".into(),
                    score: Some(2.5),
                    ts_unix_ms: Some(1_700_000_000_000)
                }
            )]
        );
        assert_eq!(
            b.on_line(Ts::ZERO, ENDED).unwrap(),
            vec![(
                "wlp2s0".into(),
                WisenseMessage::MotionEnded {
                    zone: "living-room".into(),
                    duration_ms: Some(4000),
                    ts_unix_ms: Some(1_700_000_004_000)
                }
            )]
        );
    }

    #[test]
    fn status_is_a_rate_limited_heartbeat() {
        let mut b = bridge();
        let beats = |b: &mut Bridge, secs| {
            b.on_line(Ts::from_secs(secs), &status("wlp2s0"))
                .unwrap()
                .len()
        };
        assert_eq!(beats(&mut b, 0), 1);
        assert_eq!(beats(&mut b, 1), 0);
        assert_eq!(beats(&mut b, 9), 0);
        assert_eq!(beats(&mut b, 10), 1);
        // Per link.
        assert_eq!(
            b.on_line(Ts::from_secs(11), &status("esp-hall"))
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn zone_changes_are_motion_on_the_named_link() {
        let mut b = bridge();
        let set = r#"{"event":"ZoneChanged","ts_unix_ms":1700000001000,"zone":"kitchen","confidence":0.5}"#;
        let cleared =
            r#"{"event":"ZoneChanged","ts_unix_ms":1700000002000,"zone":null,"confidence":0.0}"#;
        assert_eq!(
            b.on_line(Ts::ZERO, set).unwrap(),
            vec![(
                "kitchen".into(),
                WisenseMessage::Motion {
                    zone: "living-room".into(),
                    score: None,
                    ts_unix_ms: Some(1_700_000_001_000)
                }
            )]
        );
        assert!(b.on_line(Ts::ZERO, cleared).unwrap().is_empty());
    }

    #[test]
    fn silence_reports_every_link_offline_once_then_recovers() {
        let mut b = bridge();
        assert!(b.on_silence().is_empty(), "no links yet");
        // The first silence above counts; a line ends it.
        b.on_line(Ts::ZERO, &status("wlp2s0")).unwrap();
        b.on_line(Ts::ZERO, &status("esp-hall")).unwrap();
        let offline = b.on_silence();
        assert_eq!(offline.len(), 2);
        assert!(offline
            .iter()
            .all(|(_, m)| matches!(m, WisenseMessage::Offline { .. })));
        assert!(b.on_silence().is_empty());
        // Back: the next status is a heartbeat right away, even within 10 s.
        let back = b.on_line(Ts::from_secs(1), &status("wlp2s0")).unwrap();
        assert!(matches!(
            back.as_slice(),
            [(_, WisenseMessage::Heartbeat { .. })]
        ));
        assert_eq!(b.on_silence().len(), 2);
    }

    #[test]
    fn calibration_counts_as_alive() {
        let mut b = bridge();
        let line = r#"{"event":"CalibrationStarted","link_id":"wlp2s0","ts_unix_ms":1,"duration_ms":30000}"#;
        assert!(matches!(
            b.on_line(Ts::ZERO, line).unwrap().as_slice(),
            [(_, WisenseMessage::Heartbeat { .. })]
        ));
    }

    #[test]
    fn unknown_events_and_garbage() {
        let mut b = bridge();
        assert!(b
            .on_line(Ts::ZERO, r#"{"event":"SomethingNew","link_id":"x"}"#)
            .unwrap()
            .is_empty());
        assert!(b.on_line(Ts::ZERO, "").unwrap().is_empty());
        assert!(b.on_line(Ts::ZERO, "wisense: starting").is_err());
        assert!(b.on_line(Ts::ZERO, r#"{"event":"MotionStarted"}"#).is_err());
        assert!(b.on_line(Ts::ZERO, r#"{"no_event":1}"#).is_err());
        assert_eq!(b.links().count(), 0);
    }
}
