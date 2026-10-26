//! Sensor fusion on top of the state machine: the rules that decide whether
//! an event is an intrusion, so that a flaky sensor cannot cause an alarm.
//!
//! - A door or window opening while armed starts the entry delay.
//! - Motion alone (wisense, PIR) is not an alarm. It opens a confirmation
//!   window in its zone. A person on a camera in the same zone, or motion
//!   from a different kind of source, within the window triggers the alarm
//!   at once. Without confirmation it is logged, with a low-priority notice
//!   at most once per cooldown.
//! - A door opening shortly after a known phone came home is the owner.
//! - A camera or critical sensor going offline while armed is reported as
//!   possible tampering.
//! - Repeated alerts with the same cause are sent at most once per cooldown.
//!
//! [`Engine`] is the single entry point the shell feeds; like the machine it
//! is pure, so every rule is tested with exact timings below.

use std::collections::{BTreeMap, BTreeSet};

use super::config::{Catalog, ConfigError, CoreConfig, FusionConfig, PresenceRules};
use super::event::{Action, AlertLevel, Command, Event, Input, MotionSource};
use super::machine::{describe, AlarmMachine, Delay, Hazard};
use super::state::{Cause, State};
use super::time::{format_duration, Ts};

/// Motion waiting for confirmation in one zone.
#[derive(Debug, Clone, PartialEq)]
pub struct Window {
    pub opened: Ts,
    pub until: Ts,
    /// Kinds of source seen so far; two different kinds confirm.
    pub sources: BTreeSet<MotionSource>,
    /// The sensors or cameras that reported, in order.
    pub reporters: Vec<String>,
    /// Set when a camera saw a person: (camera, Frigate event id).
    pub person: Option<(String, Option<String>)>,
}

#[derive(Debug, Clone)]
pub struct Engine {
    machine: AlarmMachine,
    fusion: FusionConfig,
    presence: PresenceRules,
    windows: BTreeMap<String, Window>,
    /// Last "unconfirmed motion" notice per zone.
    last_notice: BTreeMap<String, Ts>,
    /// Last time each de-duplicated alert was sent, by cause key.
    last_alert: BTreeMap<String, Ts>,
    /// When each known person was last seen arriving.
    arrivals: BTreeMap<String, Ts>,
    home: BTreeMap<String, bool>,
    contacts: BTreeMap<String, bool>,
    /// Reported offline, waiting for the debounce to pass.
    going_offline: BTreeMap<String, Ts>,
    offline: BTreeSet<String>,
    /// Frigate events already used, so their many updates are not re-processed.
    seen_person_events: BTreeMap<String, Ts>,
    /// Intrusion sensors ignored until then: an owner is walking in.
    owner_grace_until: Option<Ts>,
}

impl Engine {
    pub fn new(config: &CoreConfig) -> Result<Self, ConfigError> {
        let catalog = config.catalog()?;
        Ok(Engine {
            machine: AlarmMachine::new(config.alarm.clone(), catalog),
            fusion: config.fusion.clone(),
            presence: config.presence.clone(),
            windows: BTreeMap::new(),
            last_notice: BTreeMap::new(),
            last_alert: BTreeMap::new(),
            arrivals: BTreeMap::new(),
            home: BTreeMap::new(),
            contacts: BTreeMap::new(),
            going_offline: BTreeMap::new(),
            offline: BTreeSet::new(),
            seen_person_events: BTreeMap::new(),
            owner_grace_until: None,
        })
    }

    pub fn state(&self) -> &State {
        self.machine.state()
    }

    pub fn catalog(&self) -> &Catalog {
        self.machine.catalog()
    }

    /// Open confirmation windows, by zone.
    pub fn windows(&self) -> &BTreeMap<String, Window> {
        &self.windows
    }

    /// Last known contact states (`true` = open).
    pub fn contacts(&self) -> &BTreeMap<String, bool> {
        &self.contacts
    }

    /// Devices confirmed offline.
    pub fn offline(&self) -> &BTreeSet<String> {
        &self.offline
    }

    /// Who is home, as last reported.
    pub fn presence(&self) -> &BTreeMap<String, bool> {
        &self.home
    }

    /// The next time a `Tick` would change something.
    pub fn next_deadline(&self) -> Option<Ts> {
        let windows = self.windows.values().map(|w| w.until);
        let offline = self
            .going_offline
            .values()
            .map(|since| *since + self.fusion.offline_debounce);
        self.machine
            .next_deadline()
            .into_iter()
            .chain(windows)
            .chain(offline)
            .min()
    }

    pub fn handle(&mut self, input: &Input) -> Vec<Action> {
        let at = input.at;
        let mut actions = self.advance(at);
        actions.extend(match &input.event {
            Event::Tick => Vec::new(),
            Event::Command { command, by } => self.command(at, *command, by),
            Event::ContactChanged { sensor, open } => self.contact(at, sensor, *open),
            Event::MotionDetected { source, sensor } => self.motion(at, *source, sensor),
            Event::PersonDetected {
                camera,
                score,
                snapshot_ref,
            } => self.person(at, camera, *score, snapshot_ref.as_deref()),
            Event::PresenceChanged { person, home } => self.presence_changed(at, person, *home),
            Event::DeviceOffline { device } => self.device_offline(at, device),
            Event::DeviceOnline { device } => self.device_online(device),
            Event::SmokeDetected { sensor } => self.hazard(at, Hazard::Smoke, sensor),
            Event::LeakDetected { sensor } => self.hazard(at, Hazard::Leak, sensor),
        });
        if !self.state().is_guarding() {
            self.windows.clear();
        }
        actions
    }

    /// Everything that happens just because time passed.
    fn advance(&mut self, at: Ts) -> Vec<Action> {
        let mut actions = self.machine.tick(at);

        let expired: Vec<String> = self
            .windows
            .iter()
            .filter(|(_, w)| w.until <= at)
            .map(|(zone, _)| zone.clone())
            .collect();
        for zone in expired {
            if let Some(window) = self.windows.remove(&zone) {
                actions.extend(self.unconfirmed(window.until, &zone, window));
            }
        }

        let confirmed: Vec<String> = self
            .going_offline
            .iter()
            .filter(|(_, since)| **since + self.fusion.offline_debounce <= at)
            .map(|(device, _)| device.clone())
            .collect();
        for device in confirmed {
            if let Some(since) = self.going_offline.remove(&device) {
                actions.extend(self.confirm_offline(since + self.fusion.offline_debounce, &device));
            }
        }

        if self.owner_grace_until.is_some_and(|until| until <= at) {
            self.owner_grace_until = None;
        }
        // Frigate events last minutes at most; forget them after an hour.
        let horizon = std::time::Duration::from_secs(3600);
        self.seen_person_events
            .retain(|_, first| *first + horizon > at);
        actions
    }

    fn command(&mut self, at: Ts, command: Command, by: &str) -> Vec<Action> {
        if command == Command::Disarm {
            self.owner_grace_until = None;
        }
        self.machine.command(at, command, by)
    }

    fn contact(&mut self, at: Ts, sensor: &str, open: bool) -> Vec<Action> {
        // Zigbee sensors repeat their state; only changes matter.
        if self.contacts.insert(sensor.to_string(), open) == Some(open) {
            return Vec::new();
        }
        let name = self.catalog().display_name(sensor).to_string();
        if !open || !self.machine.is_watching(sensor) {
            return vec![Action::log(describe(&Event::ContactChanged {
                sensor: sensor.into(),
                open,
            }))];
        }
        if matches!(self.state(), State::Armed { .. }) {
            if let Some((person, ago)) = self.recent_arrival(at) {
                return self.owner_arriving(at, &name, &person, ago);
            }
            if self.in_owner_grace(at) {
                return vec![Action::log(format!(
                    "{name} opened during owner-arrival grace; ignored"
                ))];
            }
        }
        let cause = Cause::new(sensor, format!("{name} opened"));
        self.machine.intrusion(at, cause, Delay::Entry)
    }

    /// The most recent arrival within the owner-arrival window.
    fn recent_arrival(&self, at: Ts) -> Option<(String, std::time::Duration)> {
        self.arrivals
            .iter()
            .filter(|(_, arrived)| **arrived <= at)
            .map(|(person, arrived)| (person.clone(), at.since(*arrived)))
            .filter(|(_, ago)| *ago <= self.fusion.owner_arrival_window)
            .min_by_key(|(_, ago)| *ago)
    }

    fn in_owner_grace(&self, at: Ts) -> bool {
        self.owner_grace_until.is_some_and(|until| at < until)
    }

    fn owner_arriving(
        &mut self,
        at: Ts,
        door: &str,
        person: &str,
        ago: std::time::Duration,
    ) -> Vec<Action> {
        let ago = format_duration(ago);
        if self.presence.auto_disarm {
            let mut actions = vec![Action::log(format!(
                "{door} opened {ago} after {person}'s phone arrived: owner arriving"
            ))];
            actions.extend(self.command(at, Command::Disarm, &format!("presence ({person})")));
            return actions;
        }
        self.owner_grace_until = Some(at + self.fusion.owner_grace);
        let mode = self.state().mode().map(|m| m.as_str()).unwrap_or("?");
        vec![Action::alert(
            AlertLevel::Info,
            format!(
                "{door} opened {ago} after {person}'s phone arrived: treated as {person} \
                 coming home, no alarm. Still armed ({mode}); sensors are ignored for {}, \
                 disarm once inside.",
                format_duration(self.fusion.owner_grace)
            ),
        )]
    }

    fn motion(&mut self, at: Ts, source: MotionSource, sensor: &str) -> Vec<Action> {
        let name = self.catalog().display_name(sensor).to_string();
        let logged = || {
            vec![Action::log(describe(&Event::MotionDetected {
                source,
                sensor: sensor.into(),
            }))]
        };
        if !self.machine.is_watching(sensor) || !matches!(self.state(), State::Armed { .. }) {
            return logged();
        }
        if self.in_owner_grace(at) {
            return logged();
        }
        let Some(zone) = self.catalog().zone_of(sensor).map(str::to_string) else {
            return logged();
        };

        if let Some(window) = self.windows.get_mut(&zone) {
            if let Some((camera, snapshot)) = window.person.clone() {
                let camera_name = self.machine.catalog().display_name(&camera).to_string();
                let cause = Cause::new(
                    sensor,
                    format!("motion on {name} ({source}) confirmed by a person on {camera_name}"),
                )
                .with_snapshot(snapshot);
                self.windows.remove(&zone);
                return self.machine.intrusion(at, cause, Delay::Immediate);
            }
            if !window.sources.contains(&source) {
                let first = window.reporters.first().cloned().unwrap_or_default();
                let first_name = self.machine.catalog().display_name(&first).to_string();
                let cause = Cause::new(
                    sensor,
                    format!("motion in {zone} on {first_name} and {name} ({source})"),
                );
                self.windows.remove(&zone);
                return self.machine.intrusion(at, cause, Delay::Immediate);
            }
            // Same kind of source again: no new evidence, and the window is
            // not extended, so a flaky sensor cannot keep it open.
            window.reporters.push(sensor.to_string());
            return logged();
        }

        self.open_window(at, &zone, source, sensor, None);
        vec![Action::log(format!(
            "motion on {name} ({source}) in {zone}; waiting {} for confirmation",
            format_duration(self.fusion.confirm_window)
        ))]
    }

    fn person(&mut self, at: Ts, camera: &str, score: f32, snapshot: Option<&str>) -> Vec<Action> {
        let name = self.catalog().display_name(camera).to_string();
        let logged = || vec![Action::log(format!("person on {name} (score {score:.2})"))];
        if !score.is_finite() || score < self.fusion.person_score {
            return logged();
        }
        if !self.machine.is_watching(camera) || !matches!(self.state(), State::Armed { .. }) {
            return logged();
        }
        if self.in_owner_grace(at) {
            return logged();
        }
        // Frigate publishes many updates per tracked person; act once.
        if let Some(id) = snapshot {
            if self.seen_person_events.contains_key(id) {
                return Vec::new();
            }
            self.seen_person_events.insert(id.to_string(), at);
        }
        let Some(zone) = self.catalog().zone_of(camera).map(str::to_string) else {
            return logged();
        };
        let snapshot = snapshot.map(str::to_string);

        if let Some(window) = self.windows.get_mut(&zone) {
            let others: Vec<String> = window
                .reporters
                .iter()
                .filter(|r| self.machine.catalog().camera(r).is_none())
                .cloned()
                .collect();
            if let Some(first) = others.first() {
                let first_name = self.machine.catalog().display_name(first).to_string();
                let cause = Cause::new(
                    camera,
                    format!(
                        "motion on {first_name} confirmed by a person on {name} \
                         (score {score:.2})"
                    ),
                )
                .with_snapshot(snapshot);
                self.windows.remove(&zone);
                return self.machine.intrusion(at, cause, Delay::Immediate);
            }
            // Another camera sighting in the same window: keep the latest.
            window.person = Some((camera.to_string(), snapshot));
            window.reporters.push(camera.to_string());
            return logged();
        }

        self.open_window(
            at,
            &zone,
            MotionSource::Camera,
            camera,
            Some((camera.to_string(), snapshot)),
        );
        vec![Action::log(format!(
            "person on {name} (score {score:.2}) in {zone}; waiting {} for confirmation",
            format_duration(self.fusion.confirm_window)
        ))]
    }

    fn open_window(
        &mut self,
        at: Ts,
        zone: &str,
        source: MotionSource,
        reporter: &str,
        person: Option<(String, Option<String>)>,
    ) {
        self.windows.insert(
            zone.to_string(),
            Window {
                opened: at,
                until: at + self.fusion.confirm_window,
                sources: BTreeSet::from([source]),
                reporters: vec![reporter.to_string()],
                person,
            },
        );
    }

    /// A window closed without confirmation.
    fn unconfirmed(&mut self, at: Ts, zone: &str, window: Window) -> Vec<Action> {
        let names: Vec<&str> = window
            .reporters
            .iter()
            .map(|r| self.machine.catalog().display_name(r))
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        let what = if window.person.is_some() {
            "person on camera"
        } else {
            "motion"
        };
        let summary = format!(
            "unconfirmed {what} in {zone} ({}, {} report{})",
            names.join(", "),
            window.reporters.len(),
            if window.reporters.len() == 1 { "" } else { "s" }
        );
        let mut actions = vec![Action::log(summary.clone())];
        let due = self
            .last_notice
            .get(zone)
            .is_none_or(|last| at.since(*last) >= self.fusion.unconfirmed_cooldown);
        if due {
            self.last_notice.insert(zone.to_string(), at);
            actions.push(Action::SendAlert {
                level: AlertLevel::Low,
                text: format!("Unconfirmed: {summary}. No alarm."),
                snapshot_ref: window.person.and_then(|(_, snapshot)| snapshot),
            });
        }
        actions
    }

    fn presence_changed(&mut self, at: Ts, person: &str, home: bool) -> Vec<Action> {
        let was_home = self.home.insert(person.to_string(), home);
        if was_home == Some(home) {
            return Vec::new();
        }
        if home {
            self.arrivals.insert(person.to_string(), at);
        }
        vec![Action::log(describe(&Event::PresenceChanged {
            person: person.into(),
            home,
        }))]
    }

    fn device_offline(&mut self, at: Ts, device: &str) -> Vec<Action> {
        if self.offline.contains(device) || self.going_offline.contains_key(device) {
            return Vec::new();
        }
        self.going_offline.insert(device.to_string(), at);
        if self.fusion.offline_debounce.is_zero() {
            self.going_offline.remove(device);
            return self.confirm_offline(at, device);
        }
        vec![Action::log(format!("{device} reported offline"))]
    }

    fn confirm_offline(&mut self, at: Ts, device: &str) -> Vec<Action> {
        self.offline.insert(device.to_string());
        let name = self.catalog().display_name(device).to_string();
        let guarding = self.state().is_guarding() || matches!(self.state(), State::Arming { .. });
        if guarding && self.catalog().is_critical(device) {
            let alert = Action::alert(
                AlertLevel::High,
                format!("{name} went offline while armed: possible tampering. Check it."),
            );
            let mut actions = vec![Action::log(format!("{device} offline"))];
            actions.extend(self.once(at, &format!("offline:{device}"), alert));
            return actions;
        }
        vec![Action::log(format!("{device} offline"))]
    }

    fn device_online(&mut self, device: &str) -> Vec<Action> {
        if self.going_offline.remove(device).is_some() {
            return vec![Action::log(format!(
                "{device} back online within the offline debounce"
            ))];
        }
        if self.offline.remove(device) {
            let name = self.catalog().display_name(device).to_string();
            let mut actions = vec![Action::log(format!("{device} online"))];
            if self.state().is_guarding() && self.catalog().is_critical(device) {
                actions.push(Action::alert(
                    AlertLevel::Info,
                    format!("{name} is back online."),
                ));
            }
            return actions;
        }
        // Heartbeats from devices that never left.
        Vec::new()
    }

    fn hazard(&mut self, at: Ts, hazard: Hazard, sensor: &str) -> Vec<Action> {
        let key = match hazard {
            Hazard::Smoke => format!("smoke:{sensor}"),
            Hazard::Leak => format!("leak:{sensor}"),
        };
        let mut actions = Vec::new();
        for action in self.machine.safety(hazard, sensor) {
            match action {
                Action::SendAlert { .. } => actions.extend(self.once(at, &key, action)),
                other => actions.push(other),
            }
        }
        actions
    }

    /// `alert` unless the same cause was alerted within the cooldown, in
    /// which case only a log line.
    fn once(&mut self, at: Ts, key: &str, alert: Action) -> Option<Action> {
        let due = self
            .last_alert
            .get(key)
            .is_none_or(|last| at.since(*last) >= self.fusion.alert_cooldown);
        if due {
            self.last_alert.insert(key.to_string(), at);
            Some(alert)
        } else {
            Some(Action::log(format!("{key}: alert suppressed (cooldown)")))
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::core::config::tests::{camera, sensor, zones};
    use crate::core::config::SensorKind;
    use crate::core::event::Mode;

    /// Living room: wisense link + PIR + camera. Entrance: door + camera.
    /// Garden: camera only. Defaults: entry 30 s, window 20 s, person ≥ 0.7,
    /// unconfirmed cooldown 10 min, alert cooldown 5 min, offline debounce 60 s.
    fn config() -> CoreConfig {
        CoreConfig {
            zones: zones(&["entrance", "living-room", "garden"]),
            alarm: Default::default(),
            fusion: Default::default(),
            presence: Default::default(),
            sensors: vec![
                sensor("front_door", SensorKind::Contact, "entrance", None),
                sensor("living_wisense", SensorKind::Wisense, "living-room", None),
                sensor("living_pir", SensorKind::Pir, "living-room", None),
                sensor("kitchen_smoke", SensorKind::Smoke, "living-room", None),
                sensor("hall_pir", SensorKind::Pir, "entrance", None),
            ],
            cameras: vec![
                camera("living_cam", "living-room"),
                camera("door_cam", "entrance"),
                camera("garden_cam", "garden"),
            ],
        }
    }

    struct Rig {
        engine: Engine,
        log: Vec<(Ts, Action)>,
    }

    impl Rig {
        fn new(config: CoreConfig) -> Rig {
            Rig {
                engine: Engine::new(&config).unwrap(),
                log: Vec::new(),
            }
        }

        /// Disarmed → armed away at t = 0 (exit delay 60 s), returned at t = 60.
        fn armed() -> Rig {
            let mut rig = Rig::new(config());
            rig.send(0, command(Command::Arm(Mode::Away)));
            rig.send(60, Event::Tick);
            assert_eq!(rig.engine.state(), &State::Armed { mode: Mode::Away });
            rig.log.clear();
            rig
        }

        fn send(&mut self, secs: u64, event: Event) -> Vec<Action> {
            self.send_ms(secs * 1000, event)
        }

        fn send_ms(&mut self, ms: u64, event: Event) -> Vec<Action> {
            let at = Ts::from_millis(ms);
            let actions = self.engine.handle(&Input::new(at, event));
            self.log.extend(actions.iter().cloned().map(|a| (at, a)));
            actions
        }

        fn state(&self) -> &'static str {
            self.engine.state().name()
        }

        fn alerts(&self, level: AlertLevel) -> Vec<&str> {
            self.log
                .iter()
                .filter_map(|(_, a)| match a {
                    Action::SendAlert { level: l, text, .. } if *l == level => Some(text.as_str()),
                    _ => None,
                })
                .collect()
        }
    }

    fn command(command: Command) -> Event {
        Event::Command {
            command,
            by: "test".into(),
        }
    }

    fn motion(source: MotionSource, sensor: &str) -> Event {
        Event::MotionDetected {
            source,
            sensor: sensor.into(),
        }
    }

    fn wisense() -> Event {
        motion(MotionSource::Wisense, "living_wisense")
    }

    fn person(camera: &str, score: f32, id: &str) -> Event {
        Event::PersonDetected {
            camera: camera.into(),
            score,
            snapshot_ref: Some(id.into()),
        }
    }

    fn door(open: bool) -> Event {
        Event::ContactChanged {
            sensor: "front_door".into(),
            open,
        }
    }

    fn arrive(person: &str) -> Event {
        Event::PresenceChanged {
            person: person.into(),
            home: true,
        }
    }

    #[test]
    fn door_opened_while_armed_goes_pending_then_triggered() {
        let mut rig = Rig::armed();
        rig.send(100, door(true));
        assert_eq!(rig.state(), "pending");
        rig.send(130, Event::Tick);
        assert_eq!(rig.state(), "triggered");
        assert!(rig.log.iter().any(|(_, a)| *a == Action::SirenOn));
    }

    #[test]
    fn repeated_contact_reports_are_not_new_openings() {
        let mut rig = Rig::armed();
        rig.send(61, door(false));
        rig.send(100, door(true));
        rig.send(101, command(Command::Disarm));
        rig.send(102, command(Command::Arm(Mode::Away)));
        rig.send(162, Event::Tick);
        // The sensor re-reports "open": the door did not open again.
        rig.send(170, door(true));
        assert_eq!(rig.state(), "armed");
        // Closing then opening again does count.
        rig.send(171, door(false));
        rig.send(172, door(true));
        assert_eq!(rig.state(), "pending");
    }

    #[test]
    fn motion_alone_is_not_an_alarm() {
        let mut rig = Rig::armed();
        rig.send(100, wisense());
        assert_eq!(rig.state(), "armed");
        assert_eq!(
            rig.engine.windows()["living-room"].until,
            Ts::from_secs(120)
        );
        assert_eq!(rig.engine.next_deadline(), Some(Ts::from_secs(120)));

        rig.send_ms(119_999, Event::Tick);
        assert!(rig.alerts(AlertLevel::Low).is_empty());
        rig.send(120, Event::Tick);
        assert_eq!(rig.state(), "armed");
        assert!(rig.engine.windows().is_empty());
        let notices = rig.alerts(AlertLevel::Low);
        assert_eq!(notices.len(), 1);
        assert!(notices[0].contains("Unconfirmed"), "{notices:?}");
        assert!(rig.alerts(AlertLevel::Critical).is_empty());
    }

    #[test]
    fn motion_confirmed_by_a_person_triggers_with_the_snapshot() {
        let mut rig = Rig::armed();
        rig.send(100, wisense());
        let actions = rig.send(119, person("living_cam", 0.82, "evt-1"));
        assert!(matches!(
            rig.engine.state(),
            State::Triggered { since, cause, .. }
                if *since == Ts::from_secs(119)
                    && cause.snapshot_ref.as_deref() == Some("evt-1")
        ));
        assert!(actions.iter().any(|a| matches!(
            a,
            Action::SendAlert { level: AlertLevel::Critical, snapshot_ref: Some(s), .. } if s == "evt-1"
        )));
    }

    #[test]
    fn person_first_then_motion_also_confirms() {
        let mut rig = Rig::armed();
        rig.send(100, person("living_cam", 0.9, "evt-2"));
        assert_eq!(rig.state(), "armed");
        rig.send(110, motion(MotionSource::Pir, "living_pir"));
        assert!(matches!(
            rig.engine.state(),
            State::Triggered { cause, .. } if cause.snapshot_ref.as_deref() == Some("evt-2")
        ));
    }

    #[test]
    fn low_score_or_other_zone_or_late_person_does_not_confirm() {
        // Below the threshold.
        let mut rig = Rig::armed();
        rig.send(100, wisense());
        rig.send(105, person("living_cam", 0.69, "evt-3"));
        assert_eq!(rig.state(), "armed");

        // Right score, wrong zone (garden camera, living-room motion).
        let mut rig = Rig::armed();
        rig.send(100, wisense());
        rig.send(105, person("garden_cam", 0.95, "evt-4"));
        assert_eq!(rig.state(), "armed");

        // Right zone, but the window closed at t = 120.
        let mut rig = Rig::armed();
        rig.send(100, wisense());
        rig.send(120, person("living_cam", 0.95, "evt-5"));
        assert_eq!(rig.state(), "armed");
    }

    #[test]
    fn a_threshold_score_counts() {
        let mut rig = Rig::armed();
        rig.send(100, wisense());
        rig.send(105, person("living_cam", 0.7, "evt-6"));
        assert_eq!(rig.state(), "triggered");
    }

    #[test]
    fn two_independent_sources_in_one_zone_trigger_without_a_camera() {
        let mut rig = Rig::armed();
        rig.send(100, wisense());
        rig.send(115, motion(MotionSource::Pir, "living_pir"));
        assert!(matches!(
            rig.engine.state(),
            State::Triggered { cause, .. } if cause.snapshot_ref.is_none()
        ));
    }

    #[test]
    fn sources_in_different_zones_do_not_combine() {
        let mut rig = Rig::armed();
        rig.send(100, wisense());
        rig.send(105, motion(MotionSource::Pir, "hall_pir"));
        assert_eq!(rig.state(), "armed");
        assert_eq!(rig.engine.windows().len(), 2);
    }

    #[test]
    fn fast_flaky_motion_without_a_person_never_alarms_and_notices_are_rate_limited() {
        let mut rig = Rig::armed();
        // wisense fires every 3 s for 30 minutes, nobody on camera.
        for i in 0..600 {
            rig.send(100 + i * 3, wisense());
        }
        rig.send(100 + 600 * 3 + 30, Event::Tick);
        assert_eq!(rig.state(), "armed");
        assert!(rig.alerts(AlertLevel::Critical).is_empty());
        assert!(rig.alerts(AlertLevel::High).is_empty());
        // 30 minutes of windows, one notice per 10 minutes at most: at
        // about 0, 10 and 20 minutes.
        let notices: Vec<Ts> = rig
            .log
            .iter()
            .filter(|(_, a)| {
                matches!(
                    a,
                    Action::SendAlert {
                        level: AlertLevel::Low,
                        ..
                    }
                )
            })
            .map(|(at, _)| *at)
            .collect();
        assert_eq!(notices.len(), 3, "{notices:?}");
        for pair in notices.windows(2) {
            assert!(pair[1].since(pair[0]) >= Duration::from_secs(600));
        }
    }

    #[test]
    fn unconfirmed_cooldown_is_per_zone() {
        let mut rig = Rig::armed();
        rig.send(100, wisense());
        rig.send(100, motion(MotionSource::Pir, "hall_pir"));
        rig.send(120, Event::Tick);
        assert_eq!(rig.alerts(AlertLevel::Low).len(), 2);
    }

    #[test]
    fn same_frigate_event_is_used_once() {
        let mut rig = Rig::armed();
        rig.send(100, person("garden_cam", 0.9, "evt-7"));
        rig.send(121, Event::Tick);
        // Frigate keeps sending updates for the same tracked person.
        for t in 122..140 {
            rig.send(t, person("garden_cam", 0.9, "evt-7"));
        }
        assert!(rig.engine.windows().is_empty());
        assert_eq!(rig.alerts(AlertLevel::Low).len(), 1);
    }

    #[test]
    fn motion_is_ignored_when_inactive_in_the_mode() {
        let mut rig = Rig::new(config());
        rig.send(0, command(Command::Arm(Mode::Home)));
        rig.send(60, Event::Tick);
        rig.send(100, wisense());
        rig.send(101, motion(MotionSource::Pir, "living_pir"));
        assert_eq!(rig.state(), "armed");
        assert!(rig.engine.windows().is_empty());
    }

    #[test]
    fn owner_arriving_is_not_an_alarm() {
        let mut rig = Rig::armed();
        rig.send(1000, arrive("alice"));
        rig.send(1090, door(true));
        assert_eq!(rig.state(), "armed");
        let info = rig.alerts(AlertLevel::Info);
        assert!(info.iter().any(|t| t.contains("alice")), "{info:?}");
        // Walking in: motion during the grace period is ignored too.
        rig.send(1100, wisense());
        rig.send(1105, motion(MotionSource::Pir, "living_pir"));
        assert_eq!(rig.state(), "armed");
        // After the grace period (5 min) the system guards again.
        rig.send(1090 + 300, wisense());
        rig.send(1090 + 305, motion(MotionSource::Pir, "living_pir"));
        assert_eq!(rig.state(), "triggered");
    }

    #[test]
    fn owner_arriving_auto_disarms_when_enabled() {
        let mut config = config();
        config.presence.auto_disarm = true;
        let mut rig = Rig::new(config);
        rig.send(0, command(Command::Arm(Mode::Away)));
        rig.send(60, Event::Tick);
        rig.send(1000, arrive("bob"));
        rig.send(1119, door(true));
        assert_eq!(rig.state(), "disarmed");
    }

    #[test]
    fn arrival_too_long_before_the_door_is_not_the_owner() {
        let mut rig = Rig::armed();
        rig.send(1000, arrive("alice"));
        rig.send(1121, door(true));
        assert_eq!(rig.state(), "pending");
    }

    #[test]
    fn repeated_presence_reports_do_not_refresh_the_arrival() {
        let mut rig = Rig::armed();
        rig.send(1000, arrive("alice"));
        rig.send(1100, arrive("alice"));
        rig.send(1150, door(true));
        assert_eq!(rig.state(), "pending");
    }

    #[test]
    fn camera_offline_while_armed_is_possible_tampering() {
        let mut rig = Rig::armed();
        let offline = || Event::DeviceOffline {
            device: "door_cam".into(),
        };
        rig.send(100, offline());
        assert!(rig.alerts(AlertLevel::High).is_empty());
        assert_eq!(rig.engine.next_deadline(), Some(Ts::from_secs(160)));
        rig.send(159, offline());
        rig.send(160, Event::Tick);
        let high = rig.alerts(AlertLevel::High);
        assert_eq!(high.len(), 1);
        assert!(high[0].contains("tampering"));
        assert!(rig.engine.offline().contains("door_cam"));

        rig.send(
            200,
            Event::DeviceOnline {
                device: "door_cam".into(),
            },
        );
        assert!(rig
            .alerts(AlertLevel::Info)
            .iter()
            .any(|t| t.contains("back online")));
    }

    #[test]
    fn flapping_camera_status_is_not_reported() {
        let mut rig = Rig::armed();
        for t in (100..400).step_by(20) {
            rig.send(
                t,
                Event::DeviceOffline {
                    device: "door_cam".into(),
                },
            );
            rig.send(
                t + 10,
                Event::DeviceOnline {
                    device: "door_cam".into(),
                },
            );
        }
        rig.send(500, Event::Tick);
        assert!(rig.alerts(AlertLevel::High).is_empty());
        assert!(rig.engine.offline().is_empty());
    }

    #[test]
    fn offline_while_disarmed_or_non_critical_is_only_logged() {
        let mut rig = Rig::new(config());
        rig.send(
            0,
            Event::DeviceOffline {
                device: "door_cam".into(),
            },
        );
        rig.send(100, Event::Tick);
        assert!(rig.alerts(AlertLevel::High).is_empty());

        let mut rig = Rig::armed();
        rig.send(
            100,
            Event::DeviceOffline {
                device: "living_pir".into(),
            },
        );
        rig.send(200, Event::Tick);
        assert!(rig.alerts(AlertLevel::High).is_empty());
        assert!(rig.engine.offline().contains("living_pir"));
    }

    #[test]
    fn tampering_alert_is_deduplicated() {
        let mut rig = Rig::armed();
        let off = |rig: &mut Rig, t| {
            rig.send(
                t,
                Event::DeviceOffline {
                    device: "front_door".into(),
                },
            );
        };
        let on = |rig: &mut Rig, t| {
            rig.send(
                t,
                Event::DeviceOnline {
                    device: "front_door".into(),
                },
            );
        };
        off(&mut rig, 100);
        rig.send(160, Event::Tick);
        on(&mut rig, 170);
        off(&mut rig, 180);
        rig.send(240, Event::Tick);
        // Second outage within the 5-minute cooldown: logged, not alerted.
        assert_eq!(rig.alerts(AlertLevel::High).len(), 1);
        on(&mut rig, 250);
        off(&mut rig, 400);
        rig.send(460, Event::Tick);
        assert_eq!(rig.alerts(AlertLevel::High).len(), 2);
    }

    #[test]
    fn smoke_alerts_once_per_cooldown_but_always_sounds() {
        let mut rig = Rig::new(config());
        let smoke = || Event::SmokeDetected {
            sensor: "kitchen_smoke".into(),
        };
        for t in (0..299).step_by(10) {
            let actions = rig.send(t, smoke());
            assert!(actions.contains(&Action::SirenOn));
        }
        assert_eq!(rig.alerts(AlertLevel::Critical).len(), 1);
        rig.send(300, smoke());
        assert_eq!(rig.alerts(AlertLevel::Critical).len(), 2);
        assert_eq!(rig.state(), "disarmed");
    }

    #[test]
    fn disarming_clears_pending_windows() {
        let mut rig = Rig::armed();
        rig.send(100, wisense());
        rig.send(101, command(Command::Disarm));
        assert!(rig.engine.windows().is_empty());
        rig.send(130, Event::Tick);
        assert!(rig.alerts(AlertLevel::Low).is_empty());
    }

    #[test]
    fn motion_during_entry_delay_does_not_skip_it() {
        let mut rig = Rig::armed();
        rig.send(100, door(true));
        rig.send(101, motion(MotionSource::Pir, "hall_pir"));
        rig.send(102, motion(MotionSource::Wisense, "living_wisense"));
        rig.send(103, motion(MotionSource::Pir, "living_pir"));
        assert_eq!(rig.state(), "pending");
        rig.send(110, command(Command::Disarm));
        assert_eq!(rig.state(), "disarmed");
        assert!(rig.alerts(AlertLevel::Critical).is_empty());
    }

    #[test]
    fn nan_scores_are_ignored() {
        let mut rig = Rig::armed();
        rig.send(100, wisense());
        rig.send(101, person("living_cam", f32::NAN, "evt-8"));
        assert_eq!(rig.state(), "armed");
    }
}
