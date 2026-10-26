//! The alarm states.

use std::fmt;

use serde::Serialize;

use super::event::Mode;
use super::time::Ts;

/// Why the alarm went into `Pending` or `Triggered`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Cause {
    /// The sensor or camera that caused it.
    pub sensor: String,
    /// Human-readable, e.g. "front door opened".
    pub description: String,
    /// A Frigate event id whose snapshot shows what happened.
    pub snapshot_ref: Option<String>,
}

impl Cause {
    pub fn new(sensor: impl Into<String>, description: impl Into<String>) -> Self {
        Cause {
            sensor: sensor.into(),
            description: description.into(),
            snapshot_ref: None,
        }
    }

    pub fn with_snapshot(mut self, snapshot_ref: Option<String>) -> Self {
        self.snapshot_ref = snapshot_ref;
        self
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum State {
    Disarmed,
    /// Exit delay: armed once `until` is reached.
    Arming {
        mode: Mode,
        until: Ts,
    },
    Armed {
        mode: Mode,
    },
    /// Entry delay: triggered at `until` unless disarmed first.
    Pending {
        mode: Mode,
        until: Ts,
        cause: Cause,
    },
    Triggered {
        mode: Mode,
        cause: Cause,
        since: Ts,
    },
}

impl State {
    /// The short name used in MQTT, the API and scenarios.
    pub fn name(&self) -> &'static str {
        match self {
            State::Disarmed => "disarmed",
            State::Arming { .. } => "arming",
            State::Armed { .. } => "armed",
            State::Pending { .. } => "pending",
            State::Triggered { .. } => "triggered",
        }
    }

    /// The mode the system is armed (or arming) in.
    pub fn mode(&self) -> Option<Mode> {
        match self {
            State::Disarmed => None,
            State::Arming { mode, .. }
            | State::Armed { mode }
            | State::Pending { mode, .. }
            | State::Triggered { mode, .. } => Some(*mode),
        }
    }

    /// Armed, pending or triggered: intrusion sensors are being watched.
    pub fn is_guarding(&self) -> bool {
        matches!(
            self,
            State::Armed { .. } | State::Pending { .. } | State::Triggered { .. }
        )
    }
}

impl fmt::Display for State {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            State::Disarmed => f.write_str("disarmed"),
            State::Arming { mode, until } => write!(f, "arming ({mode}) until {until}"),
            State::Armed { mode } => write!(f, "armed ({mode})"),
            State::Pending { mode, until, cause } => {
                write!(f, "pending ({mode}) until {until}: {}", cause.description)
            }
            State::Triggered { mode, cause, since } => {
                write!(f, "TRIGGERED ({mode}) since {since}: {}", cause.description)
            }
        }
    }
}
