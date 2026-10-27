-- Every input event, state change, alert and log line, oldest first.
CREATE TABLE events (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    -- Wall-clock time, milliseconds since the Unix epoch.
    unix_ms      INTEGER NOT NULL,
    -- input | state | alert | event
    kind         TEXT    NOT NULL,
    -- Alerts only: low | info | high | critical
    level        TEXT,
    text         TEXT    NOT NULL,
    -- The input event or action as JSON.
    detail       TEXT,
    -- Frigate event id, when there is a snapshot.
    snapshot_ref TEXT
);

CREATE INDEX events_by_time ON events (unix_ms);
