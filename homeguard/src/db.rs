//! The event log: every input event, state change, alert and log line, in
//! SQLite (one file in the data directory), pruned after a retention period.
//!
//! Writes go through [`EventLog::writer`]: the core hands rows to a queue
//! and a task inserts them in batches, so a slow disk never delays a
//! decision.

use std::path::Path;
use std::str::FromStr;
use std::time::Duration;

use anyhow::Context;
use serde::Serialize;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePool, SqlitePoolOptions};
use tokio::sync::mpsc;
use tracing::{info, warn};

use crate::core::machine::describe;
use crate::core::{Action, Event};

/// One row, as stored and as served by the API.
#[derive(Debug, Clone, PartialEq, Serialize, sqlx::FromRow)]
pub struct StoredEvent {
    pub id: i64,
    pub unix_ms: i64,
    pub kind: String,
    pub level: Option<String>,
    pub text: String,
    pub snapshot_ref: Option<String>,
}

/// A row to insert.
#[derive(Debug, Clone, PartialEq)]
pub struct NewEvent {
    pub unix_ms: i64,
    pub kind: &'static str,
    pub level: Option<String>,
    pub text: String,
    pub detail: Option<String>,
    pub snapshot_ref: Option<String>,
}

impl NewEvent {
    /// An input event; `None` for ticks, which are not worth a row.
    pub fn input(unix_ms: u64, event: &Event) -> Option<NewEvent> {
        if *event == Event::Tick {
            return None;
        }
        let snapshot_ref = match event {
            Event::PersonDetected { snapshot_ref, .. } => snapshot_ref.clone(),
            _ => None,
        };
        Some(NewEvent {
            unix_ms: clamp(unix_ms),
            kind: "input",
            level: None,
            text: describe(event),
            detail: serde_json::to_string(event).ok(),
            snapshot_ref,
        })
    }

    pub fn action(unix_ms: u64, action: &Action) -> NewEvent {
        let (kind, level, text, snapshot_ref) = match action {
            Action::StateChanged { state } => ("state", None, state.to_string(), None),
            Action::SendAlert {
                level,
                text,
                snapshot_ref,
            } => (
                "alert",
                Some(level.to_string()),
                text.clone(),
                snapshot_ref.clone(),
            ),
            Action::LogEvent { text } => ("event", None, text.clone(), None),
            other => ("event", None, other.to_string(), None),
        };
        NewEvent {
            unix_ms: clamp(unix_ms),
            kind,
            level,
            text,
            detail: serde_json::to_string(action).ok(),
            snapshot_ref,
        }
    }
}

fn clamp(ms: u64) -> i64 {
    i64::try_from(ms).unwrap_or(i64::MAX)
}

#[derive(Debug, Clone)]
pub struct EventLog {
    pool: SqlitePool,
}

impl EventLog {
    /// Opens (creating if needed) the database file and applies migrations.
    pub async fn open(path: &Path) -> anyhow::Result<EventLog> {
        let options = SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            .busy_timeout(Duration::from_secs(5));
        let pool = SqlitePoolOptions::new()
            .max_connections(4)
            .connect_with(options)
            .await
            .with_context(|| format!("opening {}", path.display()))?;
        EventLog::migrated(pool).await
    }

    /// A private in-memory database, for tests.
    pub async fn in_memory() -> anyhow::Result<EventLog> {
        let options = SqliteConnectOptions::from_str("sqlite::memory:")?;
        // One connection: each in-memory connection is its own database.
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(options)
            .await?;
        EventLog::migrated(pool).await
    }

    async fn migrated(pool: SqlitePool) -> anyhow::Result<EventLog> {
        sqlx::migrate!("./migrations")
            .run(&pool)
            .await
            .context("applying database migrations")?;
        Ok(EventLog { pool })
    }

    pub async fn insert(&self, rows: &[NewEvent]) -> anyhow::Result<()> {
        let mut tx = self.pool.begin().await?;
        for row in rows {
            sqlx::query(
                "INSERT INTO events (unix_ms, kind, level, text, detail, snapshot_ref) \
                 VALUES (?, ?, ?, ?, ?, ?)",
            )
            .bind(row.unix_ms)
            .bind(row.kind)
            .bind(&row.level)
            .bind(&row.text)
            .bind(&row.detail)
            .bind(&row.snapshot_ref)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// The newest `limit` rows, newest first.
    pub async fn latest(&self, limit: u32) -> anyhow::Result<Vec<StoredEvent>> {
        let rows = sqlx::query_as::<_, StoredEvent>(
            "SELECT id, unix_ms, kind, level, text, snapshot_ref FROM events \
             ORDER BY id DESC LIMIT ?",
        )
        .bind(i64::from(limit))
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    /// Deletes rows older than `before_unix_ms`; returns how many.
    pub async fn prune(&self, before_unix_ms: i64) -> anyhow::Result<u64> {
        let result = sqlx::query("DELETE FROM events WHERE unix_ms < ?")
            .bind(before_unix_ms)
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected())
    }

    /// A queue to hand rows to without waiting, and the task that drains it.
    pub fn writer(
        &self,
    ) -> (
        mpsc::Sender<NewEvent>,
        impl std::future::Future<Output = ()>,
    ) {
        let (tx, mut rx) = mpsc::channel::<NewEvent>(4096);
        let log = self.clone();
        let task = async move {
            let mut batch = Vec::new();
            while let Some(row) = rx.recv().await {
                batch.push(row);
                while batch.len() < 256 {
                    match rx.try_recv() {
                        Ok(row) => batch.push(row),
                        Err(_) => break,
                    }
                }
                if let Err(e) = log.insert(&batch).await {
                    warn!(error = %format!("{e:#}"), rows = batch.len(), "event log write failed");
                }
                batch.clear();
            }
        };
        (tx, task)
    }

    /// Prunes every hour, keeping `retention`.
    pub async fn prune_forever(self, retention: Duration) {
        let mut ticker = tokio::time::interval(Duration::from_secs(3600));
        loop {
            ticker.tick().await;
            let now = crate::run::unix_ms();
            let keep = u64::try_from(retention.as_millis()).unwrap_or(u64::MAX);
            let cutoff = clamp(now.saturating_sub(keep));
            match self.prune(cutoff).await {
                Ok(0) => {}
                Ok(n) => info!(rows = n, "pruned old events"),
                Err(e) => warn!(error = %format!("{e:#}"), "pruning the event log failed"),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::{AlertLevel, MotionSource};

    #[tokio::test]
    async fn stores_lists_and_prunes() {
        let log = EventLog::in_memory().await.unwrap();
        let rows = vec![
            NewEvent::input(
                1_000,
                &Event::MotionDetected {
                    source: MotionSource::Pir,
                    sensor: "hall".into(),
                },
            )
            .unwrap(),
            NewEvent::action(
                2_000,
                &Action::SendAlert {
                    level: AlertLevel::Critical,
                    text: "ALARM".into(),
                    snapshot_ref: Some("evt-1".into()),
                },
            ),
            NewEvent::action(3_000, &Action::log("x")),
        ];
        log.insert(&rows).await.unwrap();
        let latest = log.latest(2).await.unwrap();
        assert_eq!(latest.len(), 2);
        assert_eq!(latest[0].text, "x");
        assert_eq!(latest[1].kind, "alert");
        assert_eq!(latest[1].level.as_deref(), Some("critical"));
        assert_eq!(latest[1].snapshot_ref.as_deref(), Some("evt-1"));
        assert_eq!(log.prune(2_500).await.unwrap(), 2);
        assert_eq!(log.latest(10).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn the_writer_drains_its_queue() {
        let log = EventLog::in_memory().await.unwrap();
        let (tx, task) = log.writer();
        let handle = tokio::spawn(task);
        for i in 0..300 {
            tx.send(NewEvent::action(i, &Action::log(format!("e{i}"))))
                .await
                .unwrap();
        }
        drop(tx);
        handle.await.unwrap();
        assert_eq!(log.latest(1000).await.unwrap().len(), 300);
    }

    #[test]
    fn ticks_are_not_stored() {
        assert!(NewEvent::input(0, &Event::Tick).is_none());
        let person = NewEvent::input(
            0,
            &Event::PersonDetected {
                camera: "cam".into(),
                score: 0.9,
                snapshot_ref: Some("e".into()),
            },
        )
        .unwrap();
        assert_eq!(person.snapshot_ref.as_deref(), Some("e"));
        assert!(person.detail.unwrap().contains("person_detected"));
    }

    #[tokio::test]
    async fn opens_a_file_and_reopens_it() {
        let dir = std::env::temp_dir().join(format!("homeguard-db-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("events.db");
        {
            let log = EventLog::open(&path).await.unwrap();
            log.insert(&[NewEvent::action(1, &Action::log("kept"))])
                .await
                .unwrap();
        }
        let log = EventLog::open(&path).await.unwrap();
        assert_eq!(log.latest(5).await.unwrap()[0].text, "kept");
        drop(log);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
