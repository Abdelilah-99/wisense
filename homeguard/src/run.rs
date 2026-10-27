//! `homeguard run`: the imperative shell around the core.
//!
//! One task owns the [`Engine`]. Everything else (MQTT, the ticker) sends it
//! events over a channel; it stamps each with the current time, runs the
//! core, and dispatches the resulting actions. Dispatch never waits on the
//! network, so a slow broker cannot delay a decision.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use anyhow::Context;
use rumqttc::AsyncClient;
use tokio::sync::mpsc;
use tracing::{error, info, warn};

use crate::config::AppConfig;
use crate::core::{Action, AlertLevel, Engine, Event, Input, Mode, State, Ts};
use crate::mqtt::{self, Connection};
use crate::publish::{state_message, Outgoing, Publisher, AVAILABILITY_TOPIC};

/// How often time is fed to the core when nothing else happens.
const TICK: Duration = Duration::from_millis(500);
/// How often the heartbeat file is refreshed.
const HEARTBEAT_EVERY: Duration = Duration::from_secs(10);
pub const HEARTBEAT_FILE: &str = "heartbeat";
const ARMED_FILE: &str = "armed";

/// The runtime clock: milliseconds since the process started.
#[derive(Debug, Clone, Copy)]
pub struct Clock {
    start: Instant,
}

impl Clock {
    pub fn start() -> Self {
        Clock {
            start: Instant::now(),
        }
    }

    pub fn now(&self) -> Ts {
        Ts::from_millis(u64::try_from(self.start.elapsed().as_millis()).unwrap_or(u64::MAX))
    }
}

pub async fn run(config: AppConfig, data_dir: PathBuf) -> anyhow::Result<()> {
    let (username, password) = config
        .mqtt
        .credentials(|name| std::env::var(name).ok())
        .context("MQTT credentials")?;
    let mut engine = config.engine()?;
    let phones = config.presence.phones(|name| std::env::var(name).ok())?;
    let sweep = config.presence.sweep()?;
    std::fs::create_dir_all(&data_dir)
        .with_context(|| format!("creating data directory {}", data_dir.display()))?;

    let (client, eventloop) = Connection::from_config(&config.mqtt, username, password).open();
    let (events_tx, events_rx) = mpsc::channel::<Event>(1024);
    let clock = Clock::start();
    if !phones.is_empty() {
        crate::presence::warn_about_randomized_macs(&phones);
        tokio::spawn(crate::presence::run(
            phones,
            sweep,
            config.presence.probe_interval,
            config.core.presence.grace,
            clock,
            events_tx.clone(),
        ));
    }
    tokio::spawn(mqtt::run_incoming(
        eventloop,
        client.clone(),
        config.mapping(),
        events_tx,
    ));
    info!(
        broker = %format!("{}:{}", config.mqtt.host, config.mqtt.port),
        sensors = engine.catalog().sensors().count(),
        cameras = engine.catalog().cameras().count(),
        "homeguard started"
    );

    let mut dispatcher = Dispatcher {
        client: client.clone(),
        publisher: Publisher {
            zigbee_base: config.zigbee.base_topic.clone(),
            actuators: config.actuators.clone(),
            siren_duration: config.core.alarm.auto_reset,
        },
        data_dir: data_dir.clone(),
    };
    match read_armed(&data_dir) {
        Ok(Some(mode)) => {
            let actions = engine.restore(mode);
            dispatcher.dispatch(&actions);
        }
        Ok(None) => dispatcher.send(state_message(engine.state())),
        Err(e) => {
            warn!(error = %format!("{e:#}"), "could not read the saved arm state; starting disarmed")
        }
    }

    core_loop(engine, events_rx, clock, &mut dispatcher).await;

    mqtt::publish(
        &client,
        Outgoing {
            topic: AVAILABILITY_TOPIC.into(),
            payload: b"offline".to_vec(),
            retain: true,
        },
    );
    // Give the event loop a moment to flush before exiting.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let _ = client.try_disconnect();
    info!("homeguard stopped");
    Ok(())
}

async fn core_loop(
    mut engine: Engine,
    mut events: mpsc::Receiver<Event>,
    clock: Clock,
    dispatcher: &mut Dispatcher,
) {
    let mut ticker = tokio::time::interval(TICK);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut last_heartbeat: Option<Instant> = None;
    let shutdown = shutdown_signal();
    tokio::pin!(shutdown);
    loop {
        let event = tokio::select! {
            event = events.recv() => match event {
                Some(event) => event,
                None => break,
            },
            _ = ticker.tick() => Event::Tick,
            _ = &mut shutdown => break,
        };
        let actions = engine.handle(&Input::new(clock.now(), event));
        dispatcher.dispatch(&actions);

        if last_heartbeat.is_none_or(|t| t.elapsed() >= HEARTBEAT_EVERY) {
            last_heartbeat = Some(Instant::now());
            dispatcher.heartbeat();
        }
    }
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        match signal(SignalKind::terminate()) {
            Ok(mut term) => {
                tokio::select! {
                    _ = term.recv() => {}
                    _ = tokio::signal::ctrl_c() => {}
                }
            }
            Err(_) => {
                let _ = tokio::signal::ctrl_c().await;
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
    info!("shutting down");
}

struct Dispatcher {
    client: AsyncClient,
    publisher: Publisher,
    data_dir: PathBuf,
}

impl Dispatcher {
    fn dispatch(&mut self, actions: &[Action]) {
        for action in actions {
            log_action(action);
            if let Action::StateChanged { state } = action {
                if let Err(e) = write_armed(&self.data_dir, state) {
                    error!(error = %format!("{e:#}"), "could not save the arm state");
                }
            }
            for message in self.publisher.outgoing(action) {
                self.send(message);
            }
        }
    }

    fn send(&self, message: Outgoing) {
        mqtt::publish(&self.client, message);
    }

    fn heartbeat(&self) {
        let path = self.data_dir.join(HEARTBEAT_FILE);
        if let Err(e) = std::fs::write(&path, b"ok\n") {
            warn!(path = %path.display(), error = %e, "could not write the heartbeat file");
        }
    }
}

fn log_action(action: &Action) {
    match action {
        Action::SendAlert { level, .. } if *level >= AlertLevel::High => warn!("{action}"),
        Action::SirenOn => warn!("{action}"),
        _ => info!("{action}"),
    }
}

/// The mode to restore after a restart: whatever the system is armed (or
/// arming) in, nothing when disarmed.
pub fn mode_to_save(state: &State) -> Option<Mode> {
    state.mode()
}

fn write_armed(data_dir: &Path, state: &State) -> anyhow::Result<()> {
    let path = data_dir.join(ARMED_FILE);
    match mode_to_save(state) {
        Some(mode) => {
            let tmp = data_dir.join(format!("{ARMED_FILE}.tmp"));
            std::fs::write(&tmp, mode.as_str())
                .with_context(|| format!("writing {}", tmp.display()))?;
            std::fs::rename(&tmp, &path).with_context(|| format!("writing {}", path.display()))
        }
        None => match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e).with_context(|| format!("removing {}", path.display())),
        },
    }
}

fn read_armed(data_dir: &Path) -> anyhow::Result<Option<Mode>> {
    let path = data_dir.join(ARMED_FILE);
    match std::fs::read_to_string(&path) {
        Ok(text) => Mode::parse(&text)
            .map(Some)
            .with_context(|| format!("{} holds {text:?}, not an arm mode", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

/// Whether a heartbeat written at `modified` is recent enough at `now`.
pub fn heartbeat_fresh(modified: SystemTime, now: SystemTime, max_age: Duration) -> bool {
    match now.duration_since(modified) {
        Ok(age) => age <= max_age,
        // Written "in the future" (clock adjusted): it was just written.
        Err(_) => true,
    }
}

/// `homeguard healthcheck`: succeeds if the core loop wrote its heartbeat
/// recently.
pub fn healthcheck(data_dir: &Path, max_age: Duration) -> anyhow::Result<()> {
    let path = data_dir.join(HEARTBEAT_FILE);
    let modified = std::fs::metadata(&path)
        .and_then(|m| m.modified())
        .with_context(|| format!("no heartbeat at {}", path.display()))?;
    if heartbeat_fresh(modified, SystemTime::now(), max_age) {
        Ok(())
    } else {
        anyhow::bail!("heartbeat at {} is older than {max_age:?}", path.display())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::Cause;

    #[test]
    fn heartbeat_age() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1000);
        let max = Duration::from_secs(30);
        assert!(heartbeat_fresh(now - Duration::from_secs(30), now, max));
        assert!(!heartbeat_fresh(now - Duration::from_secs(31), now, max));
        assert!(heartbeat_fresh(now + Duration::from_secs(5), now, max));
    }

    #[test]
    fn armed_mode_survives_a_restart() {
        let dir = std::env::temp_dir().join(format!("homeguard-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(read_armed(&dir).unwrap(), None);
        let triggered = State::Triggered {
            mode: Mode::Night,
            cause: Cause::new("d", "d"),
            since: Ts::ZERO,
        };
        write_armed(&dir, &triggered).unwrap();
        assert_eq!(read_armed(&dir).unwrap(), Some(Mode::Night));
        write_armed(&dir, &State::Disarmed).unwrap();
        assert_eq!(read_armed(&dir).unwrap(), None);
        write_armed(&dir, &State::Disarmed).unwrap();
        std::fs::write(dir.join(ARMED_FILE), "sideways").unwrap();
        assert!(read_armed(&dir).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn heartbeat_check_reads_the_file() {
        let dir = std::env::temp_dir().join(format!("homeguard-hb-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert!(healthcheck(&dir, Duration::from_secs(30)).is_err());
        std::fs::write(dir.join(HEARTBEAT_FILE), "ok").unwrap();
        assert!(healthcheck(&dir, Duration::from_secs(30)).is_ok());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
