//! End-to-end: replay a checked-in synthetic recording through the real
//! binary and check the events it reports.
//!
//! The fixture (`tests/fixtures/synthetic_walk.csv`, regenerate with
//! `cargo test regenerate_replay_fixture -- --ignored`) is 60 s of quantized
//! RSSI noise around -55 dBm at 20 Hz, with one burst of motion from 35 s to
//! 42 s.

use std::process::Command;

use serde_json::Value;

const FIXTURE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/synthetic_walk.csv"
);
/// `ts_unix_ms` of the first row.
const T0: u64 = 1_790_000_000_000;

fn replay(extra: &[&str]) -> (Vec<Value>, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_wisense"))
        .args(["replay", FIXTURE, "--speed", "0", "--json"])
        .args(extra)
        .output()
        .expect("running wisense");
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(out.status.success(), "wisense failed: {stderr}");
    let events = String::from_utf8(out.stdout)
        .expect("utf-8 output")
        .lines()
        .map(|l| serde_json::from_str(l).unwrap_or_else(|e| panic!("bad JSON line {l:?}: {e}")))
        .collect();
    (events, stderr)
}

fn kind(e: &Value) -> &str {
    e["event"].as_str().unwrap_or_default()
}

/// Seconds since the start of the recording.
fn at(e: &Value) -> f64 {
    (e["ts_unix_ms"].as_u64().expect("ts_unix_ms") - T0) as f64 / 1000.0
}

#[test]
fn replay_reports_the_burst() {
    let (events, stderr) = replay(&[]);
    let notable: Vec<&Value> = events.iter().filter(|e| kind(e) != "Status").collect();
    let kinds: Vec<&str> = notable.iter().map(|e| kind(e)).collect();
    assert_eq!(
        kinds,
        [
            "CalibrationStarted",
            "Calibrated",
            "MotionStarted",
            "MotionEnded"
        ],
        "{notable:#?}"
    );

    // Auto-calibration over the quiet first 20 s.
    assert_eq!(at(notable[1]), 20.0);
    let threshold = notable[1]["threshold"].as_f64().unwrap();
    assert!(threshold > 0.0);

    // Motion starts shortly after the burst begins (35 s)...
    let started = at(notable[2]);
    assert!((35.0..38.0).contains(&started), "started at {started}");
    assert!(notable[2]["score"].as_f64().unwrap() > threshold);
    // ...and ends only after the burst (42 s) has left the 2 s window and
    // the score stayed low for the 3 s off time.
    let ended = at(notable[3]);
    assert!((47.0..58.0).contains(&ended), "ended at {ended}");
    assert!(notable[3]["peak_score"].as_f64().unwrap() > threshold);

    // Status every 500 ms of recording time.
    let statuses = events.iter().filter(|e| kind(e) == "Status").count();
    assert_eq!(statuses, 120);
    assert!(stderr.contains("CALIBRATING"), "{stderr}");
}

#[test]
fn higher_sensitivity_value_misses_the_weak_burst() {
    // k = 50 puts the threshold far above anything in the recording.
    let (events, _) = replay(&["--sensitivity", "50"]);
    assert!(events.iter().all(|e| kind(e) != "MotionStarted"));
}

#[test]
fn replay_with_a_saved_baseline_skips_calibration() {
    let dir = std::env::temp_dir().join(format!("wisense-it-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let baseline = dir.join("baseline.json");
    std::fs::write(
        &baseline,
        r#"{"version":1,"links":{"wlan0":{"mean":0.26,"std":0.064,"kind":"rssi",
            "metric":"diff","window_ms":2000,"poll_hz":20.0,"seconds":60.0,
            "calibrated_unix_ms":0}}}"#,
    )
    .unwrap();
    let (events, stderr) = replay(&["--baseline", baseline.to_str().unwrap()]);
    std::fs::remove_dir_all(&dir).unwrap();
    assert!(
        events.iter().all(|e| kind(e) != "CalibrationStarted"),
        "{stderr}"
    );
    let starts: Vec<f64> = events
        .iter()
        .filter(|e| kind(e) == "MotionStarted")
        .map(at)
        .collect();
    assert_eq!(starts.len(), 1, "{starts:?}");
    assert!((35.0..38.0).contains(&starts[0]));
}

#[test]
fn missing_file_is_a_clear_error() {
    let out = Command::new(env!("CARGO_BIN_EXE_wisense"))
        .args(["replay", "/nonexistent/recording.csv"])
        .output()
        .expect("running wisense");
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("opening /nonexistent/recording.csv"),
        "{stderr}"
    );
}
