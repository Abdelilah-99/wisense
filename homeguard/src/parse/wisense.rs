//! Messages from the wisense bridge on `homeguard/wisense/<link_id>`.
//!
//! The bridge (`homeguard bridge-wisense`) turns wisense's own JSON lines
//! into these smaller messages, so homeguard does not depend on wisense's
//! internal event format.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::{mapped, ParseError};
use crate::core::{Event, MotionSource};

/// What the bridge publishes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WisenseMessage {
    /// Motion started on the link.
    Motion {
        zone: String,
        #[serde(default)]
        score: Option<f32>,
        #[serde(default)]
        ts_unix_ms: Option<u64>,
    },
    /// Motion ended on the link (logged by the bridge, not an event here).
    MotionEnded {
        zone: String,
        #[serde(default)]
        duration_ms: Option<u64>,
        #[serde(default)]
        ts_unix_ms: Option<u64>,
    },
    /// wisense is alive (sent periodically).
    Heartbeat { zone: String },
    /// wisense stopped sending: the bridge saw no line for a while, or the
    /// broker published the bridge's last will.
    Offline { zone: String },
}

/// `rest` is the topic after `homeguard/wisense/`: the link id.
pub fn parse(
    rest: &str,
    payload: &[u8],
    links: &BTreeMap<String, String>,
) -> Result<Vec<Event>, ParseError> {
    if rest.is_empty() || rest.contains('/') {
        return Ok(Vec::new());
    }
    let message: WisenseMessage =
        serde_json::from_slice(payload).map_err(|e| ParseError::Json(e.to_string()))?;
    let sensor = mapped(links, rest);
    Ok(match message {
        WisenseMessage::Motion { .. } => vec![Event::MotionDetected {
            source: MotionSource::Wisense,
            sensor,
        }],
        WisenseMessage::MotionEnded { .. } => Vec::new(),
        WisenseMessage::Heartbeat { .. } => vec![Event::DeviceOnline { device: sensor }],
        WisenseMessage::Offline { .. } => vec![Event::DeviceOffline { device: sensor }],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn links() -> BTreeMap<String, String> {
        BTreeMap::from([("wlp2s0".to_string(), "living_wisense".to_string())])
    }

    #[test]
    fn maps_each_message() {
        assert_eq!(
            parse(
                "wlp2s0",
                br#"{"type":"motion","zone":"living-room","score":2.5}"#,
                &links()
            ),
            Ok(vec![Event::MotionDetected {
                source: MotionSource::Wisense,
                sensor: "living_wisense".into()
            }])
        );
        assert_eq!(
            parse(
                "wlp2s0",
                br#"{"type":"heartbeat","zone":"living-room"}"#,
                &links()
            ),
            Ok(vec![Event::DeviceOnline {
                device: "living_wisense".into()
            }])
        );
        assert_eq!(
            parse("esp-1", br#"{"type":"offline","zone":"hall"}"#, &links()),
            Ok(vec![Event::DeviceOffline {
                device: "esp-1".into()
            }])
        );
        assert_eq!(
            parse(
                "wlp2s0",
                br#"{"type":"motion_ended","zone":"living-room","duration_ms":4000}"#,
                &links()
            ),
            Ok(vec![])
        );
    }

    #[test]
    fn rejects_garbage_and_ignores_deeper_topics() {
        assert!(parse("wlp2s0", b"{", &links()).is_err());
        assert!(parse("wlp2s0", br#"{"type":"dance","zone":"x"}"#, &links()).is_err());
        assert!(parse("wlp2s0", br#"{"type":"motion"}"#, &links()).is_err());
        assert_eq!(parse("wlp2s0/extra", b"{}", &links()), Ok(vec![]));
    }

    #[test]
    fn round_trips_through_json() {
        let message = WisenseMessage::Motion {
            zone: "living-room".into(),
            score: Some(2.5),
            ts_unix_ms: Some(1_700_000_000_000),
        };
        let json = serde_json::to_string(&message).unwrap();
        assert_eq!(
            json,
            r#"{"type":"motion","zone":"living-room","score":2.5,"ts_unix_ms":1700000000000}"#
        );
        assert_eq!(
            serde_json::from_str::<WisenseMessage>(&json).unwrap(),
            message
        );
    }
}
