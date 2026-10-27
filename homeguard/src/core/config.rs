//! Settings the core needs, deserialized from `homeguard.toml`.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use serde::Deserialize;
use thiserror::Error;

use super::event::Mode;
use super::time::serde_duration;

/// The core's part of `homeguard.toml`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct CoreConfig {
    /// Every zone of the home, e.g. `["entrance", "living-room"]`.
    pub zones: Vec<String>,
    #[serde(default)]
    pub alarm: AlarmConfig,
    #[serde(default)]
    pub fusion: FusionConfig,
    #[serde(default)]
    pub presence: PresenceRules,
    #[serde(default)]
    pub sensors: Vec<SensorSpec>,
    #[serde(default)]
    pub cameras: Vec<CameraSpec>,
}

impl CoreConfig {
    pub fn catalog(&self) -> Result<Catalog, ConfigError> {
        Catalog::new(&self.zones, self.sensors.clone(), self.cameras.clone())
    }
}

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

/// Rules that combine sensors to cut false alarms.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FusionConfig {
    /// Motion alone opens a window this long, waiting for confirmation.
    #[serde(with = "serde_duration", default = "defaults::confirm_window")]
    pub confirm_window: Duration,
    /// Minimum Frigate score for a person to count.
    #[serde(default = "defaults::person_score")]
    pub person_score: f32,
    /// At most one "unconfirmed motion" notice per zone this often.
    #[serde(with = "serde_duration", default = "defaults::unconfirmed_cooldown")]
    pub unconfirmed_cooldown: Duration,
    /// The same alert (same cause) is sent at most once this often.
    #[serde(with = "serde_duration", default = "defaults::alert_cooldown")]
    pub alert_cooldown: Duration,
    /// A door opening this soon after a known phone arrives is the owner.
    #[serde(with = "serde_duration", default = "defaults::owner_arrival_window")]
    pub owner_arrival_window: Duration,
    /// After an owner arrival, intrusion sensors are ignored this long
    /// (when auto-disarm is off), so walking in does not trip motion.
    #[serde(with = "serde_duration", default = "defaults::owner_grace")]
    pub owner_grace: Duration,
    /// A device must stay offline this long before it counts: Frigate's
    /// camera status flaps while it restarts a stream.
    #[serde(with = "serde_duration", default = "defaults::offline_debounce")]
    pub offline_debounce: Duration,
}

impl Default for FusionConfig {
    fn default() -> Self {
        FusionConfig {
            confirm_window: defaults::confirm_window(),
            person_score: defaults::person_score(),
            unconfirmed_cooldown: defaults::unconfirmed_cooldown(),
            alert_cooldown: defaults::alert_cooldown(),
            owner_arrival_window: defaults::owner_arrival_window(),
            owner_grace: defaults::owner_grace(),
            offline_debounce: defaults::offline_debounce(),
        }
    }
}

/// Known people and what their presence may do to the alarm. The `[presence]`
/// table also holds the shell's probe settings (phone MACs come from the
/// environment), which this struct ignores.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct PresenceRules {
    #[serde(default)]
    pub people: Vec<PersonSpec>,
    /// A phone unseen this long is away: phones sleep and stop answering.
    #[serde(with = "serde_duration", default = "defaults::presence_grace")]
    pub grace: Duration,
    /// Arm (away) when everyone has been away past the grace period.
    #[serde(default)]
    pub auto_arm: bool,
    /// Disarm an away arming when a known person arrives home.
    #[serde(default)]
    pub auto_disarm: bool,
}

impl Default for PresenceRules {
    fn default() -> Self {
        PresenceRules {
            people: Vec::new(),
            grace: defaults::presence_grace(),
            auto_arm: false,
            auto_disarm: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct PersonSpec {
    pub name: String,
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
    pub fn confirm_window() -> Duration {
        Duration::from_secs(20)
    }
    pub fn person_score() -> f32 {
        0.7
    }
    pub fn unconfirmed_cooldown() -> Duration {
        Duration::from_secs(10 * 60)
    }
    pub fn alert_cooldown() -> Duration {
        Duration::from_secs(5 * 60)
    }
    pub fn owner_arrival_window() -> Duration {
        Duration::from_secs(2 * 60)
    }
    pub fn owner_grace() -> Duration {
        Duration::from_secs(5 * 60)
    }
    pub fn offline_debounce() -> Duration {
        Duration::from_secs(60)
    }
    pub fn presence_grace() -> Duration {
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

    /// Whether losing it while armed suggests tampering, unless the config
    /// says otherwise. Entry points, wisense (its process can be killed) and
    /// smoke detectors are; a PIR or leak sensor dropping off is more often a
    /// flat battery.
    pub fn default_critical(self) -> bool {
        matches!(
            self,
            SensorKind::Contact | SensorKind::Wisense | SensorKind::Smoke
        )
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SensorSpec {
    /// The name used everywhere in homeguard.
    pub id: String,
    pub kind: SensorKind,
    /// One of the configured zones.
    pub zone: String,
    /// Human-readable name for alerts; defaults to the id.
    #[serde(default)]
    pub name: Option<String>,
    /// Modes in which it can raise an alarm; defaults per kind.
    #[serde(default)]
    pub modes: Option<Vec<Mode>>,
    /// Going offline while armed is a tampering alert; defaults per kind.
    #[serde(default)]
    pub critical: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CameraSpec {
    /// The Frigate camera name.
    pub id: String,
    /// One of the configured zones.
    pub zone: String,
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
    #[error("{id:?} is in zone {zone:?}, which is not in `zones`")]
    UnknownZone { id: String, zone: String },
    #[error("zone {0:?} is listed twice")]
    DuplicateZone(String),
}

/// Everything known about zones, sensors and cameras, indexed by id.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Catalog {
    zones: BTreeSet<String>,
    sensors: BTreeMap<String, SensorSpec>,
    cameras: BTreeMap<String, CameraSpec>,
}

impl Catalog {
    pub fn new(
        zones: &[String],
        sensors: Vec<SensorSpec>,
        cameras: Vec<CameraSpec>,
    ) -> Result<Self, ConfigError> {
        let mut catalog = Catalog::default();
        for zone in zones {
            if !catalog.zones.insert(zone.clone()) {
                return Err(ConfigError::DuplicateZone(zone.clone()));
            }
        }
        for sensor in sensors {
            catalog.check(&sensor.id, &sensor.zone, sensor.modes.as_deref())?;
            catalog.sensors.insert(sensor.id.clone(), sensor);
        }
        for camera in cameras {
            catalog.check(&camera.id, &camera.zone, camera.modes.as_deref())?;
            catalog.cameras.insert(camera.id.clone(), camera);
        }
        Ok(catalog)
    }

    fn check(&self, id: &str, zone: &str, modes: Option<&[Mode]>) -> Result<(), ConfigError> {
        if modes.is_some_and(<[Mode]>::is_empty) {
            return Err(ConfigError::NoModes(id.into()));
        }
        if self.sensors.contains_key(id) || self.cameras.contains_key(id) {
            return Err(ConfigError::DuplicateId(id.into()));
        }
        if !self.zones.contains(zone) {
            return Err(ConfigError::UnknownZone {
                id: id.into(),
                zone: zone.into(),
            });
        }
        Ok(())
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

    pub fn zones(&self) -> impl Iterator<Item = &str> {
        self.zones.iter().map(String::as_str)
    }

    /// The zone of a sensor or camera.
    pub fn zone_of(&self, id: &str) -> Option<&str> {
        self.sensors
            .get(id)
            .map(|s| s.zone.as_str())
            .or_else(|| self.cameras.get(id).map(|c| c.zone.as_str()))
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

    /// Whether `id` going offline while armed is a tampering alert. Cameras
    /// always are.
    pub fn is_critical(&self, id: &str) -> bool {
        if let Some(sensor) = self.sensors.get(id) {
            return sensor
                .critical
                .unwrap_or_else(|| sensor.kind.default_critical());
        }
        self.cameras.contains_key(id)
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
pub(crate) mod tests {
    use super::*;

    pub(crate) fn sensor(
        id: &str,
        kind: SensorKind,
        zone: &str,
        modes: Option<Vec<Mode>>,
    ) -> SensorSpec {
        SensorSpec {
            id: id.into(),
            kind,
            zone: zone.into(),
            name: None,
            modes,
            critical: None,
        }
    }

    pub(crate) fn camera(id: &str, zone: &str) -> CameraSpec {
        CameraSpec {
            id: id.into(),
            zone: zone.into(),
            name: None,
            modes: None,
        }
    }

    pub(crate) fn zones(names: &[&str]) -> Vec<String> {
        names.iter().map(|z| z.to_string()).collect()
    }

    #[test]
    fn default_modes_per_kind() {
        let catalog = Catalog::new(
            &zones(&["hall"]),
            vec![
                sensor("door", SensorKind::Contact, "hall", None),
                sensor("pir", SensorKind::Pir, "hall", None),
            ],
            vec![camera("cam", "hall")],
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
            &zones(&["hall"]),
            vec![sensor(
                "downstairs",
                SensorKind::Pir,
                "hall",
                Some(vec![Mode::Away, Mode::Night]),
            )],
            vec![],
        )
        .unwrap();
        assert!(catalog.is_active("downstairs", Mode::Night));
        assert!(!catalog.is_active("downstairs", Mode::Home));
    }

    #[test]
    fn zones_and_criticality() {
        let mut pir = sensor("pir", SensorKind::Pir, "hall", None);
        pir.critical = Some(true);
        let catalog = Catalog::new(
            &zones(&["hall", "garden"]),
            vec![
                sensor("door", SensorKind::Contact, "hall", None),
                sensor("leak", SensorKind::Leak, "hall", None),
                pir,
            ],
            vec![camera("cam", "garden")],
        )
        .unwrap();
        assert_eq!(catalog.zone_of("door"), Some("hall"));
        assert_eq!(catalog.zone_of("cam"), Some("garden"));
        assert_eq!(catalog.zone_of("nope"), None);
        assert!(catalog.is_critical("door"));
        assert!(catalog.is_critical("cam"));
        assert!(catalog.is_critical("pir"));
        assert!(!catalog.is_critical("leak"));
        assert!(!catalog.is_critical("nope"));
    }

    #[test]
    fn rejects_bad_catalogs() {
        let hall = zones(&["hall"]);
        let dup = Catalog::new(
            &hall,
            vec![
                sensor("a", SensorKind::Contact, "hall", None),
                sensor("a", SensorKind::Pir, "hall", None),
            ],
            vec![],
        );
        assert_eq!(dup, Err(ConfigError::DuplicateId("a".into())));
        let cam_dup = Catalog::new(
            &hall,
            vec![sensor("a", SensorKind::Contact, "hall", None)],
            vec![camera("a", "hall")],
        );
        assert_eq!(cam_dup, Err(ConfigError::DuplicateId("a".into())));
        let empty = Catalog::new(
            &hall,
            vec![sensor("a", SensorKind::Pir, "hall", Some(vec![]))],
            vec![],
        );
        assert_eq!(empty, Err(ConfigError::NoModes("a".into())));
        let zone = Catalog::new(&hall, vec![], vec![camera("c", "attic")]);
        assert_eq!(
            zone,
            Err(ConfigError::UnknownZone {
                id: "c".into(),
                zone: "attic".into()
            })
        );
        let twice = Catalog::new(&zones(&["hall", "hall"]), vec![], vec![]);
        assert_eq!(twice, Err(ConfigError::DuplicateZone("hall".into())));
    }
}
