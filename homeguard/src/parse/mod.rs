//! Pure parsers for the MQTT messages homeguard listens to.
//!
//! Each parser turns one `(topic, payload)` into zero or more core
//! [`Event`]s. They never panic: unknown fields are ignored, messages on
//! topics they do not handle produce nothing, and malformed payloads come
//! back as a [`ParseError`] for the shell to log and drop.

pub mod frigate;
pub mod wisense;
pub mod zigbee;

use std::collections::BTreeMap;

use thiserror::Error;

use crate::core::Event;

#[derive(Debug, Clone, PartialEq, Error)]
pub enum ParseError {
    #[error("payload is not valid JSON: {0}")]
    Json(String),
    #[error("payload is not a JSON object")]
    NotAnObject,
    #[error("payload is not UTF-8")]
    NotUtf8,
    #[error("field {field:?} {problem}")]
    Field {
        field: &'static str,
        problem: &'static str,
    },
    #[error("unexpected value {0:?}")]
    UnexpectedValue(String),
}

/// How external names map to homeguard ids, and where each source publishes.
#[derive(Debug, Clone, PartialEq)]
pub struct Mapping {
    pub zigbee_base: String,
    /// Zigbee2MQTT friendly name → sensor id; unmapped names are used as is.
    pub zigbee_devices: BTreeMap<String, String>,
    pub frigate_prefix: String,
    /// Every configured camera, for Frigate-wide availability.
    pub cameras: Vec<String>,
    /// wisense link id → sensor id; unmapped links are used as is.
    pub wisense_links: BTreeMap<String, String>,
}

impl Default for Mapping {
    fn default() -> Self {
        Mapping {
            zigbee_base: "zigbee2mqtt".into(),
            zigbee_devices: BTreeMap::new(),
            frigate_prefix: "frigate".into(),
            cameras: Vec::new(),
            wisense_links: BTreeMap::new(),
        }
    }
}

/// Topic prefix of the messages published by the wisense bridge.
pub const WISENSE_PREFIX: &str = "homeguard/wisense";

impl Mapping {
    /// The topic filters to subscribe to.
    pub fn subscriptions(&self) -> Vec<String> {
        vec![
            format!("{}/+", self.zigbee_base),
            format!("{}/+/availability", self.zigbee_base),
            format!("{}/events", self.frigate_prefix),
            format!("{}/available", self.frigate_prefix),
            format!("{}/+/status/detect", self.frigate_prefix),
            format!("{WISENSE_PREFIX}/+"),
        ]
    }

    /// Routes a message to its parser.
    pub fn parse(&self, topic: &str, payload: &[u8]) -> Result<Vec<Event>, ParseError> {
        if let Some(rest) = strip(topic, &self.zigbee_base) {
            return zigbee::parse(rest, payload, &self.zigbee_devices);
        }
        if let Some(rest) = strip(topic, &self.frigate_prefix) {
            return frigate::parse(rest, payload, &self.cameras);
        }
        if let Some(rest) = strip(topic, WISENSE_PREFIX) {
            return wisense::parse(rest, payload, &self.wisense_links);
        }
        Ok(Vec::new())
    }
}

/// `topic` without `prefix/`, if it starts with it.
fn strip<'a>(topic: &'a str, prefix: &str) -> Option<&'a str> {
    topic.strip_prefix(prefix)?.strip_prefix('/')
}

fn utf8(payload: &[u8]) -> Result<&str, ParseError> {
    std::str::from_utf8(payload).map_err(|_| ParseError::NotUtf8)
}

fn json_object(payload: &[u8]) -> Result<serde_json::Map<String, serde_json::Value>, ParseError> {
    let value: serde_json::Value =
        serde_json::from_slice(payload).map_err(|e| ParseError::Json(e.to_string()))?;
    match value {
        serde_json::Value::Object(map) => Ok(map),
        _ => Err(ParseError::NotAnObject),
    }
}

fn mapped(map: &BTreeMap<String, String>, name: &str) -> String {
    map.get(name).cloned().unwrap_or_else(|| name.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routes_by_prefix_and_ignores_other_topics() {
        let mapping = Mapping::default();
        assert_eq!(
            mapping.parse("zigbee2mqtt/front_door", br#"{"contact":false}"#),
            Ok(vec![Event::ContactChanged {
                sensor: "front_door".into(),
                open: true
            }])
        );
        assert_eq!(mapping.parse("somewhere/else", b"{}"), Ok(vec![]));
        // A prefix must be a whole topic level.
        assert_eq!(mapping.parse("zigbee2mqttx/door", b"{}"), Ok(vec![]));
        assert_eq!(mapping.parse("frigate", b"{}"), Ok(vec![]));
    }

    #[test]
    fn subscriptions_cover_every_source() {
        let subs = Mapping::default().subscriptions();
        for topic in [
            "zigbee2mqtt/+",
            "zigbee2mqtt/+/availability",
            "frigate/events",
            "frigate/available",
            "frigate/+/status/detect",
            "homeguard/wisense/+",
        ] {
            assert!(subs.iter().any(|s| s == topic), "{topic}");
        }
    }
}
