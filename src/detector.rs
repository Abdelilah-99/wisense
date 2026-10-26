//! Motion detection: windowed statistics, baseline, threshold and hysteresis.
//!
//! This module is pure. It never reads a clock or does I/O; time comes from
//! [`Sample::t`]. The same code runs live, on replays and in tests.
//!
//! Per link:
//! 1. keep a sliding window of samples (`DetectorConfig::window`),
//! 2. compute a motion score from it: the mean absolute first difference or
//!    the standard deviation of RSSI, or for CSI links the mean variance of
//!    the most variant subcarriers,
//! 3. compare the score with `baseline_mean + k * baseline_std`; the baseline
//!    comes from a calibration file or is learned from the first seconds,
//! 4. debounce with hysteresis: MOTION starts after the score has stayed above
//!    the threshold for `on`, and ends after it has stayed below
//!    `off_ratio * threshold` for `off`.

use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::config::{DetectorConfig, Metric};
use crate::sample::Sample;

/// A calibration needs at least this many scores before it can finish.
const MIN_CALIBRATION_SCORES: u64 = 10;
/// A window only yields a score once its oldest sample is at least this
/// fraction of the window length old. Scores from a handful of samples (at
/// start-up or after a gap) are too noisy to compare with the baseline.
const WARM_FRACTION: f64 = 0.75;
/// The CSI score averages this fraction of the most variant subcarriers.
const CSI_TOP_FRACTION: f32 = 0.25;

/// Distribution of the motion score in an empty, still room.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Baseline {
    pub mean: f32,
    pub std: f32,
}

impl Baseline {
    /// `mean + k * std`, but at least `min_threshold_ratio * mean` and at
    /// least `min_threshold`.
    ///
    /// The ratio floor matters when the idle score barely varies (small std):
    /// without it the threshold sits just above the idle mean, the release
    /// level `off_ratio * threshold` falls *below* normal idle scores, and
    /// MOTION never ends.
    pub fn threshold(&self, cfg: &DetectorConfig) -> f32 {
        (self.mean + cfg.sensitivity * self.std)
            .max(cfg.min_threshold_ratio * self.mean)
            .max(cfg.min_threshold)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum MotionState {
    Calibrating,
    Idle,
    Motion,
}

/// What a link's score is computed from. Decides the unit shown to users.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ScoreKind {
    /// RSSI statistic, in dB.
    Rssi,
    /// CSI amplitude variance (unitless).
    Csi,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "event")]
pub enum Event {
    /// No baseline for this link: the first `duration_ms` are used to learn one.
    CalibrationStarted {
        link_id: String,
        ts_unix_ms: u64,
        duration_ms: u64,
    },
    /// Calibration finished; detection is active from now on.
    Calibrated {
        link_id: String,
        ts_unix_ms: u64,
        kind: ScoreKind,
        baseline: Baseline,
        threshold: f32,
    },
    MotionStarted {
        link_id: String,
        ts_unix_ms: u64,
        kind: ScoreKind,
        score: f32,
        threshold: f32,
    },
    /// `duration_ms` runs from when the score first crossed the threshold to
    /// when it last dropped below the release level, so it excludes the
    /// debounce delays.
    MotionEnded {
        link_id: String,
        ts_unix_ms: u64,
        kind: ScoreKind,
        duration_ms: u64,
        peak_score: f32,
    },
    /// Periodic snapshot. `score` is `None` until the window has filled;
    /// `threshold` is `None` while calibrating.
    Status {
        link_id: String,
        ts_unix_ms: u64,
        kind: ScoreKind,
        score: Option<f32>,
        threshold: Option<f32>,
        state: MotionState,
    },
    /// Most disturbed link while there is motion (`None`: no motion). Only
    /// with two or more links; produced by `zones::ZoneTracker`, not by the
    /// detector. `confidence` is the lead of the top link's `score /
    /// threshold` over the runner-up's.
    ZoneChanged {
        ts_unix_ms: u64,
        zone: Option<String>,
        confidence: f32,
    },
}

/// Mean absolute first difference: `sum |x[i] - x[i-1]| / n` over the `n`
/// differences. Needs at least two values.
pub fn mean_abs_diff(values: impl IntoIterator<Item = f32>) -> Option<f32> {
    let mut values = values.into_iter();
    let mut prev = values.next()?;
    let (mut sum, mut n) = (0.0f64, 0u32);
    for x in values {
        sum += f64::from((x - prev).abs());
        n += 1;
        prev = x;
    }
    (n > 0).then(|| (sum / f64::from(n)) as f32)
}

/// Population standard deviation. Needs at least two values.
pub fn std_dev(values: impl IntoIterator<Item = f32>) -> Option<f32> {
    let mut stats = RunningStats::default();
    for x in values {
        stats.push(x);
    }
    stats.baseline().map(|b| b.std)
}

/// CSI motion score: per subcarrier, the variance of its amplitude over the
/// window; the score is the mean of the top 25 % most variant subcarriers.
/// Subcarriers that are zero throughout (guard/null carriers) are ignored.
/// All rows must have the same length; needs at least two rows.
pub fn csi_score(rows: &[&[f32]]) -> Option<f32> {
    let width = rows.first()?.len();
    if rows.len() < 2 || rows.iter().any(|r| r.len() != width) {
        return None;
    }
    let mut variances: Vec<f32> = (0..width)
        .filter_map(|j| {
            let column = rows.iter().filter_map(|r| r.get(j).copied());
            if column.clone().all(|a| a == 0.0) {
                return None;
            }
            let mut stats = RunningStats::default();
            column.for_each(|a| stats.push(a));
            stats.baseline().map(|b| b.std * b.std)
        })
        .collect();
    if variances.is_empty() {
        return None;
    }
    variances.sort_by(|a, b| b.total_cmp(a));
    let top = ((variances.len() as f32 * CSI_TOP_FRACTION).ceil() as usize).max(1);
    Some(variances.iter().take(top).sum::<f32>() / top as f32)
}

/// Welford's online mean/variance.
#[derive(Debug, Clone, Default)]
pub struct RunningStats {
    n: u64,
    mean: f64,
    m2: f64,
}

impl RunningStats {
    pub fn push(&mut self, x: f32) {
        let x = f64::from(x);
        self.n += 1;
        let delta = x - self.mean;
        self.mean += delta / self.n as f64;
        self.m2 += delta * (x - self.mean);
    }

    pub fn count(&self) -> u64 {
        self.n
    }

    /// Mean and population std, once there are at least two values.
    pub fn baseline(&self) -> Option<Baseline> {
        (self.n >= 2).then(|| Baseline {
            mean: self.mean as f32,
            std: (self.m2 / self.n as f64).sqrt() as f32,
        })
    }
}

/// Edge produced by [`Hysteresis::update`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Transition {
    /// `since` is when the score first crossed the threshold.
    Started { since: Instant },
    Ended {
        started: Instant,
        ended: Instant,
        peak: f32,
    },
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum HystState {
    Idle {
        above_since: Option<Instant>,
        peak: f32,
    },
    Motion {
        started: Instant,
        below_since: Option<Instant>,
        peak: f32,
    },
}

/// Debounced two-level state machine.
///
/// IDLE -> MOTION once `score > threshold` has held for `on`.
/// MOTION -> IDLE once `score < off_ratio * threshold` has held for `off`.
#[derive(Debug, Clone)]
pub struct Hysteresis {
    on: Duration,
    off: Duration,
    off_ratio: f32,
    state: HystState,
}

impl Hysteresis {
    pub fn new(on: Duration, off: Duration, off_ratio: f32) -> Self {
        Self {
            on,
            off,
            off_ratio,
            state: HystState::Idle {
                above_since: None,
                peak: 0.0,
            },
        }
    }

    pub fn in_motion(&self) -> bool {
        matches!(self.state, HystState::Motion { .. })
    }

    pub fn update(&mut self, score: f32, threshold: f32, now: Instant) -> Option<Transition> {
        match &mut self.state {
            HystState::Idle { above_since, peak } => {
                if score <= threshold {
                    *above_since = None;
                    *peak = 0.0;
                    return None;
                }
                let since = *above_since.get_or_insert(now);
                *peak = peak.max(score);
                if now.duration_since(since) < self.on {
                    return None;
                }
                self.state = HystState::Motion {
                    started: since,
                    below_since: None,
                    peak: *peak,
                };
                Some(Transition::Started { since })
            }
            HystState::Motion {
                started,
                below_since,
                peak,
            } => {
                *peak = peak.max(score);
                if score >= self.off_ratio * threshold {
                    *below_since = None;
                    return None;
                }
                let since = *below_since.get_or_insert(now);
                if now.duration_since(since) < self.off {
                    return None;
                }
                let ended = Transition::Ended {
                    started: *started,
                    ended: since,
                    peak: *peak,
                };
                self.state = HystState::Idle {
                    above_since: None,
                    peak: 0.0,
                };
                Some(ended)
            }
        }
    }
}

struct Entry {
    t: Instant,
    rssi: Option<f32>,
    csi: Option<Vec<f32>>,
}

enum Phase {
    Calibrating {
        started: Instant,
        stats: RunningStats,
    },
    Active {
        threshold: f32,
        hysteresis: Hysteresis,
    },
}

struct Link {
    kind: ScoreKind,
    window: VecDeque<Entry>,
    phase: Phase,
    last_status: Option<Instant>,
}

impl Link {
    fn push(&mut self, entry: Entry, window: Duration) {
        let now = entry.t;
        self.window.push_back(entry);
        while self
            .window
            .front()
            .is_some_and(|e| now.duration_since(e.t) > window)
        {
            self.window.pop_front();
        }
    }

    fn score(&self, now: Instant, cfg: &DetectorConfig) -> Option<f32> {
        let oldest = self.window.front()?;
        if now.duration_since(oldest.t).as_secs_f64() < cfg.window.as_secs_f64() * WARM_FRACTION {
            return None;
        }
        match self.kind {
            ScoreKind::Csi => {
                // Only compare rows of the same layout as the newest one; the
                // subcarrier count can change with the frame type.
                let width = self.window.iter().rev().find_map(|e| e.csi.as_ref())?.len();
                let rows: Vec<&[f32]> = self
                    .window
                    .iter()
                    .filter_map(|e| e.csi.as_deref())
                    .filter(|c| c.len() == width)
                    .collect();
                csi_score(&rows)
            }
            ScoreKind::Rssi => {
                let values = self.window.iter().filter_map(|e| e.rssi);
                match cfg.metric {
                    Metric::Diff => mean_abs_diff(values),
                    Metric::Std => std_dev(values),
                }
            }
        }
    }

    fn state(&self) -> MotionState {
        match &self.phase {
            Phase::Calibrating { .. } => MotionState::Calibrating,
            Phase::Active { hysteresis, .. } if hysteresis.in_motion() => MotionState::Motion,
            Phase::Active { .. } => MotionState::Idle,
        }
    }

    fn threshold(&self) -> Option<f32> {
        match &self.phase {
            Phase::Calibrating { .. } => None,
            Phase::Active { threshold, .. } => Some(*threshold),
        }
    }
}

/// Per-link motion detector. Feed it samples in time order (per link).
pub struct Detector {
    cfg: DetectorConfig,
    baselines: HashMap<String, Baseline>,
    links: HashMap<String, Link>,
}

impl Detector {
    /// `baselines` maps `link_id` to a calibrated baseline. Links without one
    /// calibrate themselves from their first `cfg.auto_calibration`.
    pub fn new(cfg: DetectorConfig, baselines: HashMap<String, Baseline>) -> Self {
        Self {
            cfg,
            baselines,
            links: HashMap::new(),
        }
    }

    pub fn process(&mut self, sample: Sample) -> Vec<Event> {
        let mut events = Vec::new();
        let now = sample.t;
        let ts = sample.ts_unix_ms;

        if !self.links.contains_key(&sample.link_id) {
            let link = self.new_link(&sample, &mut events);
            self.links.insert(sample.link_id.clone(), link);
        }
        let cfg = &self.cfg;
        let Some(link) = self.links.get_mut(&sample.link_id) else {
            return events;
        };
        let link_id = sample.link_id;

        link.push(
            Entry {
                t: now,
                rssi: sample.rssi_dbm,
                csi: sample.csi_amplitudes,
            },
            cfg.window,
        );
        let score = link.score(now, cfg);
        let kind = link.kind;

        let mut calibrated = None;
        match &mut link.phase {
            Phase::Calibrating { started, stats } => {
                if let Some(s) = score {
                    stats.push(s);
                }
                if now.duration_since(*started) >= cfg.auto_calibration
                    && stats.count() >= MIN_CALIBRATION_SCORES
                {
                    calibrated = stats.baseline();
                }
            }
            Phase::Active {
                threshold,
                hysteresis,
            } => match score.and_then(|s| Some((s, hysteresis.update(s, *threshold, now)?))) {
                Some((score, Transition::Started { .. })) => events.push(Event::MotionStarted {
                    link_id: link_id.clone(),
                    ts_unix_ms: ts,
                    kind,
                    score,
                    threshold: *threshold,
                }),
                Some((
                    _,
                    Transition::Ended {
                        started,
                        ended,
                        peak,
                    },
                )) => events.push(Event::MotionEnded {
                    link_id: link_id.clone(),
                    ts_unix_ms: ts,
                    kind,
                    duration_ms: duration_ms(ended.duration_since(started)),
                    peak_score: peak,
                }),
                None => {}
            },
        }

        if let Some(baseline) = calibrated {
            let threshold = baseline.threshold(cfg);
            link.phase = Phase::Active {
                threshold,
                hysteresis: Hysteresis::new(cfg.on, cfg.off, cfg.off_ratio),
            };
            self.baselines.insert(link_id.clone(), baseline);
            events.push(Event::Calibrated {
                link_id: link_id.clone(),
                ts_unix_ms: ts,
                kind,
                baseline,
                threshold,
            });
        }

        if link
            .last_status
            .is_none_or(|t| now.duration_since(t) >= cfg.status_every)
        {
            link.last_status = Some(now);
            events.push(Event::Status {
                link_id,
                ts_unix_ms: ts,
                kind,
                score,
                threshold: link.threshold(),
                state: link.state(),
            });
        }
        events
    }

    fn new_link(&self, sample: &Sample, events: &mut Vec<Event>) -> Link {
        let kind = if sample.csi_amplitudes.is_some() {
            ScoreKind::Csi
        } else {
            ScoreKind::Rssi
        };
        let phase = match self.baselines.get(&sample.link_id) {
            Some(b) => Phase::Active {
                threshold: b.threshold(&self.cfg),
                hysteresis: Hysteresis::new(self.cfg.on, self.cfg.off, self.cfg.off_ratio),
            },
            None => {
                events.push(Event::CalibrationStarted {
                    link_id: sample.link_id.clone(),
                    ts_unix_ms: sample.ts_unix_ms,
                    duration_ms: duration_ms(self.cfg.auto_calibration),
                });
                Phase::Calibrating {
                    started: sample.t,
                    stats: RunningStats::default(),
                }
            }
        };
        Link {
            kind,
            window: VecDeque::new(),
            phase,
            last_status: None,
        }
    }
}

fn duration_ms(d: Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::synthetic::{Burst, SyntheticConfig, SyntheticSignal};

    const HZ: u64 = 20;
    const DT: Duration = Duration::from_millis(1000 / HZ);

    /// `t0` plus `s` seconds, rounded to whole milliseconds so comparisons
    /// against the on/off durations are exact.
    fn at(t0: Instant, s: f64) -> Instant {
        t0 + Duration::from_millis((s * 1000.0).round() as u64)
    }

    /// Feed `values` (one per DT, starting at t0) and return the non-status
    /// events with their offset in seconds from t0.
    fn run(det: &mut Detector, t0: Instant, values: &[f32]) -> Vec<(f64, Event)> {
        let mut out = Vec::new();
        for (i, v) in values.iter().enumerate() {
            let t = t0 + DT * i as u32;
            let s = Sample::rssi("wlan0", t, 1_000_000 + i as u64 * 50, *v);
            for ev in det.process(s) {
                if !matches!(ev, Event::Status { .. }) {
                    out.push((t.duration_since(t0).as_secs_f64(), ev));
                }
            }
        }
        out
    }

    fn synthetic(cfg: SyntheticConfig, t0: Instant) -> Vec<f32> {
        let mut sig = SyntheticSignal::new(cfg, t0, 0);
        std::iter::from_fn(|| sig.next_sample())
            .filter_map(|s| s.rssi_dbm)
            .collect()
    }

    fn starts(events: &[(f64, Event)]) -> Vec<f64> {
        events
            .iter()
            .filter(|(_, e)| matches!(e, Event::MotionStarted { .. }))
            .map(|(t, _)| *t)
            .collect()
    }

    fn ends(events: &[(f64, Event)]) -> Vec<(f64, u64)> {
        events
            .iter()
            .filter_map(|(t, e)| match e {
                Event::MotionEnded { duration_ms, .. } => Some((*t, *duration_ms)),
                _ => None,
            })
            .collect()
    }

    fn with_baseline(cfg: DetectorConfig, mean: f32, std: f32) -> Detector {
        let baselines = HashMap::from([("wlan0".to_string(), Baseline { mean, std })]);
        Detector::new(cfg, baselines)
    }

    #[test]
    fn metrics() {
        assert_eq!(mean_abs_diff([1.0, 2.0, 0.0, 0.0]), Some(1.0));
        assert_eq!(mean_abs_diff([5.0]), None);
        assert_eq!(std_dev([2.0, 4.0, 4.0, 4.0, 5.0, 5.0, 7.0, 9.0]), Some(2.0));
        assert_eq!(std_dev([1.0]), None);
    }

    #[test]
    fn csi_score_ignores_null_subcarriers_and_takes_top_quarter() {
        // 8 subcarriers: 0 and 7 are null (always 0); variances of the others
        // are 0, 0, 0, 0, 1, 4 (amplitudes +-1 and +-2 around 10).
        let rows: Vec<Vec<f32>> = (0..10)
            .map(|i| {
                let s = if i % 2 == 0 { 1.0 } else { -1.0 };
                vec![0.0, 5.0, 5.0, 5.0, 5.0, 10.0 + s, 10.0 + 2.0 * s, 0.0]
            })
            .collect();
        let refs: Vec<&[f32]> = rows.iter().map(Vec::as_slice).collect();
        // 6 live subcarriers -> top ceil(1.5) = 2 -> mean(4, 1) = 2.5
        let score = csi_score(&refs).unwrap();
        assert!((score - 2.5).abs() < 1e-5, "{score}");

        let short: &[f32] = &[1.0, 2.0];
        assert_eq!(csi_score(&[refs[0], short]), None);
        assert_eq!(csi_score(&refs[..1]), None);
    }

    #[test]
    fn baseline_threshold_floors() {
        let cfg = DetectorConfig::default();
        let b = Baseline {
            mean: 0.2,
            std: 0.1,
        };
        assert!((b.threshold(&cfg) - 0.6).abs() < 1e-6);
        // A score that barely varies: threshold is twice the idle mean, so
        // the release level (0.7 * 0.4 = 0.28) stays above idle scores.
        let steady = Baseline {
            mean: 0.2,
            std: 0.01,
        };
        assert!((steady.threshold(&cfg) - 0.4).abs() < 1e-6);
        // A perfectly still calibration falls back to the minimum threshold.
        let zero = Baseline {
            mean: 0.0,
            std: 0.0,
        };
        assert_eq!(zero.threshold(&cfg), cfg.min_threshold);
    }

    #[test]
    fn flat_noisy_signal_never_triggers() {
        for metric in [Metric::Diff, Metric::Std] {
            for seed in 1..=5 {
                let t0 = Instant::now();
                let values = synthetic(
                    SyntheticConfig {
                        seed,
                        duration: Some(Duration::from_secs(600)),
                        ..SyntheticConfig::default()
                    },
                    t0,
                );
                // A 60 s calibration, as `wisense calibrate` does by default.
                // The 20 s auto-calibration sees only ~10 independent windows
                // and can underestimate the idle score enough to let pure
                // noise through now and then (seed 3 does).
                //
                // Even so, k = 4 is a statistical threshold, not a guarantee:
                // over 40 seeds x 10 min of this noise the diff metric gave
                // about one false start per 100 min (std: none; k = 5: none).
                // Seeds 1-5 are clean runs, and the tests stay deterministic.
                let cfg = DetectorConfig {
                    metric,
                    auto_calibration: Duration::from_secs(60),
                    ..DetectorConfig::default()
                };
                let mut det = Detector::new(cfg, HashMap::new());
                let events = run(&mut det, t0, &values);
                assert!(
                    matches!(events.first(), Some((_, Event::CalibrationStarted { .. }))),
                    "{events:?}"
                );
                let calibrated: Vec<f64> = events
                    .iter()
                    .filter(|(_, e)| matches!(e, Event::Calibrated { .. }))
                    .map(|(t, _)| *t)
                    .collect();
                assert_eq!(calibrated.len(), 1, "{metric:?} seed {seed}: {events:?}");
                assert!((60.0..61.0).contains(&calibrated[0]));
                assert!(
                    starts(&events).is_empty(),
                    "{metric:?} seed {seed}: false motion {events:?}"
                );
            }
        }
    }

    #[test]
    fn synthetic_bursts_are_detected() {
        let t0 = Instant::now();
        let bursts: Vec<Burst> = [40.0, 70.0, 100.0]
            .iter()
            .map(|&s| Burst {
                start: Duration::from_secs_f64(s),
                duration: Duration::from_secs(5),
                std_db: 3.0,
            })
            .collect();
        let values = synthetic(
            SyntheticConfig {
                duration: Some(Duration::from_secs(130)),
                bursts,
                ..SyntheticConfig::default()
            },
            t0,
        );
        let mut det = Detector::new(DetectorConfig::default(), HashMap::new());
        let events = run(&mut det, t0, &values);
        let starts = starts(&events);
        assert_eq!(starts.len(), 3, "{events:?}");
        for (start, burst) in starts.iter().zip([40.0, 70.0, 100.0]) {
            assert!(*start > burst && *start < burst + 3.0, "{start} vs {burst}");
        }
        assert_eq!(ends(&events).len(), 3, "{events:?}");
    }

    /// Flat signal with a burst of +-5 dB alternation from 10 s to 14 s.
    fn step_burst() -> Vec<f32> {
        (0..30 * HZ)
            .map(|i| {
                let t = i as f64 / HZ as f64;
                if (10.0..14.0).contains(&t) {
                    if i % 2 == 0 {
                        -50.0
                    } else {
                        -60.0
                    }
                } else {
                    -55.0
                }
            })
            .collect()
    }

    #[test]
    fn burst_triggers_after_on_ms_and_ends_after_off_ms() {
        let cfg = DetectorConfig::default();
        let (on, off, window) = (
            cfg.on.as_secs_f64(),
            cfg.off.as_secs_f64(),
            cfg.window.as_secs_f64(),
        );
        // Threshold 0.05 + 4 * 0.05 = 0.25 dB.
        let mut det = with_baseline(cfg, 0.05, 0.05);
        let t0 = Instant::now();
        let events = run(&mut det, t0, &step_burst());

        let starts = starts(&events);
        assert_eq!(starts.len(), 1, "{events:?}");
        // The score crosses the threshold on the second burst sample, then has
        // to stay above it for `on`.
        let crossed = 10.0 + DT.as_secs_f64();
        assert!(starts[0] >= crossed + on - 1e-6, "started at {}", starts[0]);
        assert!(starts[0] < crossed + on + 0.1, "started at {}", starts[0]);

        let ends = ends(&events);
        assert_eq!(ends.len(), 1, "{events:?}");
        let (ended, duration_ms) = ends[0];
        // The burst leaves the window `window` after it stops, then the score
        // has to stay low for `off`.
        assert!(ended >= 14.0 + off, "ended at {ended}");
        assert!(ended <= 14.0 + window + off + 0.1, "ended at {ended}");
        // Reported duration excludes the debounce delays.
        let expected_ms = (14.0 + window - crossed) * 1000.0;
        assert!(
            (duration_ms as f64 - expected_ms).abs() < 300.0,
            "{duration_ms} vs {expected_ms}"
        );
        assert!(matches!(
            events.last(),
            Some((_, Event::MotionEnded { peak_score, .. })) if *peak_score > 3.0
        ));
    }

    #[test]
    fn no_score_until_window_is_warm() {
        let mut det = with_baseline(DetectorConfig::default(), 0.05, 0.05);
        let t0 = Instant::now();
        // A wildly varying start must not trigger from a half-empty window...
        let mut scores = Vec::new();
        for i in 0..40u32 {
            let v = if i % 2 == 0 { -40.0 } else { -70.0 };
            for ev in det.process(Sample::rssi("wlan0", t0 + DT * i, 0, v)) {
                if let Event::Status { score, .. } = ev {
                    scores.push((i, score));
                }
            }
        }
        // ...the first status (t = 0) has no score, later ones (window >= 75 %
        // full, i.e. after 1.5 s) do.
        assert_eq!(scores[0], (0, None));
        assert!(scores
            .iter()
            .filter(|(i, _)| *i < 30)
            .all(|(_, s)| s.is_none()));
        assert!(scores
            .iter()
            .filter(|(i, _)| *i >= 30)
            .all(|(_, s)| s.is_some()));
    }

    #[test]
    fn gap_in_samples_resets_the_window() {
        let mut det = with_baseline(DetectorConfig::default(), 0.05, 0.05);
        let t0 = Instant::now();
        let mut last_score = None;
        for i in 0..60u32 {
            for ev in det.process(Sample::rssi("wlan0", t0 + DT * i, 0, -55.0)) {
                if let Event::Status { score, .. } = ev {
                    last_score = score;
                }
            }
        }
        assert_eq!(last_score, Some(0.0));
        // 10 s later: the old samples are gone and the window is cold again.
        let later = t0 + Duration::from_secs(13);
        let events = det.process(Sample::rssi("wlan0", later, 0, -60.0));
        assert!(events
            .iter()
            .any(|e| matches!(e, Event::Status { score: None, .. })));
    }

    #[test]
    fn links_are_independent() {
        let mut det = Detector::new(
            DetectorConfig::default(),
            HashMap::from([
                (
                    "a".to_string(),
                    Baseline {
                        mean: 0.05,
                        std: 0.05,
                    },
                ),
                (
                    "b".to_string(),
                    Baseline {
                        mean: 0.05,
                        std: 0.05,
                    },
                ),
            ]),
        );
        let t0 = Instant::now();
        let burst = step_burst();
        let mut started = Vec::new();
        for (i, v) in burst.iter().enumerate() {
            let t = t0 + DT * i as u32;
            for (link, value) in [("a", *v), ("b", -55.0)] {
                for ev in det.process(Sample::rssi(link, t, 0, value)) {
                    if let Event::MotionStarted { link_id, .. } = ev {
                        started.push(link_id);
                    }
                }
            }
        }
        assert_eq!(started, vec!["a".to_string()]);
    }

    #[test]
    fn status_is_periodic() {
        let mut det = with_baseline(DetectorConfig::default(), 0.05, 0.05);
        let t0 = Instant::now();
        let statuses = (0..200u32) // 10 s
            .flat_map(|i| det.process(Sample::rssi("wlan0", t0 + DT * i, 0, -55.0)))
            .filter(|e| matches!(e, Event::Status { .. }))
            .count();
        assert_eq!(statuses, 20);
    }

    #[test]
    fn csi_link_detects_motion_on_a_few_subcarriers() {
        let mut det = Detector::new(DetectorConfig::default(), HashMap::new());
        let t0 = Instant::now();
        let mut rng = crate::source::synthetic::Rng::new(3);
        let mut events = Vec::new();
        for i in 0..(60 * HZ) as u32 {
            let t = t0 + DT * i;
            let moving = (35.0..40.0).contains(&t.duration_since(t0).as_secs_f64());
            let csi: Vec<f32> = (0..64)
                .map(|j| {
                    if j < 6 || j == 32 || j > 58 {
                        return 0.0; // guard and null subcarriers
                    }
                    let noise = 0.3 * rng.normal() as f32;
                    let motion = if moving && (10..20).contains(&j) {
                        4.0 * rng.normal() as f32
                    } else {
                        0.0
                    };
                    (20.0 + noise + motion).max(0.0)
                })
                .collect();
            let s = Sample {
                link_id: "esp32-kitchen".into(),
                t,
                ts_unix_ms: 0,
                rssi_dbm: Some(-60.0),
                csi_amplitudes: Some(csi),
            };
            events.extend(det.process(s).into_iter().filter_map(|e| match e {
                Event::Calibrated { kind, .. } => Some(("calibrated", kind)),
                Event::MotionStarted { kind, .. } => Some(("started", kind)),
                Event::MotionEnded { kind, .. } => Some(("ended", kind)),
                _ => None,
            }));
        }
        assert_eq!(
            events,
            vec![
                ("calibrated", ScoreKind::Csi),
                ("started", ScoreKind::Csi),
                ("ended", ScoreKind::Csi)
            ]
        );
    }

    mod hysteresis {
        use super::*;

        fn hyst() -> Hysteresis {
            Hysteresis::new(Duration::from_millis(600), Duration::from_millis(3000), 0.7)
        }

        /// Feed (seconds, score) pairs, return transitions with their time.
        fn feed(h: &mut Hysteresis, t0: Instant, scores: &[(f64, f32)]) -> Vec<(f64, Transition)> {
            scores
                .iter()
                .filter_map(|&(s, score)| Some((s, h.update(score, 1.0, at(t0, s))?)))
                .collect()
        }

        fn every_100ms(from: f64, to: f64, score: impl Fn(usize) -> f32) -> Vec<(f64, f32)> {
            let n = ((to - from) * 10.0).round() as usize;
            (0..n).map(|i| (from + i as f64 / 10.0, score(i))).collect()
        }

        #[test]
        fn starts_only_after_on_time() {
            let t0 = Instant::now();
            let mut h = hyst();
            // Above for 0.5 s, then below: no motion.
            let mut scores = every_100ms(0.0, 0.5, |_| 2.0);
            scores.extend(every_100ms(0.5, 2.0, |_| 0.5));
            assert!(feed(&mut h, t0, &scores).is_empty());
            // Above from 2.0 s: starts at 2.6 s, reporting 2.0 s as onset.
            let tr = feed(&mut h, t0, &every_100ms(2.0, 3.0, |_| 2.0));
            assert_eq!(tr.len(), 1);
            assert!((tr[0].0 - 2.6).abs() < 1e-9);
            assert_eq!(tr[0].1, Transition::Started { since: at(t0, 2.0) });
            assert!(h.in_motion());
        }

        #[test]
        fn hovering_at_threshold_does_not_flicker() {
            let t0 = Instant::now();
            let mut h = hyst();
            // Alternating just above / just below the threshold: never stays
            // above for 600 ms, so never starts.
            let hover = every_100ms(0.0, 60.0, |i| if i % 3 == 2 { 0.95 } else { 1.05 });
            assert!(feed(&mut h, t0, &hover).is_empty());

            // Enter motion, then hover between the release level and the
            // threshold for a minute: stays in motion, no end/start pairs.
            let tr = feed(&mut h, t0, &every_100ms(60.0, 61.0, |_| 3.0));
            assert_eq!(tr.len(), 1);
            let hover = every_100ms(61.0, 121.0, |i| if i % 2 == 0 { 0.75 } else { 1.1 });
            assert!(feed(&mut h, t0, &hover).is_empty());
            assert!(h.in_motion());

            // Dips below the release level shorter than `off` don't end it...
            let mut dips = every_100ms(121.0, 123.0, |_| 0.5);
            dips.extend(every_100ms(123.0, 123.5, |_| 0.8));
            assert!(feed(&mut h, t0, &dips).is_empty());
            // ...staying below for 3 s does, with the end at the start of the quiet.
            let tr = feed(&mut h, t0, &every_100ms(123.5, 130.0, |_| 0.5));
            assert_eq!(tr.len(), 1);
            assert!((tr[0].0 - 126.5).abs() < 1e-9);
            assert!(matches!(
                tr[0].1,
                Transition::Ended { started, ended, peak }
                    if started == at(t0, 60.0) && ended == at(t0, 123.5) && peak == 3.0
            ));
        }

        #[test]
        fn detector_hysteresis_beats_no_hysteresis_on_hovering_noise() {
            // k = 0 puts the threshold at the mean idle score, so the score of
            // plain noise hovers around it all the time.
            let t0 = Instant::now();
            let values = synthetic(
                SyntheticConfig {
                    duration: Some(Duration::from_secs(300)),
                    ..SyntheticConfig::default()
                },
                t0,
            );
            let count = |cfg: DetectorConfig| {
                let mut det = Detector::new(cfg, HashMap::new());
                starts(&run(&mut det, t0, &values)).len()
            };
            let base = DetectorConfig {
                sensitivity: 0.0,
                min_threshold: 0.0,
                min_threshold_ratio: 0.0,
                ..DetectorConfig::default()
            };
            let raw = count(DetectorConfig {
                on: Duration::ZERO,
                off: Duration::ZERO,
                off_ratio: 1.0,
                ..base.clone()
            });
            let debounced = count(base);
            assert!(raw >= 30, "raw transitions {raw}");
            assert!(debounced * 5 <= raw, "debounced {debounced} vs raw {raw}");
        }
    }
}
