//! A snapshot of everything worth showing: the alarm state, every zone with
//! its sensors and cameras, who is home, and recent events. The core loop
//! rebuilds it after each event; Telegram and the dashboard read it.

use std::collections::VecDeque;

use serde::Serialize;

use crate::core::config::SensorKind;
use crate::core::{Action, AlertLevel, Engine, Mode};

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Status {
    /// `disarmed`, `arming`, `armed`, `pending` or `triggered`.
    pub state: String,
    pub mode: Option<Mode>,
    /// Why it is pending or triggered.
    pub cause: Option<String>,
    pub snapshot_ref: Option<String>,
    pub zones: Vec<ZoneStatus>,
    pub presence: Vec<PersonStatus>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ZoneStatus {
    pub id: String,
    pub devices: Vec<DeviceStatus>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DeviceStatus {
    pub id: String,
    pub name: String,
    /// `contact`, `pir`, `wisense`, `smoke`, `leak` or `camera`.
    pub kind: &'static str,
    pub online: bool,
    /// Contacts only: last reported state.
    pub open: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PersonStatus {
    pub name: String,
    /// `None` until first known.
    pub home: Option<bool>,
}

impl Status {
    pub fn from_engine(engine: &Engine) -> Status {
        let state = engine.state();
        let (cause, snapshot_ref) = match state {
            crate::core::State::Pending { cause, .. }
            | crate::core::State::Triggered { cause, .. } => {
                (Some(cause.description.clone()), cause.snapshot_ref.clone())
            }
            _ => (None, None),
        };
        let catalog = engine.catalog();
        let zones = catalog
            .zones()
            .map(|zone| {
                let sensors = catalog
                    .sensors()
                    .filter(|s| s.zone == zone)
                    .map(|s| DeviceStatus {
                        id: s.id.clone(),
                        name: catalog.display_name(&s.id).to_string(),
                        kind: kind_name(s.kind),
                        online: !engine.offline().contains(&s.id),
                        open: (s.kind == SensorKind::Contact)
                            .then(|| engine.contacts().get(&s.id).copied())
                            .flatten(),
                    });
                let cameras = catalog
                    .cameras()
                    .filter(|c| c.zone == zone)
                    .map(|c| DeviceStatus {
                        id: c.id.clone(),
                        name: catalog.display_name(&c.id).to_string(),
                        kind: "camera",
                        online: !engine.offline().contains(&c.id),
                        open: None,
                    });
                ZoneStatus {
                    id: zone.to_string(),
                    devices: sensors.chain(cameras).collect(),
                }
            })
            .collect();
        let presence = engine
            .known_people()
            .map(|name| PersonStatus {
                name: name.to_string(),
                home: engine.presence().get(name).copied(),
            })
            .collect();
        Status {
            state: state.name().to_string(),
            mode: state.mode(),
            cause,
            snapshot_ref,
            zones,
            presence,
        }
    }

    pub fn camera_ids(&self) -> impl Iterator<Item = &str> {
        self.zones
            .iter()
            .flat_map(|z| &z.devices)
            .filter(|d| d.kind == "camera")
            .map(|d| d.id.as_str())
    }
}

fn kind_name(kind: SensorKind) -> &'static str {
    match kind {
        SensorKind::Contact => "contact",
        SensorKind::Pir => "pir",
        SensorKind::Wisense => "wisense",
        SensorKind::Smoke => "smoke",
        SensorKind::Leak => "leak",
    }
}

/// One line of the recent-events list.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct LogLine {
    /// Wall-clock time, milliseconds since the Unix epoch.
    pub unix_ms: u64,
    /// `state`, `alert` or `event`.
    pub kind: &'static str,
    pub level: Option<AlertLevel>,
    pub text: String,
    pub snapshot_ref: Option<String>,
}

impl LogLine {
    /// The line for an action worth listing; `None` for siren and lights,
    /// which accompany a state change that is listed anyway.
    pub fn from_action(unix_ms: u64, action: &Action) -> Option<LogLine> {
        let (kind, level, text, snapshot_ref) = match action {
            Action::StateChanged { state } => ("state", None, state.to_string(), None),
            Action::SendAlert {
                level,
                text,
                snapshot_ref,
            } => ("alert", Some(*level), text.clone(), snapshot_ref.clone()),
            Action::LogEvent { text } => ("event", None, text.clone(), None),
            Action::SirenOn | Action::SirenOff | Action::LightsOn => return None,
        };
        Some(LogLine {
            unix_ms,
            kind,
            level,
            text,
            snapshot_ref,
        })
    }
}

/// The last `capacity` lines, newest last.
#[derive(Debug, Clone)]
pub struct RecentLog {
    lines: VecDeque<LogLine>,
    capacity: usize,
}

impl RecentLog {
    pub fn new(capacity: usize) -> Self {
        RecentLog {
            lines: VecDeque::with_capacity(capacity),
            capacity,
        }
    }

    pub fn push(&mut self, line: LogLine) {
        if self.lines.len() == self.capacity {
            self.lines.pop_front();
        }
        self.lines.push_back(line);
    }

    /// The newest `n`, newest first.
    pub fn latest(&self, n: usize) -> Vec<LogLine> {
        self.lines.iter().rev().take(n).cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::{CoreConfig, Event, Input, Ts};

    fn engine() -> Engine {
        let config: CoreConfig = toml::from_str(
            r#"
            zones = ["hall", "garden"]
            [presence]
            people = [{ name = "alice" }]
            [[sensors]]
            id = "door"
            kind = "contact"
            zone = "hall"
            name = "Front door"
            [[cameras]]
            id = "cam"
            zone = "garden"
            "#,
        )
        .unwrap();
        Engine::new(&config).unwrap()
    }

    #[test]
    fn reflects_zones_devices_and_presence() {
        let mut engine = engine();
        for event in [
            Event::ContactChanged {
                sensor: "door".into(),
                open: true,
            },
            Event::DeviceOffline {
                device: "cam".into(),
            },
        ] {
            engine.handle(&Input::new(Ts::ZERO, event));
        }
        engine.handle(&Input::new(Ts::from_secs(120), Event::Tick));
        let status = Status::from_engine(&engine);
        assert_eq!(status.state, "disarmed");
        assert_eq!(status.zones.len(), 2);
        let door = &status
            .zones
            .iter()
            .find(|z| z.id == "hall")
            .unwrap()
            .devices[0];
        assert_eq!(door.name, "Front door");
        assert_eq!(door.open, Some(true));
        assert!(door.online);
        let cam = &status
            .zones
            .iter()
            .find(|z| z.id == "garden")
            .unwrap()
            .devices[0];
        assert!(!cam.online);
        assert_eq!(status.camera_ids().collect::<Vec<_>>(), ["cam"]);
        assert_eq!(
            status.presence,
            vec![PersonStatus {
                name: "alice".into(),
                home: None
            }]
        );
    }

    #[test]
    fn recent_log_keeps_the_newest() {
        let mut log = RecentLog::new(3);
        for i in 0..5 {
            log.push(LogLine::from_action(i, &Action::log(format!("e{i}"))).unwrap());
        }
        let texts: Vec<String> = log.latest(10).into_iter().map(|l| l.text).collect();
        assert_eq!(texts, ["e4", "e3", "e2"]);
        assert!(LogLine::from_action(0, &Action::SirenOn).is_none());
    }
}
