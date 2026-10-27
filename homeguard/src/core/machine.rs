//! The arm/disarm state machine.
//!
//! ```text
//!            arm                 exit delay over
//! Disarmed ───────► Arming ─────────────────────► Armed ◄────────────┐
//!    ▲                                              │ │               │
//!    │ disarm (from any state)   intrusion, delayed │ │ immediate     │ auto-reset
//!    │                                              ▼ │               │
//!    └──────────────────────────────────────── Pending ──► Triggered ─┘
//!                                          entry delay over
//! ```
//!
//! [`AlarmMachine::handle`] applies the plain rules: a sensor that is active
//! in the current mode starts the entry delay. Higher-level policy (fusion)
//! drives the lower-level methods ([`AlarmMachine::intrusion`], ...) instead.

use super::config::{AlarmConfig, Catalog};
use super::event::{Action, AlertLevel, Command, Event, Input, Mode};
use super::state::{Cause, State};
use super::time::{format_duration, Ts};

/// How an intrusion should be handled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Delay {
    /// Start the entry delay (doors people normally come in through).
    Entry,
    /// Trigger now (confirmed intrusion).
    Immediate,
}

#[derive(Debug, Clone)]
pub struct AlarmMachine {
    config: AlarmConfig,
    catalog: Catalog,
    state: State,
}

impl AlarmMachine {
    pub fn new(config: AlarmConfig, catalog: Catalog) -> Self {
        AlarmMachine {
            config,
            catalog,
            state: State::Disarmed,
        }
    }

    pub fn state(&self) -> &State {
        &self.state
    }

    pub fn config(&self) -> &AlarmConfig {
        &self.config
    }

    pub fn catalog(&self) -> &Catalog {
        &self.catalog
    }

    /// The next time something happens on its own (an exit or entry delay
    /// ending, an auto-reset). The shell or simulator sends a `Tick` then.
    pub fn next_deadline(&self) -> Option<Ts> {
        match &self.state {
            State::Disarmed | State::Armed { .. } => None,
            State::Arming { until, .. } | State::Pending { until, .. } => Some(*until),
            State::Triggered { since, .. } => Some(*since + self.config.auto_reset),
        }
    }

    /// Whether `id` can raise an alarm right now.
    pub fn is_watching(&self, id: &str) -> bool {
        self.state.is_guarding()
            && self
                .state
                .mode()
                .is_some_and(|mode| self.catalog.is_active(id, mode))
    }

    /// Plain rules, without fusion: an active sensor starts the entry delay.
    pub fn handle(&mut self, input: &Input) -> Vec<Action> {
        let at = input.at;
        match &input.event {
            Event::Command { command, by } => self.command(at, *command, by),
            Event::Tick => self.tick(at),
            Event::SmokeDetected { sensor } => self.safety(Hazard::Smoke, sensor),
            Event::LeakDetected { sensor } => self.safety(Hazard::Leak, sensor),
            Event::ContactChanged { sensor, open: true } if self.is_watching(sensor) => {
                let name = self.catalog.display_name(sensor);
                let cause = Cause::new(sensor, format!("{name} opened"));
                self.intrusion(at, cause, Delay::Entry)
            }
            Event::MotionDetected { source, sensor } if self.is_watching(sensor) => {
                let name = self.catalog.display_name(sensor);
                let cause = Cause::new(sensor, format!("motion on {name} ({source})"));
                self.intrusion(at, cause, Delay::Entry)
            }
            other => vec![Action::log(describe(other))],
        }
    }

    pub fn command(&mut self, at: Ts, command: Command, by: &str) -> Vec<Action> {
        match command {
            Command::Arm(mode) => self.arm(at, mode, by),
            Command::Disarm => self.disarm(by),
        }
    }

    fn arm(&mut self, at: Ts, mode: Mode, by: &str) -> Vec<Action> {
        match &self.state {
            State::Pending { .. } | State::Triggered { .. } => {
                return vec![Action::log(format!(
                    "arm {mode} by {by} ignored: {}; disarm first",
                    self.state.name()
                ))];
            }
            State::Armed { mode: current } | State::Arming { mode: current, .. }
                if *current == mode =>
            {
                return vec![Action::log(format!(
                    "arm {mode} by {by} ignored: already {}",
                    self.state
                ))];
            }
            _ => {}
        }
        let delay = self.config.exit_delay;
        if delay.is_zero() {
            return self.set(
                State::Armed { mode },
                Some(Action::alert(
                    AlertLevel::Info,
                    format!("Armed ({mode}) by {by}."),
                )),
            );
        }
        self.set(
            State::Arming {
                mode,
                until: at + delay,
            },
            Some(Action::alert(
                AlertLevel::Info,
                format!(
                    "Arming ({mode}) by {by}: armed in {}.",
                    format_duration(delay)
                ),
            )),
        )
    }

    fn disarm(&mut self, by: &str) -> Vec<Action> {
        if self.state == State::Disarmed {
            return vec![Action::log(format!("disarm by {by}: already disarmed"))];
        }
        let was_triggered = matches!(self.state, State::Triggered { .. });
        let mut actions = self.set(
            State::Disarmed,
            Some(Action::alert(
                AlertLevel::Info,
                format!("Disarmed by {by}."),
            )),
        );
        if was_triggered {
            actions.push(Action::SirenOff);
        }
        actions
    }

    /// After a restart: back to armed in `mode` without an exit delay, as it
    /// was before. (An entry delay or alarm in progress is not restored; if
    /// the intrusion continues, the sensors trip again.)
    pub fn restore(&mut self, mode: Mode) -> Vec<Action> {
        self.set(
            State::Armed { mode },
            Some(Action::alert(
                AlertLevel::High,
                format!("homeguard restarted; armed again ({mode}) as before."),
            )),
        )
    }

    /// Lets time-based transitions happen.
    pub fn tick(&mut self, at: Ts) -> Vec<Action> {
        match self.state.clone() {
            State::Arming { mode, until } if at >= until => self.set(
                State::Armed { mode },
                Some(Action::alert(AlertLevel::Info, format!("Armed ({mode})."))),
            ),
            State::Pending { mode, until, cause } if at >= until => {
                self.trigger(until, mode, cause)
            }
            State::Triggered { mode, since, .. } if at >= since + self.config.auto_reset => {
                let after = format_duration(self.config.auto_reset);
                let mut actions = vec![Action::SirenOff];
                actions.extend(self.set(
                    State::Armed { mode },
                    Some(Action::alert(
                        AlertLevel::High,
                        format!("Alarm reset after {after}; armed again ({mode})."),
                    )),
                ));
                actions
            }
            _ => Vec::new(),
        }
    }

    /// An intrusion while guarding. Ignored when disarmed or arming (people
    /// leaving during the exit delay).
    pub fn intrusion(&mut self, at: Ts, cause: Cause, delay: Delay) -> Vec<Action> {
        let delay = if self.config.entry_delay.is_zero() {
            Delay::Immediate
        } else {
            delay
        };
        match (self.state.clone(), delay) {
            (State::Armed { mode }, Delay::Entry) => {
                let text = format!(
                    "Entry: {}. Disarm within {} or the alarm goes off.",
                    cause.description,
                    format_duration(self.config.entry_delay)
                );
                let snapshot_ref = cause.snapshot_ref.clone();
                self.set(
                    State::Pending {
                        mode,
                        until: at + self.config.entry_delay,
                        cause,
                    },
                    Some(Action::SendAlert {
                        level: AlertLevel::High,
                        text,
                        snapshot_ref,
                    }),
                )
            }
            (State::Armed { mode } | State::Pending { mode, .. }, Delay::Immediate) => {
                self.trigger(at, mode, cause)
            }
            (State::Pending { .. } | State::Triggered { .. }, _) => vec![Action::log(format!(
                "{} while {}",
                cause.description,
                self.state.name()
            ))],
            (State::Disarmed | State::Arming { .. }, _) => vec![Action::log(format!(
                "{} ignored: {}",
                cause.description,
                self.state.name()
            ))],
        }
    }

    fn trigger(&mut self, at: Ts, mode: Mode, cause: Cause) -> Vec<Action> {
        let alert = Action::SendAlert {
            level: AlertLevel::Critical,
            text: format!("ALARM ({mode}): {}.", cause.description),
            snapshot_ref: cause.snapshot_ref.clone(),
        };
        let mut actions = self.set(
            State::Triggered {
                mode,
                cause,
                since: at,
            },
            Some(alert),
        );
        actions.push(Action::SirenOn);
        actions.push(Action::LightsOn);
        actions
    }

    /// Smoke and leaks alert whatever the arm state, without changing it.
    pub fn safety(&mut self, hazard: Hazard, sensor: &str) -> Vec<Action> {
        let name = self.catalog.display_name(sensor);
        match hazard {
            Hazard::Smoke => vec![
                Action::alert(AlertLevel::Critical, format!("SMOKE detected by {name}.")),
                Action::SirenOn,
                Action::LightsOn,
            ],
            Hazard::Leak => vec![Action::alert(
                AlertLevel::Critical,
                format!("WATER LEAK detected by {name}."),
            )],
        }
    }

    fn set(&mut self, state: State, alert: Option<Action>) -> Vec<Action> {
        self.state = state;
        let mut actions = vec![Action::StateChanged {
            state: self.state.clone(),
        }];
        actions.extend(alert);
        actions
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hazard {
    Smoke,
    Leak,
}

/// A log line for events the machine does not act on.
pub fn describe(event: &Event) -> String {
    match event {
        Event::ContactChanged { sensor, open } => {
            format!("{sensor} {}", if *open { "opened" } else { "closed" })
        }
        Event::MotionDetected { source, sensor } => format!("motion on {sensor} ({source})"),
        Event::PersonDetected { camera, score, .. } => {
            format!("person on {camera} (score {score:.2})")
        }
        Event::PresenceChanged { person, home } => {
            format!("{person} is {}", if *home { "home" } else { "away" })
        }
        Event::DeviceOffline { device } => format!("{device} offline"),
        Event::DeviceOnline { device } => format!("{device} online"),
        Event::SmokeDetected { sensor } => format!("smoke on {sensor}"),
        Event::LeakDetected { sensor } => format!("leak on {sensor}"),
        Event::Command { command, by } => format!("{command:?} by {by}"),
        Event::Tick => "tick".into(),
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::core::config::tests::{camera, sensor, zones};
    use crate::core::config::SensorKind;
    use crate::core::event::MotionSource;

    /// Exit 60 s, entry 30 s, auto-reset 10 min (the defaults).
    fn machine() -> AlarmMachine {
        let catalog = Catalog::new(
            &zones(&["house", "garden"]),
            vec![
                sensor("front_door", SensorKind::Contact, "house", None),
                sensor("bedroom_window", SensorKind::Contact, "house", None),
                sensor("living_pir", SensorKind::Pir, "house", None),
                sensor(
                    "downstairs_pir",
                    SensorKind::Pir,
                    "house",
                    Some(vec![Mode::Away, Mode::Night]),
                ),
                sensor("kitchen_smoke", SensorKind::Smoke, "house", None),
                sensor("sink_leak", SensorKind::Leak, "house", None),
            ],
            vec![camera("garden", "garden")],
        )
        .unwrap();
        AlarmMachine::new(AlarmConfig::default(), catalog)
    }

    fn at(machine: &mut AlarmMachine, secs: u64, event: Event) -> Vec<Action> {
        machine.handle(&Input::new(Ts::from_secs(secs), event))
    }

    fn arm(mode: Mode) -> Event {
        Event::Command {
            command: Command::Arm(mode),
            by: "test".into(),
        }
    }

    fn disarm() -> Event {
        Event::Command {
            command: Command::Disarm,
            by: "test".into(),
        }
    }

    fn open(sensor: &str) -> Event {
        Event::ContactChanged {
            sensor: sensor.into(),
            open: true,
        }
    }

    fn pir(sensor: &str) -> Event {
        Event::MotionDetected {
            source: MotionSource::Pir,
            sensor: sensor.into(),
        }
    }

    fn armed_away() -> AlarmMachine {
        let mut m = machine();
        at(&mut m, 0, arm(Mode::Away));
        at(&mut m, 60, Event::Tick);
        assert_eq!(m.state(), &State::Armed { mode: Mode::Away });
        m
    }

    fn has_siren_on(actions: &[Action]) -> bool {
        actions.contains(&Action::SirenOn)
    }

    #[test]
    fn exit_delay_lasts_exactly_the_configured_time() {
        let mut m = machine();
        at(&mut m, 0, arm(Mode::Away));
        assert_eq!(
            m.state(),
            &State::Arming {
                mode: Mode::Away,
                until: Ts::from_secs(60)
            }
        );
        assert_eq!(m.next_deadline(), Some(Ts::from_secs(60)));

        // One millisecond before the end: still arming.
        m.handle(&Input::new(Ts::from_millis(59_999), Event::Tick));
        assert_eq!(m.state().name(), "arming");
        let actions = at(&mut m, 60, Event::Tick);
        assert_eq!(m.state(), &State::Armed { mode: Mode::Away });
        assert!(matches!(
            actions[0],
            Action::StateChanged {
                state: State::Armed { .. }
            }
        ));
        assert_eq!(m.next_deadline(), None);
    }

    #[test]
    fn doors_opened_during_exit_delay_are_ignored() {
        let mut m = machine();
        at(&mut m, 0, arm(Mode::Away));
        let actions = at(&mut m, 30, open("front_door"));
        assert_eq!(m.state().name(), "arming");
        assert!(matches!(actions.as_slice(), [Action::LogEvent { .. }]));
    }

    #[test]
    fn zero_exit_delay_arms_immediately() {
        let mut m = machine();
        m.config.exit_delay = Duration::ZERO;
        at(&mut m, 0, arm(Mode::Home));
        assert_eq!(m.state(), &State::Armed { mode: Mode::Home });
    }

    #[test]
    fn entry_delay_then_disarm_in_time() {
        let mut m = armed_away();
        let actions = at(&mut m, 100, open("front_door"));
        assert!(matches!(
            m.state(),
            State::Pending { until, .. } if *until == Ts::from_secs(130)
        ));
        assert!(actions.iter().any(|a| matches!(
            a,
            Action::SendAlert { level: AlertLevel::High, text, .. } if text.contains("front_door opened")
        )));
        assert!(!has_siren_on(&actions));

        // 29.999 s later: still pending.
        m.handle(&Input::new(Ts::from_millis(129_999), Event::Tick));
        assert_eq!(m.state().name(), "pending");
        let actions = at(&mut m, 129, disarm());
        assert_eq!(m.state(), &State::Disarmed);
        assert!(!has_siren_on(&actions));
        // No siren to silence: it never started.
        assert!(!actions.contains(&Action::SirenOff));
        assert!(at(&mut m, 130, Event::Tick).is_empty());
    }

    #[test]
    fn entry_delay_expiring_triggers_at_its_exact_end() {
        let mut m = armed_away();
        at(&mut m, 100, open("front_door"));
        // A late tick still records the trigger time as the end of the delay.
        let actions = m.handle(&Input::new(Ts::from_millis(130_400), Event::Tick));
        assert!(matches!(
            m.state(),
            State::Triggered { since, cause, .. }
                if *since == Ts::from_secs(130) && cause.sensor == "front_door"
        ));
        assert!(has_siren_on(&actions));
        assert!(actions.contains(&Action::LightsOn));
        assert!(actions.iter().any(|a| matches!(
            a,
            Action::SendAlert {
                level: AlertLevel::Critical,
                ..
            }
        )));
    }

    #[test]
    fn second_sensor_during_entry_delay_does_not_restart_it() {
        let mut m = armed_away();
        at(&mut m, 100, open("front_door"));
        at(&mut m, 120, open("bedroom_window"));
        assert!(matches!(
            m.state(),
            State::Pending { until, cause, .. }
                if *until == Ts::from_secs(130) && cause.sensor == "front_door"
        ));
    }

    #[test]
    fn immediate_intrusion_skips_the_entry_delay() {
        let mut m = armed_away();
        let actions = m.intrusion(
            Ts::from_secs(70),
            Cause::new("garden", "person on garden"),
            Delay::Immediate,
        );
        assert_eq!(m.state().name(), "triggered");
        assert!(has_siren_on(&actions));

        // From pending too.
        let mut m = armed_away();
        at(&mut m, 100, open("front_door"));
        m.intrusion(Ts::from_secs(105), Cause::new("x", "x"), Delay::Immediate);
        assert!(
            matches!(m.state(), State::Triggered { since, .. } if *since == Ts::from_secs(105))
        );
    }

    #[test]
    fn mode_filtering() {
        // Home: doors and windows only.
        let mut m = machine();
        at(&mut m, 0, arm(Mode::Home));
        at(&mut m, 60, Event::Tick);
        at(&mut m, 70, pir("living_pir"));
        at(&mut m, 71, pir("downstairs_pir"));
        assert_eq!(m.state(), &State::Armed { mode: Mode::Home });
        at(&mut m, 72, open("bedroom_window"));
        assert_eq!(m.state().name(), "pending");

        // Night: doors plus downstairs motion, not the living-room PIR.
        let mut m = machine();
        at(&mut m, 0, arm(Mode::Night));
        at(&mut m, 60, Event::Tick);
        at(&mut m, 70, pir("living_pir"));
        assert_eq!(m.state(), &State::Armed { mode: Mode::Night });
        at(&mut m, 71, pir("downstairs_pir"));
        assert_eq!(m.state().name(), "pending");

        // Away: everything.
        let mut m = armed_away();
        at(&mut m, 70, pir("living_pir"));
        assert_eq!(m.state().name(), "pending");
    }

    #[test]
    fn unknown_sensors_never_trigger() {
        let mut m = armed_away();
        at(&mut m, 70, open("garage_door"));
        assert_eq!(m.state(), &State::Armed { mode: Mode::Away });
    }

    #[test]
    fn closing_a_door_does_nothing() {
        let mut m = armed_away();
        at(
            &mut m,
            70,
            Event::ContactChanged {
                sensor: "front_door".into(),
                open: false,
            },
        );
        assert_eq!(m.state(), &State::Armed { mode: Mode::Away });
    }

    #[test]
    fn auto_reset_rearms_after_trigger() {
        let mut m = armed_away();
        at(&mut m, 100, open("front_door"));
        at(&mut m, 130, Event::Tick);
        assert_eq!(m.state().name(), "triggered");
        assert_eq!(m.next_deadline(), Some(Ts::from_secs(130 + 600)));

        assert!(at(&mut m, 729, Event::Tick).is_empty());
        let actions = at(&mut m, 730, Event::Tick);
        assert_eq!(m.state(), &State::Armed { mode: Mode::Away });
        assert!(actions.contains(&Action::SirenOff));

        // Armed again: a new intrusion starts a new entry delay.
        at(&mut m, 800, open("front_door"));
        assert!(matches!(m.state(), State::Pending { until, .. } if *until == Ts::from_secs(830)));
    }

    #[test]
    fn disarm_while_triggered_silences_the_siren() {
        let mut m = armed_away();
        at(&mut m, 100, open("front_door"));
        at(&mut m, 130, Event::Tick);
        let actions = at(&mut m, 140, disarm());
        assert_eq!(m.state(), &State::Disarmed);
        assert!(actions.contains(&Action::SirenOff));
    }

    #[test]
    fn rearming_after_a_trigger_needs_a_disarm() {
        let mut m = armed_away();
        at(&mut m, 100, open("front_door"));
        at(&mut m, 130, Event::Tick);
        at(&mut m, 140, arm(Mode::Away));
        assert_eq!(m.state().name(), "triggered");

        at(&mut m, 150, disarm());
        at(&mut m, 160, arm(Mode::Night));
        assert_eq!(
            m.state(),
            &State::Arming {
                mode: Mode::Night,
                until: Ts::from_secs(220)
            }
        );
        at(&mut m, 220, Event::Tick);
        assert_eq!(m.state(), &State::Armed { mode: Mode::Night });
    }

    #[test]
    fn switching_mode_restarts_the_exit_delay() {
        let mut m = armed_away();
        let actions = at(&mut m, 100, arm(Mode::Away));
        assert!(matches!(actions.as_slice(), [Action::LogEvent { .. }]));
        at(&mut m, 100, arm(Mode::Home));
        assert_eq!(
            m.state(),
            &State::Arming {
                mode: Mode::Home,
                until: Ts::from_secs(160)
            }
        );
    }

    #[test]
    fn disarm_when_disarmed_is_harmless() {
        let mut m = machine();
        let actions = at(&mut m, 0, disarm());
        assert_eq!(m.state(), &State::Disarmed);
        assert!(matches!(actions.as_slice(), [Action::LogEvent { .. }]));
    }

    #[test]
    fn smoke_always_alerts_and_sounds_the_siren() {
        for state_setup in [Mode::Away, Mode::Home] {
            let mut disarmed = machine();
            let mut armed = machine();
            at(&mut armed, 0, arm(state_setup));
            for m in [&mut disarmed, &mut armed] {
                let before = m.state().clone();
                let actions = at(
                    m,
                    5,
                    Event::SmokeDetected {
                        sensor: "kitchen_smoke".into(),
                    },
                );
                assert!(actions.iter().any(|a| matches!(
                    a,
                    Action::SendAlert { level: AlertLevel::Critical, text, .. } if text.contains("SMOKE")
                )));
                assert!(has_siren_on(&actions));
                // The arm state is untouched.
                assert_eq!(m.state(), &before);
            }
        }
    }

    #[test]
    fn leaks_always_alert_without_the_siren() {
        let mut m = machine();
        let actions = at(
            &mut m,
            5,
            Event::LeakDetected {
                sensor: "sink_leak".into(),
            },
        );
        assert!(matches!(
            actions.as_slice(),
            [Action::SendAlert { level: AlertLevel::Critical, text, .. }] if text.contains("WATER LEAK")
        ));
        assert_eq!(m.state(), &State::Disarmed);
    }

    #[test]
    fn restore_rearms_without_exit_delay() {
        let mut m = machine();
        let actions = m.restore(Mode::Night);
        assert_eq!(m.state(), &State::Armed { mode: Mode::Night });
        assert!(actions.iter().any(|a| matches!(
            a,
            Action::SendAlert {
                level: AlertLevel::High,
                ..
            }
        )));
    }

    #[test]
    fn zero_entry_delay_triggers_immediately() {
        let mut m = armed_away();
        m.config.entry_delay = Duration::ZERO;
        at(&mut m, 70, open("front_door"));
        assert_eq!(m.state().name(), "triggered");
    }
}
