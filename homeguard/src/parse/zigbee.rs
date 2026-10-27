//! Zigbee2MQTT device messages.
//!
//! - `zigbee2mqtt/<name>`: the device state as a JSON object, e.g.
//!   `{"contact": false, "battery": 97, "linkquality": 120}`.
//! - `zigbee2mqtt/<name>/availability`: `{"state": "online"}` (or the
//!   legacy plain `online` / `offline`).

use std::collections::BTreeMap;

use serde_json::Value;

use super::{json_object, mapped, utf8, ParseError};
use crate::core::{Event, MotionSource};

/// `rest` is the topic after `zigbee2mqtt/`.
pub fn parse(
    rest: &str,
    payload: &[u8],
    devices: &BTreeMap<String, String>,
) -> Result<Vec<Event>, ParseError> {
    let mut levels = rest.split('/');
    let (Some(name), second, None) = (levels.next(), levels.next(), levels.next()) else {
        return Ok(Vec::new());
    };
    // Zigbee2MQTT's own topics (bridge/state, bridge/devices, ...).
    if name.is_empty() || name == "bridge" {
        return Ok(Vec::new());
    }
    let sensor = mapped(devices, name);
    match second {
        None => state(sensor, payload),
        Some("availability") => availability(sensor, payload),
        Some(_) => Ok(Vec::new()),
    }
}

fn state(sensor: String, payload: &[u8]) -> Result<Vec<Event>, ParseError> {
    let fields = json_object(payload)?;
    let flag = |key: &'static str| -> Result<Option<bool>, ParseError> {
        match fields.get(key) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::Bool(b)) => Ok(Some(*b)),
            Some(_) => Err(ParseError::Field {
                field: key,
                problem: "is not a boolean",
            }),
        }
    };
    let mut events = Vec::new();
    // Zigbee2MQTT reports `contact: true` when the magnet is near: closed.
    if let Some(contact) = flag("contact")? {
        events.push(Event::ContactChanged {
            sensor: sensor.clone(),
            open: !contact,
        });
    }
    if flag("occupancy")? == Some(true) {
        events.push(Event::MotionDetected {
            source: MotionSource::Pir,
            sensor: sensor.clone(),
        });
    }
    if flag("smoke")? == Some(true) {
        events.push(Event::SmokeDetected {
            sensor: sensor.clone(),
        });
    }
    if flag("water_leak")? == Some(true) {
        events.push(Event::LeakDetected { sensor });
    }
    Ok(events)
}

fn availability(device: String, payload: &[u8]) -> Result<Vec<Event>, ParseError> {
    let text = utf8(payload)?.trim();
    let state = if text.starts_with('{') {
        let fields = json_object(payload)?;
        match fields.get("state") {
            Some(Value::String(s)) => s.clone(),
            _ => {
                return Err(ParseError::Field {
                    field: "state",
                    problem: "is missing or not a string",
                })
            }
        }
    } else {
        text.to_string()
    };
    match state.as_str() {
        "online" => Ok(vec![Event::DeviceOnline { device }]),
        "offline" => Ok(vec![Event::DeviceOffline { device }]),
        other => Err(ParseError::UnexpectedValue(other.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(rest: &str, payload: &str) -> Result<Vec<Event>, ParseError> {
        let devices = BTreeMap::from([("0x00124b0024c1a2b3".to_string(), "back_door".to_string())]);
        parse(rest, payload.as_bytes(), &devices)
    }

    #[test]
    fn contact_false_means_open() {
        assert_eq!(
            run("front_door", r#"{"contact":false,"battery":100}"#),
            Ok(vec![Event::ContactChanged {
                sensor: "front_door".into(),
                open: true
            }])
        );
        assert_eq!(
            run("front_door", r#"{"contact":true}"#),
            Ok(vec![Event::ContactChanged {
                sensor: "front_door".into(),
                open: false
            }])
        );
    }

    #[test]
    fn maps_friendly_names_to_sensor_ids() {
        assert_eq!(
            run("0x00124b0024c1a2b3", r#"{"contact":false}"#),
            Ok(vec![Event::ContactChanged {
                sensor: "back_door".into(),
                open: true
            }])
        );
    }

    #[test]
    fn occupancy_smoke_and_leak_only_when_true() {
        assert_eq!(
            run("hall_pir", r#"{"occupancy":true}"#),
            Ok(vec![Event::MotionDetected {
                source: MotionSource::Pir,
                sensor: "hall_pir".into()
            }])
        );
        assert_eq!(run("hall_pir", r#"{"occupancy":false}"#), Ok(vec![]));
        assert_eq!(
            run("smoke", r#"{"smoke":true,"battery_low":false}"#),
            Ok(vec![Event::SmokeDetected {
                sensor: "smoke".into()
            }])
        );
        assert_eq!(run("smoke", r#"{"smoke":false}"#), Ok(vec![]));
        assert_eq!(
            run("leak", r#"{"water_leak":true}"#),
            Ok(vec![Event::LeakDetected {
                sensor: "leak".into()
            }])
        );
    }

    #[test]
    fn availability_new_and_legacy_formats() {
        let offline = Ok(vec![Event::DeviceOffline {
            device: "front_door".into(),
        }]);
        assert_eq!(
            run("front_door/availability", r#"{"state":"offline"}"#),
            offline
        );
        assert_eq!(run("front_door/availability", "offline"), offline);
        assert_eq!(
            run("front_door/availability", r#"{"state":"online"}"#),
            Ok(vec![Event::DeviceOnline {
                device: "front_door".into()
            }])
        );
        assert!(run("front_door/availability", "maybe").is_err());
        assert!(run("front_door/availability", r#"{"state":1}"#).is_err());
    }

    #[test]
    fn ignores_bridge_and_other_subtopics() {
        assert_eq!(run("bridge", r#"{"state":"online"}"#), Ok(vec![]));
        assert_eq!(run("front_door/set", r#"{"state":"ON"}"#), Ok(vec![]));
        assert_eq!(run("a/b/c", "{}"), Ok(vec![]));
        assert_eq!(run("", "{}"), Ok(vec![]));
    }

    #[test]
    fn malformed_payloads_are_errors_not_panics() {
        assert!(matches!(run("door", "not json"), Err(ParseError::Json(_))));
        assert_eq!(run("door", "[1,2]"), Err(ParseError::NotAnObject));
        assert!(matches!(
            run("door", r#"{"contact":"yes"}"#),
            Err(ParseError::Field {
                field: "contact",
                ..
            })
        ));
        assert!(parse("door", &[0xff, 0xfe], &BTreeMap::new()).is_err());
        // Unrelated fields alone: nothing to report.
        assert_eq!(run("plug", r#"{"state":"ON","power":3.2}"#), Ok(vec![]));
        assert_eq!(run("door", r#"{"contact":null}"#), Ok(vec![]));
    }
}
