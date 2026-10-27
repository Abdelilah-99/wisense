//! `homeguard simulate`: replays a scripted timeline of events through the
//! real core, with no MQTT and no hardware. Alerts are printed, not sent.
//!
//! A scenario is a TOML file:
//!
//! ```toml
//! name = "Burglar through the front door"
//! config = "config/house.toml"   # relative to the scenario file
//! until = "7m"                    # keep time running until then
//!
//! [[step]]
//! at = "0s"
//! event = { type = "command", command = { arm = "away" }, by = "owner" }
//!
//! [[step]]
//! at = "5m"
//! event = { type = "contact_changed", sensor = "front_door", open = true }
//!
//! [expect]
//! final_state = "triggered"
//! actions = ["state:arming", "alert:high@5m", "alert:critical@5m30s", "siren_on"]
//! never = ["siren_off"]
//! counts = { "alert:critical" = 1 }
//! ```
//!
//! Between steps, time advances to each deadline the engine reports (an
//! entry delay ending, a confirmation window closing), so timers fire at
//! their exact time, as they would on the hub.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context};
use serde::Deserialize;

use crate::core::time::format_duration;
use crate::core::{Action, CoreConfig, Engine, Event, Input, Ts};

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Scenario {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    /// The homeguard configuration to use, relative to the scenario file.
    pub config: PathBuf,
    /// Keep time running until then; defaults to the last step.
    #[serde(default)]
    pub until: Option<Ts>,
    #[serde(default, rename = "step")]
    pub steps: Vec<Step>,
    #[serde(default)]
    pub expect: Option<Expect>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Step {
    pub at: Ts,
    pub event: Event,
    /// Repeat the event this often (e.g. `"5s"`)...
    #[serde(default)]
    pub repeat_every: Option<String>,
    /// ...until this time (inclusive).
    #[serde(default)]
    pub repeat_until: Option<Ts>,
}

impl Step {
    /// The step and its repetitions, as single events.
    fn expand(&self) -> anyhow::Result<Vec<(Ts, Event)>> {
        let (Some(every), Some(until)) = (&self.repeat_every, self.repeat_until) else {
            if self.repeat_every.is_some() || self.repeat_until.is_some() {
                bail!(
                    "step at {}: repeat_every and repeat_until go together",
                    show(self.at)
                );
            }
            return Ok(vec![(self.at, self.event.clone())]);
        };
        let every = crate::core::time::parse_duration(every)
            .map_err(|e| anyhow::anyhow!("step at {}: {e}", show(self.at)))?;
        if every.is_zero() {
            bail!(
                "step at {}: repeat_every must be more than 0",
                show(self.at)
            );
        }
        let mut out = Vec::new();
        let mut at = self.at;
        while at <= until {
            out.push((at, self.event.clone()));
            if out.len() > 100_000 {
                bail!("step at {}: too many repetitions", show(self.at));
            }
            at = at + every;
        }
        Ok(out)
    }
}

/// What the run must produce. Actions are named by [`tag`]; `name@time`
/// also requires the exact time.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Expect {
    /// The state name at the end, e.g. `"armed"`.
    #[serde(default)]
    pub final_state: Option<String>,
    /// Must all occur, in this order (other actions may come in between).
    #[serde(default)]
    pub actions: Vec<String>,
    /// Must not occur at all.
    #[serde(default)]
    pub never: Vec<String>,
    /// Exact number of occurrences.
    #[serde(default)]
    pub counts: BTreeMap<String, usize>,
}

/// One thing that happened during the run.
#[derive(Debug, Clone, PartialEq)]
pub enum Record {
    /// A scripted event went in.
    Input { at: Ts, event: Event },
    /// The core decided something.
    Action { at: Ts, action: Action },
}

impl Record {
    pub fn at(&self) -> Ts {
        match self {
            Record::Input { at, .. } | Record::Action { at, .. } => *at,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Outcome {
    pub records: Vec<Record>,
    /// The engine's state name at the end.
    pub final_state: String,
}

impl Outcome {
    pub fn actions(&self) -> impl Iterator<Item = (Ts, &Action)> {
        self.records.iter().filter_map(|r| match r {
            Record::Action { at, action } => Some((*at, action)),
            Record::Input { .. } => None,
        })
    }
}

/// Reads a scenario and the configuration it names.
pub fn load(path: &Path) -> anyhow::Result<(Scenario, CoreConfig)> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let scenario: Scenario =
        toml::from_str(&text).with_context(|| format!("invalid scenario {}", path.display()))?;
    let config_path = path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(&scenario.config);
    let config_text = std::fs::read_to_string(&config_path)
        .with_context(|| format!("reading {}", config_path.display()))?;
    let config: CoreConfig = toml::from_str(&config_text)
        .with_context(|| format!("invalid configuration {}", config_path.display()))?;
    Ok((scenario, config))
}

/// Runs the scenario through a fresh engine. Pure.
pub fn simulate(config: &CoreConfig, scenario: &Scenario) -> anyhow::Result<Outcome> {
    let mut engine = Engine::new(config)?;
    let mut steps = Vec::new();
    for step in &scenario.steps {
        steps.extend(step.expand()?);
    }
    steps.sort_by_key(|(at, _)| *at); // stable: same-time steps keep their order
    let end = scenario
        .until
        .into_iter()
        .chain(steps.last().map(|(at, _)| *at))
        .max()
        .unwrap_or(Ts::ZERO);

    let mut records = Vec::new();
    let mut now = Ts::ZERO;
    for (at, event) in steps {
        run_deadlines(&mut engine, &mut now, at, &mut records)?;
        now = at;
        records.push(Record::Input {
            at: now,
            event: event.clone(),
        });
        let actions = engine.handle(&Input::new(now, event));
        records.extend(
            actions
                .into_iter()
                .map(|action| Record::Action { at: now, action }),
        );
    }
    run_deadlines(&mut engine, &mut now, end, &mut records)?;
    Ok(Outcome {
        records,
        final_state: engine.state().name().to_string(),
    })
}

/// Feeds a `Tick` at every deadline up to and including `until`.
fn run_deadlines(
    engine: &mut Engine,
    now: &mut Ts,
    until: Ts,
    records: &mut Vec<Record>,
) -> anyhow::Result<()> {
    // Each tick resolves at least one deadline; this bounds a buggy loop.
    for _ in 0..100_000 {
        match engine.next_deadline() {
            Some(deadline) if deadline <= until => {
                let at = deadline.max(*now);
                *now = at;
                let actions = engine.handle(&Input::new(at, Event::Tick));
                records.extend(
                    actions
                        .into_iter()
                        .map(|action| Record::Action { at, action }),
                );
                if engine.next_deadline().is_some_and(|d| d <= at) {
                    bail!("the engine's deadline did not move past {at}");
                }
            }
            _ => return Ok(()),
        }
    }
    bail!("too many deadlines before {until}")
}

/// The name expectations use for an action: `state:<name>`,
/// `alert:<level>`, `siren_on`, `siren_off`, `lights_on` or `log`.
pub fn tag(action: &Action) -> String {
    match action {
        Action::StateChanged { state } => format!("state:{}", state.name()),
        Action::SendAlert { level, .. } => format!("alert:{level}"),
        Action::SirenOn => "siren_on".into(),
        Action::SirenOff => "siren_off".into(),
        Action::LightsOn => "lights_on".into(),
        Action::LogEvent { .. } => "log".into(),
    }
}

/// What does not match `expect`; empty when everything does.
pub fn check(outcome: &Outcome, expect: &Expect) -> Vec<String> {
    let mut failures = Vec::new();
    let tagged: Vec<(Ts, String)> = outcome.actions().map(|(at, a)| (at, tag(a))).collect();
    let matches = |pattern: &str, at: Ts, tag: &str| -> Result<bool, String> {
        match pattern.split_once('@') {
            None => Ok(pattern == tag),
            Some((name, time)) => {
                let want = crate::core::time::parse_duration(time)
                    .map_err(|e| format!("{pattern:?}: {e}"))?;
                Ok(name == tag && at == Ts::ZERO + want)
            }
        }
    };

    if let Some(want) = &expect.final_state {
        if *want != outcome.final_state {
            failures.push(format!(
                "final state is {}, expected {want}",
                outcome.final_state
            ));
        }
    }

    let mut from = 0;
    for pattern in &expect.actions {
        let found = tagged[from..]
            .iter()
            .position(|(at, tag)| matches(pattern, *at, tag).unwrap_or(false));
        match found {
            Some(i) => from += i + 1,
            None => {
                if let Err(e) = matches(pattern, Ts::ZERO, "") {
                    failures.push(e);
                } else {
                    failures.push(format!("expected {pattern} (in order), not found"));
                }
                break;
            }
        }
    }

    for pattern in &expect.never {
        if let Some((at, tag)) = tagged
            .iter()
            .find(|(at, tag)| matches(pattern, *at, tag).unwrap_or(false))
        {
            failures.push(format!("{tag} at {} should never happen", show(*at)));
        }
    }

    for (pattern, want) in &expect.counts {
        let got = tagged
            .iter()
            .filter(|(at, tag)| matches(pattern, *at, tag).unwrap_or(false))
            .count();
        if got != *want {
            failures.push(format!("{pattern} happened {got} times, expected {want}"));
        }
    }
    failures
}

/// `t+1m30s` → `1m30s`, for printing.
pub fn show(at: Ts) -> String {
    format_duration(at.since(Ts::ZERO))
}

/// One printable line per record.
pub fn format_record(record: &Record) -> String {
    match record {
        Record::Input { at, event } => format!("{:>8}  > {}", show(*at), describe_input(event)),
        Record::Action { at, action } => {
            let marker = match action {
                Action::StateChanged { .. } => "STATE",
                Action::SendAlert { .. } => "ALERT",
                Action::SirenOn | Action::SirenOff | Action::LightsOn => "DO",
                Action::LogEvent { .. } => "log",
            };
            let text = match action {
                Action::SendAlert {
                    level,
                    text,
                    snapshot_ref,
                } => {
                    let snapshot = snapshot_ref
                        .as_deref()
                        .map(|s| format!(" [snapshot {s}]"))
                        .unwrap_or_default();
                    format!("({level}) {text}{snapshot}  (printed, not sent)")
                }
                other => other.to_string(),
            };
            format!("{:>8}    {marker:<5} {text}", show(*at))
        }
    }
}

fn describe_input(event: &Event) -> String {
    match event {
        Event::Command { command, by } => format!("{command:?} by {by}").to_lowercase(),
        other => crate::core::machine::describe(other),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::{AlertLevel, Command, Mode};

    fn config() -> CoreConfig {
        toml::from_str(
            r#"
            zones = ["hall"]
            [[sensors]]
            id = "door"
            kind = "contact"
            zone = "hall"
            "#,
        )
        .unwrap()
    }

    fn scenario(steps: Vec<Step>, until: Option<Ts>) -> Scenario {
        Scenario {
            name: "test".into(),
            description: None,
            config: "unused".into(),
            until,
            steps,
            expect: None,
        }
    }

    fn step(at: Ts, event: Event) -> Step {
        Step {
            at,
            event,
            repeat_every: None,
            repeat_until: None,
        }
    }

    #[test]
    fn repeated_steps_expand() {
        let mut s = step(Ts::from_secs(10), Event::Tick);
        s.repeat_every = Some("5s".into());
        s.repeat_until = Some(Ts::from_secs(30));
        let times: Vec<u64> = s
            .expand()
            .unwrap()
            .iter()
            .map(|(t, _)| t.as_millis() / 1000)
            .collect();
        assert_eq!(times, [10, 15, 20, 25, 30]);
        s.repeat_every = Some("0s".into());
        assert!(s.expand().is_err());
        s.repeat_until = None;
        assert!(s.expand().is_err());
    }

    fn arm_then_open() -> Vec<Step> {
        vec![
            step(
                Ts::ZERO,
                Event::Command {
                    command: Command::Arm(Mode::Away),
                    by: "t".into(),
                },
            ),
            step(
                Ts::from_secs(120),
                Event::ContactChanged {
                    sensor: "door".into(),
                    open: true,
                },
            ),
        ]
    }

    #[test]
    fn timers_fire_at_their_exact_deadline() {
        let outcome = simulate(
            &config(),
            &scenario(arm_then_open(), Some(Ts::from_secs(200))),
        )
        .unwrap();
        let times: Vec<(String, Ts)> = outcome
            .actions()
            .filter(|(_, a)| matches!(a, Action::StateChanged { .. }))
            .map(|(at, a)| (tag(a), at))
            .collect();
        assert_eq!(
            times,
            vec![
                ("state:arming".into(), Ts::ZERO),
                ("state:armed".into(), Ts::from_secs(60)),
                ("state:pending".into(), Ts::from_secs(120)),
                ("state:triggered".into(), Ts::from_secs(150)),
            ]
        );
        assert_eq!(outcome.final_state, "triggered");
    }

    #[test]
    fn until_defaults_to_the_last_step() {
        let outcome = simulate(&config(), &scenario(arm_then_open(), None)).unwrap();
        assert_eq!(outcome.final_state, "pending");
    }

    #[test]
    fn expectations() {
        let outcome = simulate(
            &config(),
            &scenario(arm_then_open(), Some(Ts::from_secs(200))),
        )
        .unwrap();
        let ok = Expect {
            final_state: Some("triggered".into()),
            actions: vec![
                "state:arming".into(),
                "alert:high@2m".into(),
                "alert:critical@2m30s".into(),
                "siren_on".into(),
            ],
            never: vec!["siren_off".into()],
            counts: BTreeMap::from([("alert:critical".into(), 1)]),
        };
        assert_eq!(check(&outcome, &ok), Vec::<String>::new());

        let bad = Expect {
            final_state: Some("armed".into()),
            actions: vec!["siren_on".into(), "state:arming".into()],
            never: vec!["siren_on".into()],
            counts: BTreeMap::from([("alert:critical".into(), 2)]),
        };
        assert_eq!(
            check(&outcome, &bad).len(),
            4,
            "{:?}",
            check(&outcome, &bad)
        );

        let wrong_time = Expect {
            actions: vec!["alert:critical@2m29s".into()],
            ..Expect::default()
        };
        assert_eq!(check(&outcome, &wrong_time).len(), 1);
        let bad_time = Expect {
            actions: vec!["alert:critical@soon".into()],
            ..Expect::default()
        };
        assert!(check(&outcome, &bad_time)[0].contains("soon"));
    }

    #[test]
    fn records_print_readably() {
        let line = format_record(&Record::Action {
            at: Ts::from_secs(90),
            action: Action::alert(AlertLevel::Critical, "ALARM"),
        });
        assert!(line.contains("1m30s"), "{line}");
        assert!(line.contains("printed, not sent"), "{line}");
        let input = format_record(&Record::Input {
            at: Ts::ZERO,
            event: Event::ContactChanged {
                sensor: "door".into(),
                open: true,
            },
        });
        assert!(input.ends_with("> door opened"), "{input}");
    }

    #[test]
    fn scenario_toml_parses() {
        let scenario: Scenario = toml::from_str(
            r#"
            name = "x"
            config = "house.toml"
            until = "1m"
            [[step]]
            at = "0s"
            event = { type = "command", command = { arm = "night" }, by = "me" }
            [[step]]
            at = "10s"
            event = { type = "command", command = "disarm", by = "me" }
            [[step]]
            at = "1m30s"
            event = { type = "person_detected", camera = "cam", score = 0.9, snapshot_ref = "e1" }
            [expect]
            never = ["siren_on"]
            "#,
        )
        .unwrap();
        assert_eq!(scenario.steps.len(), 3);
        assert_eq!(scenario.steps[2].at, Ts::from_secs(90));
        assert!(matches!(
            scenario.steps[1].event,
            Event::Command {
                command: Command::Disarm,
                ..
            }
        ));
    }
}
