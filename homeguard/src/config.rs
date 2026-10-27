//! The whole `homeguard.toml`: the core's settings plus the shell's
//! (MQTT, device names, actuators).
//!
//! Secrets are never written in the file. Settings that need one name the
//! environment variable holding it (`password_env = "..."`), and the values
//! are read from the environment when connecting.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use anyhow::{bail, Context};
use serde::Deserialize;

use crate::core::time::serde_duration;
use crate::core::{CoreConfig, Engine};
use crate::discord::DiscordConfig;
use crate::parse::Mapping;
use crate::presence::ProbeConfig;
use crate::telegram::TelegramConfig;
use crate::web::WebConfig;

#[derive(Debug, Clone)]
pub struct AppConfig {
    pub core: CoreConfig,
    pub mqtt: MqttConfig,
    pub zigbee: ZigbeeConfig,
    pub frigate: FrigateConfig,
    pub wisense: WisenseConfig,
    pub actuators: Actuators,
    pub presence: ProbeConfig,
    pub telegram: TelegramConfig,
    pub discord: DiscordConfig,
    pub web: WebConfig,
    pub log: LogConfig,
}

/// The shell's sections. Parsed separately from [`CoreConfig`] (the same
/// text, read twice) so each side only sees its own tables.
#[derive(Debug, Clone, Default, Deserialize)]
struct ShellSections {
    #[serde(default)]
    mqtt: MqttConfig,
    #[serde(default)]
    zigbee: ZigbeeConfig,
    #[serde(default)]
    frigate: FrigateConfig,
    #[serde(default)]
    wisense: WisenseConfig,
    #[serde(default)]
    actuators: Actuators,
    #[serde(default)]
    presence: ProbeConfig,
    #[serde(default)]
    telegram: TelegramConfig,
    #[serde(default)]
    discord: DiscordConfig,
    #[serde(default)]
    web: WebConfig,
    #[serde(default)]
    log: LogConfig,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct MqttConfig {
    pub host: String,
    pub port: u16,
    pub client_id: String,
    /// Environment variable holding the MQTT user name.
    pub username_env: String,
    /// Environment variable holding the MQTT password.
    pub password_env: String,
    #[serde(with = "serde_duration")]
    pub keep_alive: Duration,
}

impl Default for MqttConfig {
    fn default() -> Self {
        MqttConfig {
            host: "127.0.0.1".into(),
            port: 1883,
            client_id: "homeguard".into(),
            username_env: "MQTT_HOMEGUARD_USER".into(),
            password_env: "MQTT_HOMEGUARD_PASSWORD".into(),
            keep_alive: Duration::from_secs(30),
        }
    }
}

impl MqttConfig {
    /// The user name and password, from the environment.
    pub fn credentials(
        &self,
        env: impl Fn(&str) -> Option<String>,
    ) -> anyhow::Result<(String, String)> {
        let get = |name: &str| -> anyhow::Result<String> {
            match env(name) {
                Some(value) if !value.is_empty() => Ok(value),
                _ => bail!("environment variable {name} is not set (see .env.example)"),
            }
        };
        Ok((get(&self.username_env)?, get(&self.password_env)?))
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct ZigbeeConfig {
    pub base_topic: String,
    /// Zigbee2MQTT friendly name → homeguard sensor id, where they differ.
    pub devices: BTreeMap<String, String>,
}

impl Default for ZigbeeConfig {
    fn default() -> Self {
        ZigbeeConfig {
            base_topic: "zigbee2mqtt".into(),
            devices: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct FrigateConfig {
    pub topic_prefix: String,
    /// Frigate's API, for snapshots (loopback: homeguard runs in the host
    /// network).
    pub url: String,
}

impl Default for FrigateConfig {
    fn default() -> Self {
        FrigateConfig {
            topic_prefix: "frigate".into(),
            url: "http://127.0.0.1:5000".into(),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct WisenseConfig {
    /// wisense link id (e.g. `wlp2s0`) → homeguard sensor id.
    pub links: BTreeMap<String, String>,
}

/// The event log.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct LogConfig {
    /// SQLite file, relative to the data directory.
    pub database: String,
    /// Events older than this are deleted.
    #[serde(with = "serde_duration")]
    pub retention: Duration,
}

impl Default for LogConfig {
    fn default() -> Self {
        LogConfig {
            database: "homeguard.db".into(),
            retention: Duration::from_secs(90 * 86_400),
        }
    }
}

/// Zigbee devices homeguard drives directly through Zigbee2MQTT.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Actuators {
    /// Friendly name of a Zigbee siren (IAS warning device).
    pub siren: Option<String>,
    /// Friendly names of plugs or bulbs to switch on during an alarm.
    pub lights: Vec<String>,
}

impl AppConfig {
    /// Parses and validates the configuration text.
    pub fn parse(text: &str) -> anyhow::Result<AppConfig> {
        let core: CoreConfig = toml::from_str(text).context("invalid configuration")?;
        let shell: ShellSections = toml::from_str(text).context("invalid configuration")?;
        let config = AppConfig {
            core,
            mqtt: shell.mqtt,
            zigbee: shell.zigbee,
            frigate: shell.frigate,
            wisense: shell.wisense,
            actuators: shell.actuators,
            presence: shell.presence,
            telegram: shell.telegram,
            discord: shell.discord,
            web: shell.web,
            log: shell.log,
        };
        config.validate()?;
        Ok(config)
    }

    pub fn load(path: &Path) -> anyhow::Result<AppConfig> {
        let text =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        AppConfig::parse(&text).with_context(|| format!("in {}", path.display()))
    }

    fn validate(&self) -> anyhow::Result<()> {
        let catalog = self.core.catalog()?;
        for (name, id) in self.zigbee.devices.iter().chain(&self.wisense.links) {
            if catalog.sensor(id).is_none() {
                bail!("{name:?} maps to {id:?}, which is not a configured sensor");
            }
        }
        self.presence.sweep()?;
        let probed: Vec<&str> = self
            .presence
            .people
            .iter()
            .map(|p| p.name.as_str())
            .collect();
        let mut seen = std::collections::BTreeSet::new();
        for name in &probed {
            if !seen.insert(*name) {
                bail!("presence: {name:?} is listed twice");
            }
        }
        if self.core.fusion.person_score.is_nan()
            || !(0.0..=1.0).contains(&self.core.fusion.person_score)
        {
            bail!("fusion.person_score must be between 0 and 1");
        }
        Ok(())
    }

    pub fn engine(&self) -> anyhow::Result<Engine> {
        Ok(Engine::new(&self.core)?)
    }

    pub fn mapping(&self) -> Mapping {
        Mapping {
            zigbee_base: self.zigbee.base_topic.clone(),
            zigbee_devices: self.zigbee.devices.clone(),
            frigate_prefix: self.frigate.topic_prefix.clone(),
            cameras: self.core.cameras.iter().map(|c| c.id.clone()).collect(),
            wisense_links: self.wisense.links.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MINIMAL: &str = r#"
zones = ["hall"]

[[sensors]]
id = "door"
kind = "contact"
zone = "hall"
"#;

    #[test]
    fn defaults_apply() {
        let config = AppConfig::parse(MINIMAL).unwrap();
        assert_eq!(config.mqtt, MqttConfig::default());
        assert_eq!(config.zigbee.base_topic, "zigbee2mqtt");
        assert_eq!(config.core.alarm.entry_delay, Duration::from_secs(30));
    }

    #[test]
    fn credentials_come_from_the_environment() {
        let config = AppConfig::parse(MINIMAL).unwrap();
        let env = |name: &str| match name {
            "MQTT_HOMEGUARD_USER" => Some("hg".to_string()),
            "MQTT_HOMEGUARD_PASSWORD" => Some("pw".to_string()),
            _ => None,
        };
        assert_eq!(
            config.mqtt.credentials(env).unwrap(),
            ("hg".to_string(), "pw".to_string())
        );
        let err = config.mqtt.credentials(|_| None).unwrap_err();
        assert!(err.to_string().contains("MQTT_HOMEGUARD_USER"));
    }

    #[test]
    fn rejects_mappings_to_unknown_sensors() {
        let text = format!("{MINIMAL}\n[zigbee.devices]\n\"0xabc\" = \"window\"\n");
        let err = AppConfig::parse(&text).unwrap_err();
        assert!(format!("{err:#}").contains("window"), "{err:#}");
    }

    #[test]
    fn rejects_unknown_keys_in_shell_sections() {
        let text = format!("{MINIMAL}\n[mqtt]\nhots = \"x\"\n");
        assert!(AppConfig::parse(&text).is_err());
    }

    #[test]
    fn presence_people_are_shared_by_core_and_shell() {
        let text = format!(
            "{MINIMAL}\n[presence]\ngrace = \"15m\"\nauto_arm = true\nsubnet = \"192.168.1.0/24\"\n\
             [[presence.people]]\nname = \"alice\"\nmac_env = \"PHONE_ALICE\"\n"
        );
        let config = AppConfig::parse(&text).unwrap();
        assert_eq!(config.core.presence.people[0].name, "alice");
        assert_eq!(config.core.presence.grace, Duration::from_secs(900));
        assert!(config.core.presence.auto_arm);
        assert_eq!(config.presence.people[0].mac_env, "PHONE_ALICE");
        let twice =
            format!("{text}[[presence.people]]\nname = \"alice\"\nmac_env = \"PHONE_ALICE2\"\n");
        assert!(AppConfig::parse(&twice).is_err());
        let sweep = text.replace("192.168.1.0/24", "10.0.0.0/8");
        assert!(AppConfig::parse(&sweep).is_err());
    }

    #[test]
    fn rejects_bad_scores_and_zones() {
        let text = format!("{MINIMAL}\n[fusion]\nperson_score = 1.5\n");
        assert!(AppConfig::parse(&text).is_err());
        let text = MINIMAL.replace("zone = \"hall\"", "zone = \"attic\"");
        assert!(AppConfig::parse(&text).is_err());
    }
}
