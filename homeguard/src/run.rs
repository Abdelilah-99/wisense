//! `homeguard run`: the imperative shell around the core.
//!
//! One task owns the [`Engine`]. Everything else (MQTT, presence, Telegram,
//! the ticker) sends it events or commands over channels; it stamps each
//! with the current time, runs the core, and dispatches the resulting
//! actions. Dispatch never waits on the network (alerts are queued for the
//! Telegram worker, MQTT messages for the client), so a slow broker or API
//! cannot delay a decision. After each event the core publishes a
//! [`Snapshot`] that the Telegram bot and the dashboard read.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::Context;
use rumqttc::AsyncClient;
use tokio::sync::{mpsc, oneshot, watch};
use tracing::{error, info, warn};

use crate::config::AppConfig;
use crate::core::{Action, AlertLevel, Command, Engine, Event, Input, Mode, State, Ts};
use crate::mqtt::{self, Connection};
use crate::publish::{state_message, Outgoing, Publisher, AVAILABILITY_TOPIC};
use crate::status::{LogLine, RecentLog, Status};
use crate::telegram::{self, Alert};

/// How often time is fed to the core when nothing else happens.
const TICK: Duration = Duration::from_millis(500);
/// How often the heartbeat file is refreshed.
const HEARTBEAT_EVERY: Duration = Duration::from_secs(10);
pub const HEARTBEAT_FILE: &str = "heartbeat";
const ARMED_FILE: &str = "armed";
/// Recent events kept in memory for /events and the dashboard.
const RECENT: usize = 50;

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

/// Wall-clock time for logs and the event list.
pub fn unix_ms() -> u64 {
    u64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis(),
    )
    .unwrap_or(u64::MAX)
}

/// What readers see: the status and recent events, newest first.
#[derive(Debug, Clone, PartialEq)]
pub struct Snapshot {
    pub status: Status,
    recent: Vec<LogLine>,
}

impl Snapshot {
    pub fn new(status: Status, recent: Vec<LogLine>) -> Self {
        Snapshot { status, recent }
    }

    /// The newest `n` events, newest first.
    pub fn recent(&self, n: usize) -> Vec<LogLine> {
        self.recent.iter().take(n).cloned().collect()
    }
}

struct CommandRequest {
    command: Command,
    by: String,
    reply: oneshot::Sender<Snapshot>,
}

/// How the bot and the dashboard talk to the core.
#[derive(Clone)]
pub struct CoreHandle {
    commands: mpsc::Sender<CommandRequest>,
    snapshot: watch::Receiver<Snapshot>,
}

impl CoreHandle {
    /// Runs a command and returns the snapshot right after it.
    pub async fn command(&self, command: Command, by: String) -> anyhow::Result<Snapshot> {
        let (reply, answer) = oneshot::channel();
        self.commands
            .send(CommandRequest { command, by, reply })
            .await
            .map_err(|_| anyhow::anyhow!("homeguard is shutting down"))?;
        answer
            .await
            .map_err(|_| anyhow::anyhow!("homeguard is shutting down"))
    }

    pub fn snapshot(&self) -> Snapshot {
        self.snapshot.borrow().clone()
    }

    /// A handle backed by a fixed snapshot that records the commands it
    /// receives (answering each with that snapshot). For tests of the bot
    /// and the dashboard; needs a Tokio runtime.
    pub fn for_tests(snapshot: Snapshot) -> (CoreHandle, mpsc::Receiver<(Command, String)>) {
        let (commands, mut requests) = mpsc::channel::<CommandRequest>(8);
        let (seen_tx, seen_rx) = mpsc::channel(8);
        let (_, receiver) = watch::channel(snapshot.clone());
        let handle = CoreHandle {
            commands,
            snapshot: receiver,
        };
        tokio::spawn(async move {
            while let Some(request) = requests.recv().await {
                let _ = seen_tx.send((request.command, request.by)).await;
                let _ = request.reply.send(snapshot.clone());
            }
        });
        (handle, seen_rx)
    }
}

pub struct RunOptions {
    pub data_dir: PathBuf,
    /// Log alerts instead of sending them to Telegram.
    pub dry_run: bool,
}

pub async fn run(config: AppConfig, options: RunOptions) -> anyhow::Result<()> {
    let env = |name: &str| std::env::var(name).ok();
    let (username, password) = config.mqtt.credentials(env).context("MQTT credentials")?;
    let mut engine = config.engine()?;
    let phones = config.presence.phones(env)?;
    let sweep = config.presence.sweep()?;
    let telegram = if options.dry_run {
        warn!("dry run: alerts are logged, not sent to Telegram, and Telegram commands are off");
        None
    } else {
        let credentials = config.telegram.credentials(env).context("Telegram")?;
        if credentials.is_none() {
            warn!(
                "{} is not set: Telegram is OFF, alerts are only logged",
                config.telegram.token_env
            );
        }
        credentials
    };
    let data_dir = options.data_dir;
    std::fs::create_dir_all(&data_dir)
        .with_context(|| format!("creating data directory {}", data_dir.display()))?;

    let (client, eventloop) = Connection::from_config(&config.mqtt, username, password).open();
    let (events_tx, events_rx) = mpsc::channel::<Event>(1024);
    let (commands_tx, commands_rx) = mpsc::channel::<CommandRequest>(32);
    let mut recent = RecentLog::new(RECENT);
    let (snapshot_tx, snapshot_rx) = watch::channel(Snapshot::new(
        Status::from_engine(&engine),
        recent.latest(RECENT),
    ));
    let handle = CoreHandle {
        commands: commands_tx,
        snapshot: snapshot_rx,
    };
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
    let alerts = match telegram {
        Some(credentials) => {
            let bot =
                telegram::Bot::new(&config.telegram, &credentials.token, &config.frigate.url)?;
            let (alerts_tx, alerts_rx) = mpsc::channel::<Alert>(256);
            tokio::spawn(telegram::alert_worker(
                bot.clone(),
                credentials.chats.clone(),
                alerts_rx,
            ));
            tokio::spawn(telegram::command_loop(
                bot,
                config.telegram.clone(),
                credentials.chats,
                handle.clone(),
            ));
            Some(alerts_tx)
        }
        None => None,
    };
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
        telegram = alerts.is_some(),
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
        alerts,
    };
    match read_armed(&data_dir) {
        Ok(Some(mode)) => {
            let actions = engine.restore(mode);
            dispatcher.dispatch(&actions, &mut recent);
        }
        Ok(None) => dispatcher.send(state_message(engine.state())),
        Err(e) => {
            warn!(error = %format!("{e:#}"), "could not read the saved arm state; starting disarmed")
        }
    }

    let channels = Channels {
        events: events_rx,
        commands: commands_rx,
        snapshot: snapshot_tx,
    };
    core_loop(engine, channels, clock, &mut dispatcher, recent).await;

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

struct Channels {
    events: mpsc::Receiver<Event>,
    commands: mpsc::Receiver<CommandRequest>,
    snapshot: watch::Sender<Snapshot>,
}

async fn core_loop(
    mut engine: Engine,
    mut channels: Channels,
    clock: Clock,
    dispatcher: &mut Dispatcher,
    mut recent: RecentLog,
) {
    let mut ticker = tokio::time::interval(TICK);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut last_heartbeat: Option<Instant> = None;
    let shutdown = shutdown_signal();
    tokio::pin!(shutdown);
    loop {
        let (event, reply) = tokio::select! {
            event = channels.events.recv() => match event {
                Some(event) => (event, None),
                None => break,
            },
            request = channels.commands.recv() => match request {
                Some(r) => (Event::Command { command: r.command, by: r.by }, Some(r.reply)),
                None => break,
            },
            _ = ticker.tick() => (Event::Tick, None),
            _ = &mut shutdown => break,
        };
        let actions = engine.handle(&Input::new(clock.now(), event));
        dispatcher.dispatch(&actions, &mut recent);

        let snapshot = Snapshot::new(Status::from_engine(&engine), recent.latest(RECENT));
        channels.snapshot.send_if_modified(|current| {
            let changed = *current != snapshot;
            if changed {
                *current = snapshot.clone();
            }
            changed
        });
        if let Some(reply) = reply {
            let _ = reply.send(snapshot);
        }

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
    /// `None` when Telegram is off or in dry-run mode.
    alerts: Option<mpsc::Sender<Alert>>,
}

impl Dispatcher {
    fn dispatch(&mut self, actions: &[Action], recent: &mut RecentLog) {
        let now = unix_ms();
        for action in actions {
            log_action(action, self.alerts.is_some());
            if let Some(line) = LogLine::from_action(now, action) {
                recent.push(line);
            }
            match action {
                Action::StateChanged { state } => {
                    if let Err(e) = write_armed(&self.data_dir, state) {
                        error!(error = %format!("{e:#}"), "could not save the arm state");
                    }
                }
                Action::SendAlert {
                    level,
                    text,
                    snapshot_ref,
                } => {
                    if let Some(alerts) = &self.alerts {
                        let alert = Alert {
                            level: *level,
                            text: text.clone(),
                            snapshot_ref: snapshot_ref.clone(),
                        };
                        if let Err(e) = alerts.try_send(alert) {
                            error!(error = %e, "Telegram alert queue full; alert dropped");
                        }
                    }
                }
                _ => {}
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

fn log_action(action: &Action, sending: bool) {
    let note = match action {
        Action::SendAlert { .. } if !sending => " (not sent: Telegram off)",
        _ => "",
    };
    match action {
        Action::SendAlert { level, .. } if *level >= AlertLevel::High => warn!("{action}{note}"),
        Action::SirenOn => warn!("{action}"),
        _ => info!("{action}{note}"),
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
