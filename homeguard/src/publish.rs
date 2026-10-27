//! What homeguard publishes on MQTT, as a pure function of the actions.
//!
//! | Topic | Payload | Retained |
//! |---|---|---|
//! | `homeguard/availability` | `online` / `offline` (last will) | yes |
//! | `homeguard/state` | JSON: state, mode, Home Assistant state, cause | yes |
//! | `homeguard/actions/alert` | JSON: level, text, snapshot_ref | no |
//! | `homeguard/actions/siren` | `ON` / `OFF` | yes |
//! | `homeguard/actions/lights` | `ON` | no |
//! | `zigbee2mqtt/<siren>/set` | IAS warning command | no |
//! | `zigbee2mqtt/<light>/set` | `{"state":"ON"}` | no |

use std::time::Duration;

use serde_json::json;

use crate::config::Actuators;
use crate::core::{Action, Mode, State};

pub const AVAILABILITY_TOPIC: &str = "homeguard/availability";
pub const STATE_TOPIC: &str = "homeguard/state";
pub const ACTIONS_PREFIX: &str = "homeguard/actions";

#[derive(Debug, Clone, PartialEq)]
pub struct Outgoing {
    pub topic: String,
    pub payload: Vec<u8>,
    pub retain: bool,
}

impl Outgoing {
    fn new(topic: impl Into<String>, payload: impl Into<Vec<u8>>, retain: bool) -> Self {
        Outgoing {
            topic: topic.into(),
            payload: payload.into(),
            retain,
        }
    }
}

/// Where and how to drive Zigbee actuators.
#[derive(Debug, Clone, PartialEq)]
pub struct Publisher {
    pub zigbee_base: String,
    pub actuators: Actuators,
    /// How long the siren sounds unless stopped (the alarm auto-reset).
    pub siren_duration: Duration,
}

impl Publisher {
    pub fn outgoing(&self, action: &Action) -> Vec<Outgoing> {
        match action {
            Action::StateChanged { state } => vec![state_message(state)],
            Action::SendAlert {
                level,
                text,
                snapshot_ref,
            } => vec![Outgoing::new(
                format!("{ACTIONS_PREFIX}/alert"),
                json!({ "level": level, "text": text, "snapshot_ref": snapshot_ref }).to_string(),
                false,
            )],
            Action::SirenOn => {
                let mut out = vec![Outgoing::new(format!("{ACTIONS_PREFIX}/siren"), "ON", true)];
                if let Some(siren) = &self.actuators.siren {
                    let warning = json!({ "warning": {
                        "mode": "emergency",
                        "level": "very_high",
                        "strobe": true,
                        "duration": self.siren_duration.as_secs(),
                    }});
                    out.push(Outgoing::new(
                        format!("{}/{siren}/set", self.zigbee_base),
                        warning.to_string(),
                        false,
                    ));
                }
                out
            }
            Action::SirenOff => {
                let mut out = vec![Outgoing::new(
                    format!("{ACTIONS_PREFIX}/siren"),
                    "OFF",
                    true,
                )];
                if let Some(siren) = &self.actuators.siren {
                    out.push(Outgoing::new(
                        format!("{}/{siren}/set", self.zigbee_base),
                        json!({ "warning": { "mode": "stop" } }).to_string(),
                        false,
                    ));
                }
                out
            }
            Action::LightsOn => {
                let mut out = vec![Outgoing::new(
                    format!("{ACTIONS_PREFIX}/lights"),
                    "ON",
                    false,
                )];
                for light in &self.actuators.lights {
                    out.push(Outgoing::new(
                        format!("{}/{light}/set", self.zigbee_base),
                        r#"{"state":"ON"}"#,
                        false,
                    ));
                }
                out
            }
            Action::LogEvent { .. } => Vec::new(),
        }
    }
}

/// The retained `homeguard/state` message.
pub fn state_message(state: &State) -> Outgoing {
    let cause = match state {
        State::Pending { cause, .. } | State::Triggered { cause, .. } => Some(cause),
        _ => None,
    };
    let payload = json!({
        "state": state.name(),
        "mode": state.mode(),
        "ha_state": ha_state(state),
        "cause": cause.map(|c| &c.description),
        "sensor": cause.map(|c| &c.sensor),
        "snapshot_ref": cause.and_then(|c| c.snapshot_ref.as_ref()),
    });
    Outgoing::new(STATE_TOPIC, payload.to_string(), true)
}

/// The state names Home Assistant's MQTT alarm panel expects.
pub fn ha_state(state: &State) -> &'static str {
    match state {
        State::Disarmed => "disarmed",
        State::Arming { .. } => "arming",
        State::Armed { mode: Mode::Away } => "armed_away",
        State::Armed { mode: Mode::Home } => "armed_home",
        State::Armed { mode: Mode::Night } => "armed_night",
        State::Pending { .. } => "pending",
        State::Triggered { .. } => "triggered",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::{AlertLevel, Cause, Ts};

    fn publisher(actuators: Actuators) -> Publisher {
        Publisher {
            zigbee_base: "zigbee2mqtt".into(),
            actuators,
            siren_duration: Duration::from_secs(600),
        }
    }

    fn text(o: &Outgoing) -> &str {
        std::str::from_utf8(&o.payload).unwrap()
    }

    #[test]
    fn state_is_retained_json() {
        let out = publisher(Actuators::default()).outgoing(&Action::StateChanged {
            state: State::Triggered {
                mode: Mode::Away,
                cause: Cause::new("front_door", "Front door opened")
                    .with_snapshot(Some("evt".into())),
                since: Ts::from_secs(5),
            },
        });
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].topic, "homeguard/state");
        assert!(out[0].retain);
        let value: serde_json::Value = serde_json::from_slice(&out[0].payload).unwrap();
        assert_eq!(value["state"], "triggered");
        assert_eq!(value["mode"], "away");
        assert_eq!(value["ha_state"], "triggered");
        assert_eq!(value["cause"], "Front door opened");
        assert_eq!(value["snapshot_ref"], "evt");

        let armed = state_message(&State::Armed { mode: Mode::Night });
        let value: serde_json::Value = serde_json::from_slice(&armed.payload).unwrap();
        assert_eq!(value["ha_state"], "armed_night");
        assert!(value["cause"].is_null());
    }

    #[test]
    fn siren_and_lights_drive_configured_zigbee_devices() {
        let p = publisher(Actuators {
            siren: Some("hall_siren".into()),
            lights: vec!["plug_a".into(), "plug_b".into()],
        });
        let on = p.outgoing(&Action::SirenOn);
        assert_eq!(on[0].topic, "homeguard/actions/siren");
        assert_eq!(text(&on[0]), "ON");
        assert_eq!(on[1].topic, "zigbee2mqtt/hall_siren/set");
        assert!(text(&on[1]).contains(r#""duration":600"#));
        let off = p.outgoing(&Action::SirenOff);
        assert_eq!(text(&off[1]), r#"{"warning":{"mode":"stop"}}"#);
        let lights = p.outgoing(&Action::LightsOn);
        assert_eq!(lights.len(), 3);
        assert_eq!(lights[2].topic, "zigbee2mqtt/plug_b/set");
    }

    #[test]
    fn without_actuators_only_homeguard_topics() {
        let p = publisher(Actuators::default());
        assert_eq!(p.outgoing(&Action::SirenOn).len(), 1);
        assert_eq!(p.outgoing(&Action::LightsOn).len(), 1);
        assert!(p.outgoing(&Action::log("x")).is_empty());
        let alert = p.outgoing(&Action::alert(AlertLevel::High, "hi"));
        assert_eq!(alert[0].topic, "homeguard/actions/alert");
        assert!(!alert[0].retain);
        assert_eq!(
            text(&alert[0]),
            r#"{"level":"high","snapshot_ref":null,"text":"hi"}"#
        );
    }
}
