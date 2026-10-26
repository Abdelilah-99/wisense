//! Settings the core needs, deserialized from `homeguard.toml`.

use std::collections::BTreeMap;
use std::time::Duration;

use serde::Deserialize;
use thiserror::Error;

use super::event::Mode;
use super::time::serde_duration;

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AlarmConfig {
    /// Time to leave after arming.
    #[serde(with = "serde_duration", default = "defaults::exit_delay")]
    pub exit_delay: Duration,
    /// Time to disarm after an entry door opens.
    #[serde(with = "serde_duration", default = "defaults::entry_delay")]
    pub entry_delay: Duration,
    /// A triggered alarm silences the siren and re-arms after this long.
    #[serde(with = "serde_duration", default = "defaults::auto_reset")]
    pub auto_reset: Duration,
}

impl Default for AlarmConfig {
    fn default() -> Self {
        AlarmConfig {
            exit_delay: defaults::exit_delay(),
            entry_delay: defaults::entry_delay(),
            auto_reset: defaults::auto_reset(),
        }
    }
}

mod defaults {
    use std::time::Duration;

    pub fn exit_delay() -> Duration {
        Duration::from_secs(60)
    }
    pub fn entry_delay() -> Duration {
        Duration::from_secs(30)
    }
    pub fn auto_reset() -> Duration {
        Duration::from_secs(10 * 60)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SensorKind {
    /// Door or window contact.
    Contact,
    /// PIR motion sensor.
    Pir,
    /// A wisense WiFi link.
    Wisense,
    Smoke,
    Leak,
}

impl SensorKind {
    /// Modes a sensor of this kind is active in unless the config says
    /// otherwise: doors and windows always, motion only when nobody is home.
    pub fn default_modes(self) -> &'static [Mode] {
        match self {
            SensorKind::Contact => &Mode::ALL,
            SensorKind::Pir | SensorKind::Wisense => &[Mode::Away],
            // Safety sensors ignore modes: they always alert.
            SensorKind::Smoke | SensorKind::Leak => &Mode::ALL,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SensorSpec {
    /// The name used everywhere in homeguard.
    pub id: String,
    pub kind: SensorKind,
    /// Human-readable name for alerts; defaults to the id.
    #[serde(default)]
    pub name: Option<String>,
    /// Modes in which it can raise an alarm; defaults per kind.
    #[serde(default)]
    pub modes: Option<Vec<Mode>>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CameraSpec {
    /// The Frigate camera name.
    pub id: String,
    #[serde(default)]
    pub name: Option<String>,
    /// Defaults to away and night: when someone is home and awake, people on
    /// camera are expected.
    #[serde(default)]
    pub modes: Option<Vec<Mode>>,
}

#[derive(Debug, Clone, PartialEq, Error)]
pub enum ConfigError {
    #[error("duplicate sensor or camera id {0:?}")]
    DuplicateId(String),
    #[error("sensor or camera {0:?} has an empty mode list; remove it or list at least one mode")]
    NoModes(String),
}

/// Everything known about sensors and cameras, indexed by id.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Catalog {
    sensors: BTreeMap<String, SensorSpec>,
    cameras: BTreeMap<String, CameraSpec>,
}

impl Catalog {
    pub fn new(sensors: Vec<SensorSpec>, cameras: Vec<CameraSpec>) -> Result<Self, ConfigError> {
        let mut catalog = Catalog::default();
        for sensor in sensors {
            if sensor.modes.as_ref().is_some_and(Vec::is_empty) {
                return Err(ConfigError::NoModes(sensor.id));
            }
            if catalog.sensors.contains_key(&sensor.id) {
                return Err(ConfigError::DuplicateId(sensor.id));
            }
            catalog.sensors.insert(sensor.id.clone(), sensor);
        }
        for camera in cameras {
            if camera.modes.as_ref().is_some_and(Vec::is_empty) {
                return Err(ConfigError::NoModes(camera.id));
            }
            if catalog.sensors.contains_key(&camera.id) || catalog.cameras.contains_key(&camera.id)
            {
                return Err(ConfigError::DuplicateId(camera.id));
            }
            catalog.cameras.insert(camera.id.clone(), camera);
        }
        Ok(catalog)
    }

    pub fn sensor(&self, id: &str) -> Option<&SensorSpec> {
        self.sensors.get(id)
    }

    pub fn camera(&self, id: &str) -> Option<&CameraSpec> {
        self.cameras.get(id)
    }

    pub fn sensors(&self) -> impl Iterator<Item = &SensorSpec> {
        self.sensors.values()
    }

    pub fn cameras(&self) -> impl Iterator<Item = &CameraSpec> {
        self.cameras.values()
    }

    /// Whether `id` (a sensor or a camera) can raise an alarm in `mode`.
    /// Unknown ids are never active.
    pub fn is_active(&self, id: &str, mode: Mode) -> bool {
        if let Some(sensor) = self.sensors.get(id) {
            return match &sensor.modes {
                Some(modes) => modes.contains(&mode),
                None => sensor.kind.default_modes().contains(&mode),
            };
        }
        if let Some(camera) = self.cameras.get(id) {
            return match &camera.modes {
                Some(modes) => modes.contains(&mode),
                None => matches!(mode, Mode::Away | Mode::Night),
            };
        }
        false
    }

    /// The name to show in alerts.
    pub fn display_name<'a>(&'a self, id: &'a str) -> &'a str {
        self.sensors
            .get(id)
            .and_then(|s| s.name.as_deref())
            .or_else(|| self.cameras.get(id).and_then(|c| c.name.as_deref()))
            .unwrap_or(id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sensor(id: &str, kind: SensorKind, modes: Option<Vec<Mode>>) -> SensorSpec {
        SensorSpec {
            id: id.into(),
            kind,
            name: None,
            modes,
        }
    }

    #[test]
    fn default_modes_per_kind() {
        let catalog = Catalog::new(
            vec![
                sensor("door", SensorKind::Contact, None),
                sensor("pir", SensorKind::Pir, None),
            ],
            vec![CameraSpec {
                id: "cam".into(),
                name: None,
                modes: None,
            }],
        )
        .unwrap();
        for mode in Mode::ALL {
            assert!(catalog.is_active("door", mode));
        }
        assert!(catalog.is_active("pir", Mode::Away));
        assert!(!catalog.is_active("pir", Mode::Home));
        assert!(!catalog.is_active("pir", Mode::Night));
        assert!(catalog.is_active("cam", Mode::Night));
        assert!(!catalog.is_active("cam", Mode::Home));
        assert!(!catalog.is_active("nope", Mode::Away));
    }

    #[test]
    fn explicit_modes_override_defaults() {
        let catalog = Catalog::new(
            vec![sensor(
                "downstairs",
                SensorKind::Pir,
                Some(vec![Mode::Away, Mode::Night]),
            )],
            vec![],
        )
        .unwrap();
        assert!(catalog.is_active("downstairs", Mode::Night));
        assert!(!catalog.is_active("downstairs", Mode::Home));
    }

    #[test]
    fn rejects_duplicates_and_empty_modes() {
        let dup = Catalog::new(
            vec![
                sensor("a", SensorKind::Contact, None),
                sensor("a", SensorKind::Pir, None),
            ],
            vec![],
        );
        assert_eq!(dup, Err(ConfigError::DuplicateId("a".into())));
        let empty = Catalog::new(vec![sensor("a", SensorKind::Pir, Some(vec![]))], vec![]);
        assert_eq!(empty, Err(ConfigError::NoModes("a".into())));
    }
}
