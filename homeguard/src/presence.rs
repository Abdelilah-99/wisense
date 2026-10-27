//! Phone presence probing, without root.
//!
//! Every `probe_interval`:
//! 1. read `/proc/net/arp`: a phone whose MAC has a resolved entry answered
//!    the probes sent in the previous round, so it is home;
//! 2. feed the sightings and the time to the pure [`PresenceTracker`],
//!    which decides home/away (with the grace period);
//! 3. probe for the next round: ping each phone's last known IP and send it
//!    a tiny UDP datagram (either makes the kernel resolve its MAC again,
//!    even if the phone ignores pings), and optionally sweep a subnet the
//!    same way to find phones whose IP changed.
//!
//! Reading before probing gives the kernel a whole interval to settle each
//! entry: a phone that left fails its ARP probe and loses its resolved entry
//! instead of looking present from a stale one.

use std::collections::BTreeMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{bail, Context};
use serde::Deserialize;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use crate::core::time::serde_duration;
use crate::core::{Event, PresenceTracker};
use crate::parse::arp::{parse_arp_table, subnet_hosts, Mac};
use crate::run::Clock;

/// The shell's part of `[presence]`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ProbeConfig {
    #[serde(default)]
    pub people: Vec<PersonProbe>,
    #[serde(with = "serde_duration", default = "default_interval")]
    pub probe_interval: Duration,
    /// Optional subnet to sweep, e.g. `"192.168.1.0/24"`.
    #[serde(default)]
    pub subnet: Option<String>,
}

impl Default for ProbeConfig {
    fn default() -> Self {
        ProbeConfig {
            people: Vec::new(),
            probe_interval: default_interval(),
            subnet: None,
        }
    }
}

fn default_interval() -> Duration {
    Duration::from_secs(30)
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct PersonProbe {
    pub name: String,
    /// Environment variable holding the phone's MAC address.
    pub mac_env: String,
    /// The phone's usual IP, if it has a DHCP reservation; speeds up the
    /// first sighting.
    #[serde(default)]
    pub ip: Option<Ipv4Addr>,
}

/// A person with the MAC resolved from the environment.
#[derive(Debug, Clone, PartialEq)]
pub struct Phone {
    pub person: String,
    pub mac: Mac,
    pub ip: Option<Ipv4Addr>,
}

impl ProbeConfig {
    /// Reads every phone's MAC from the environment. A missing or invalid
    /// MAC is an error: that person would otherwise always look away, which
    /// could arm the house while they are inside.
    pub fn phones(&self, env: impl Fn(&str) -> Option<String>) -> anyhow::Result<Vec<Phone>> {
        let mut phones = Vec::new();
        for person in &self.people {
            let Some(text) = env(&person.mac_env).filter(|v| !v.trim().is_empty()) else {
                bail!(
                    "presence: {} needs the phone MAC in environment variable {} (see .env.example)",
                    person.name,
                    person.mac_env
                );
            };
            let mac = Mac::parse(&text).map_err(|_| {
                anyhow::anyhow!(
                    "presence: {} holds {text:?}, not a MAC address like aa:bb:cc:dd:ee:ff",
                    person.mac_env
                )
            })?;
            phones.push(Phone {
                person: person.name.clone(),
                mac,
                ip: person.ip,
            });
        }
        Ok(phones)
    }

    pub fn sweep(&self) -> anyhow::Result<Vec<Ipv4Addr>> {
        match &self.subnet {
            Some(cidr) => subnet_hosts(cidr).with_context(|| format!("presence.subnet {cidr:?}")),
            None => Ok(Vec::new()),
        }
    }
}

/// Sightings in one ARP table: which people answered, and at which IP.
pub fn sightings(phones: &[Phone], arp_table: &str) -> Vec<(String, Ipv4Addr)> {
    let entries = parse_arp_table(arp_table);
    phones
        .iter()
        .filter_map(|phone| {
            entries
                .iter()
                .find(|e| e.complete && e.mac == phone.mac)
                .map(|e| (phone.person.clone(), e.ip))
        })
        .collect()
}

pub fn warn_about_randomized_macs(phones: &[Phone]) {
    for phone in phones {
        if phone.mac.is_locally_administered() {
            warn!(
                person = %phone.person,
                mac = %phone.mac,
                "this MAC is locally administered, as phones make up when MAC \
                 randomization (\"private address\") is on; disable it for the home \
                 WiFi on that phone or it will not be recognized reliably"
            );
        }
    }
}

/// The probe loop. Sends `PresenceChanged` events to the core.
pub async fn run(
    phones: Vec<Phone>,
    sweep: Vec<Ipv4Addr>,
    interval: Duration,
    grace: Duration,
    clock: Clock,
    events: mpsc::Sender<Event>,
) {
    let mut tracker =
        PresenceTracker::new(phones.iter().map(|p| p.person.clone()), grace, clock.now());
    let mut last_ip: BTreeMap<String, Ipv4Addr> = phones
        .iter()
        .filter_map(|p| p.ip.map(|ip| (p.person.clone(), ip)))
        .collect();
    let socket = match tokio::net::UdpSocket::bind("0.0.0.0:0").await {
        Ok(socket) => Some(socket),
        Err(e) => {
            warn!(error = %e, "presence: no UDP socket; probing with ping only");
            None
        }
    };
    let mut ping_available = true;
    info!(people = phones.len(), every = ?interval, "presence probing started");
    let mut ticker = tokio::time::interval(interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        ticker.tick().await;

        let table = match tokio::fs::read_to_string("/proc/net/arp").await {
            Ok(table) => table,
            Err(e) => {
                warn!(error = %e, "presence: cannot read /proc/net/arp");
                String::new()
            }
        };
        let now = clock.now();
        let mut changes = Vec::new();
        for (person, ip) in sightings(&phones, &table) {
            debug!(%person, %ip, "phone answered");
            last_ip.insert(person.clone(), ip);
            changes.extend(tracker.sighting(now, &person));
        }
        changes.extend(tracker.tick(now));
        for event in changes {
            if events.send(event).await.is_err() {
                return;
            }
        }

        for ip in last_ip.values() {
            if let Some(socket) = &socket {
                poke(socket, *ip).await;
            }
            if ping_available {
                ping_available = ping(*ip).await;
            }
        }
        if let Some(socket) = &socket {
            for ip in &sweep {
                poke(socket, *ip).await;
            }
        }
    }
}

/// A one-byte UDP datagram to the discard port: the kernel has to resolve
/// the MAC to send it, which is all we want.
async fn poke(socket: &tokio::net::UdpSocket, ip: Ipv4Addr) {
    let target = SocketAddr::new(IpAddr::V4(ip), 9);
    if let Err(e) = socket.send_to(&[0], target).await {
        debug!(%ip, error = %e, "presence: UDP probe failed");
    }
}

/// Pings once without waiting for the result. Returns `false` if `ping`
/// cannot be run at all, so the caller stops trying.
async fn ping(ip: Ipv4Addr) -> bool {
    let child = tokio::process::Command::new("ping")
        .args(["-c", "1", "-W", "1", &ip.to_string()])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn();
    match child {
        Ok(mut child) => {
            tokio::spawn(async move {
                let _ = tokio::time::timeout(Duration::from_secs(3), child.wait()).await;
            });
            true
        }
        Err(e) => {
            warn!(error = %e, "presence: cannot run ping; using UDP probes only");
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> ProbeConfig {
        toml::from_str(
            r#"
            subnet = "192.168.1.0/24"
            [[people]]
            name = "alice"
            mac_env = "PHONE_ALICE"
            [[people]]
            name = "bob"
            mac_env = "PHONE_BOB"
            ip = "192.168.1.50"
            "#,
        )
        .unwrap()
    }

    fn env(name: &str) -> Option<String> {
        match name {
            "PHONE_ALICE" => Some("02:00:00:00:00:17".into()),
            "PHONE_BOB" => Some("02-00-00-00-00-AB".into()),
            _ => None,
        }
    }

    #[test]
    fn phones_need_their_mac_in_the_environment() {
        let phones = config().phones(env).unwrap();
        assert_eq!(phones.len(), 2);
        assert_eq!(phones[1].ip, Some(Ipv4Addr::new(192, 168, 1, 50)));
        let err = config().phones(|_| None).unwrap_err();
        assert!(err.to_string().contains("PHONE_ALICE"));
        let err = config().phones(|_| Some("not-a-mac".into())).unwrap_err();
        assert!(err.to_string().contains("not a MAC"));
        assert_eq!(config().probe_interval, Duration::from_secs(30));
        assert_eq!(config().sweep().unwrap().len(), 254);
    }

    #[test]
    fn sightings_need_a_resolved_entry() {
        let phones = config().phones(env).unwrap();
        let table = "\
IP address       HW type     Flags       HW address            Mask     Device
192.168.1.23     0x1         0x2         02:00:00:00:00:17     *        wlp2s0
192.168.1.50     0x1         0x0         02:00:00:00:00:ab     *        wlp2s0
";
        assert_eq!(
            sightings(&phones, table),
            vec![("alice".to_string(), Ipv4Addr::new(192, 168, 1, 23))]
        );
        assert!(sightings(&phones, "").is_empty());
    }
}
