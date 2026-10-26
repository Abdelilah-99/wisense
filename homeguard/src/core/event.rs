//! What goes into the core ([`Input`]) and what comes out ([`Action`]).

use std::fmt;

use serde::{Deserialize, Serialize};

use super::state::State;
use super::time::Ts;

/// One event, stamped with the time it happened.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Input {
    pub at: Ts,
    pub event: Event,
}

impl Input {
    pub fn new(at: Ts, event: Event) -> Self {
        Input { at, event }
    }
}

/// Arming modes. Each sensor says in which modes it is active.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// Nobody home: everything is active.
    Away,
    /// Someone home and awake: doors and windows only.
    Home,
    /// Someone home and asleep: doors, windows and chosen motion sensors.
    Night,
}

impl Mode {
    pub const ALL: [Mode; 3] = [Mode::Away, Mode::Home, Mode::Night];

    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Away => "away",
            Mode::Home => "home",
            Mode::Night => "night",
        }
    }

    pub fn parse(text: &str) -> Option<Mode> {
        Mode::ALL
            .into_iter()
            .find(|m| m.as_str().eq_ignore_ascii_case(text.trim()))
    }
}

impl fmt::Display for Mode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Where a motion event came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MotionSource {
    /// A PIR sensor (Zigbee `occupancy`).
    Pir,
    /// wisense WiFi motion detection.
    Wisense,
    /// Motion seen by a camera.
    Camera,
}

impl fmt::Display for MotionSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            MotionSource::Pir => "PIR",
            MotionSource::Wisense => "wisense",
            MotionSource::Camera => "camera",
        })
    }
}

/// A user command, from the dashboard, Telegram or presence automation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Command {
    Arm(Mode),
    Disarm,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    /// A door or window contact opened (`open: true`) or closed.
    ContactChanged {
        sensor: String,
        open: bool,
    },
    MotionDetected {
        source: MotionSource,
        sensor: String,
    },
    /// Frigate saw a person. `snapshot_ref` is the Frigate event id.
    PersonDetected {
        camera: String,
        score: f32,
        #[serde(default)]
        snapshot_ref: Option<String>,
    },
    PresenceChanged {
        person: String,
        home: bool,
    },
    DeviceOffline {
        device: String,
    },
    DeviceOnline {
        device: String,
    },
    SmokeDetected {
        sensor: String,
    },
    LeakDetected {
        sensor: String,
    },
    Command {
        command: Command,
        by: String,
    },
    /// Time passes: lets delays and windows expire.
    Tick,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AlertLevel {
    /// Delivered silently.
    Low,
    /// Arming, disarming and other routine notices.
    Info,
    /// Needs attention: entry delay running, possible tampering.
    High,
    /// Alarm, smoke, water leak.
    Critical,
}

impl fmt::Display for AlertLevel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            AlertLevel::Low => "low",
            AlertLevel::Info => "info",
            AlertLevel::High => "high",
            AlertLevel::Critical => "critical",
        })
    }
}

/// What the shell must do. The core only describes; it never performs.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Action {
    /// The alarm state changed; publish it.
    StateChanged {
        state: State,
    },
    SendAlert {
        level: AlertLevel,
        text: String,
        snapshot_ref: Option<String>,
    },
    SirenOn,
    SirenOff,
    LightsOn,
    /// Something worth keeping in the event log, with no other effect.
    LogEvent {
        text: String,
    },
}

impl Action {
    pub fn alert(level: AlertLevel, text: impl Into<String>) -> Action {
        Action::SendAlert {
            level,
            text: text.into(),
            snapshot_ref: None,
        }
    }

    pub fn log(text: impl Into<String>) -> Action {
        Action::LogEvent { text: text.into() }
    }
}

impl fmt::Display for Action {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Action::StateChanged { state } => write!(f, "state → {state}"),
            Action::SendAlert {
                level,
                text,
                snapshot_ref,
            } => {
                write!(f, "alert [{level}] {text}")?;
                if let Some(snapshot) = snapshot_ref {
                    write!(f, " (snapshot {snapshot})")?;
                }
                Ok(())
            }
            Action::SirenOn => f.write_str("siren on"),
            Action::SirenOff => f.write_str("siren off"),
            Action::LightsOn => f.write_str("lights on"),
            Action::LogEvent { text } => write!(f, "log: {text}"),
        }
    }
}
