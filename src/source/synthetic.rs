//! Synthetic RSSI generator for `wisense demo` and the tests.
//!
//! Models what a real card shows: quantized Gaussian noise around a base level,
//! a slow sinusoidal drift, values that only refresh at the card's update rate
//! (and are held in between), and motion bursts. A burst adds a temporally
//! correlated (Ornstein-Uhlenbeck) fluctuation, which is closer to a body
//! moving through the radio paths than white noise.

use std::f64::consts::TAU;
use std::time::{Duration, Instant};

use async_trait::async_trait;

use super::SampleSource;
use crate::sample::Sample;

/// Correlation time of the motion fluctuation.
const MOTION_TAU_S: f64 = 0.35;
/// Bursts fade in and out over this long.
const BURST_RAMP_S: f64 = 0.3;

/// A period of simulated motion.
#[derive(Debug, Clone, PartialEq)]
pub struct Burst {
    /// Offset from the start of the stream.
    pub start: Duration,
    pub duration: Duration,
    /// Standard deviation of the added fluctuation, in dB.
    pub std_db: f32,
}

impl Burst {
    /// 0 outside the burst, 1 inside, linear ramps at the edges.
    fn envelope(&self, t_s: f64) -> f64 {
        let start = self.start.as_secs_f64();
        let end = start + self.duration.as_secs_f64();
        if t_s < start || t_s > end {
            return 0.0;
        }
        ((t_s - start).min(end - t_s) / BURST_RAMP_S).min(1.0)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct SyntheticConfig {
    pub link_id: String,
    /// How often a sample is emitted (like the procfs poll rate).
    pub sample_hz: f32,
    /// How often the value actually changes (like received frames). Between
    /// updates the previous value is repeated.
    pub update_hz: f32,
    pub base_dbm: f32,
    pub noise_std_db: f32,
    /// Quantization step; real drivers report whole dBm.
    pub quantum_db: f32,
    /// Amplitude of the slow drift.
    pub drift_db: f32,
    pub drift_period: Duration,
    pub bursts: Vec<Burst>,
    /// End of stream; `None` runs forever.
    pub duration: Option<Duration>,
    pub seed: u64,
}

impl Default for SyntheticConfig {
    fn default() -> Self {
        Self {
            link_id: "synthetic0".to_string(),
            sample_hz: 20.0,
            update_hz: 8.0,
            base_dbm: -55.0,
            noise_std_db: 0.6,
            quantum_db: 1.0,
            drift_db: 1.5,
            drift_period: Duration::from_secs(180),
            bursts: Vec::new(),
            duration: None,
            seed: 1,
        }
    }
}

/// A burst schedule for demos: the first burst after `first_after`, then
/// bursts of 2-8 s separated by quiet gaps of 10-20 s, until `total`.
pub fn random_bursts(seed: u64, first_after: Duration, total: Duration) -> Vec<Burst> {
    let mut rng = Rng::new(seed ^ 0x6275_7273_7473);
    let mut bursts = Vec::new();
    let mut t = first_after.as_secs_f64() + rng.uniform() * 5.0;
    loop {
        let duration = 2.0 + rng.uniform() * 6.0;
        if t + duration > total.as_secs_f64() {
            return bursts;
        }
        bursts.push(Burst {
            start: Duration::from_secs_f64(t),
            duration: Duration::from_secs_f64(duration),
            std_db: (2.5 + rng.uniform() * 2.0) as f32,
        });
        t += duration + 10.0 + rng.uniform() * 10.0;
    }
}

/// Deterministic sample generator (no pacing). The source wraps it.
#[derive(Debug, Clone)]
pub struct SyntheticSignal {
    cfg: SyntheticConfig,
    rng: Rng,
    t0: Instant,
    ts0_unix_ms: u64,
    n: u64,
    next_update_s: f64,
    held_dbm: f32,
    motion: f64,
}

impl SyntheticSignal {
    pub fn new(cfg: SyntheticConfig, t0: Instant, ts0_unix_ms: u64) -> Self {
        let rng = Rng::new(cfg.seed);
        let held_dbm = cfg.base_dbm;
        Self {
            cfg,
            rng,
            t0,
            ts0_unix_ms,
            n: 0,
            next_update_s: 0.0,
            held_dbm,
            motion: 0.0,
        }
    }

    /// Offset of the next sample from the start of the stream.
    pub fn elapsed(&self) -> Duration {
        Duration::from_secs_f64(self.n as f64 / f64::from(self.cfg.sample_hz))
    }

    pub fn next_sample(&mut self) -> Option<Sample> {
        let offset = self.elapsed();
        if self.cfg.duration.is_some_and(|d| offset >= d) {
            return None;
        }
        let t_s = offset.as_secs_f64();
        while t_s >= self.next_update_s {
            self.update(self.next_update_s);
        }
        self.n += 1;
        let offset_ms = u64::try_from(offset.as_millis()).unwrap_or(u64::MAX);
        Some(Sample::rssi(
            self.cfg.link_id.clone(),
            self.t0 + offset,
            self.ts0_unix_ms.saturating_add(offset_ms),
            self.held_dbm,
        ))
    }

    /// Draw a new value, as if a frame had been received at `t_s`.
    fn update(&mut self, t_s: f64) {
        let period = 1.0 / f64::from(self.cfg.update_hz);
        // Frames don't arrive on a perfect clock; jitter avoids aliasing
        // patterns against the sample clock.
        self.next_update_s = t_s + period * (0.8 + 0.4 * self.rng.uniform());

        let a = (-period / MOTION_TAU_S).exp();
        self.motion = a * self.motion + (1.0 - a * a).sqrt() * self.rng.normal();
        let motion_std = self
            .cfg
            .bursts
            .iter()
            .map(|b| b.envelope(t_s) * f64::from(b.std_db))
            .fold(0.0, f64::max);

        let drift_period = self.cfg.drift_period.as_secs_f64().max(1.0);
        let value = f64::from(self.cfg.base_dbm)
            + f64::from(self.cfg.drift_db) * (TAU * t_s / drift_period).sin()
            + motion_std * self.motion
            + f64::from(self.cfg.noise_std_db) * self.rng.normal();
        let q = f64::from(self.cfg.quantum_db);
        let value = if q > 0.0 {
            (value / q).round() * q
        } else {
            value
        };
        self.held_dbm = value as f32;
    }
}

/// [`SyntheticSignal`] as a [`SampleSource`], optionally paced in real time.
pub struct SyntheticSource {
    signal: SyntheticSignal,
    link_id: String,
    /// `Some(speed)` sleeps so the stream plays at `speed` times real time;
    /// `None` produces samples as fast as they are consumed.
    speed: Option<f64>,
    started: tokio::time::Instant,
}

impl SyntheticSource {
    pub fn new(cfg: SyntheticConfig, speed: Option<f64>) -> Self {
        let link_id = cfg.link_id.clone();
        let signal = SyntheticSignal::new(cfg, Instant::now(), crate::sample::unix_ms_now());
        Self {
            signal,
            link_id,
            speed,
            started: tokio::time::Instant::now(),
        }
    }
}

#[async_trait]
impl SampleSource for SyntheticSource {
    fn link_id(&self) -> &str {
        &self.link_id
    }

    async fn next(&mut self) -> anyhow::Result<Option<Sample>> {
        if let Some(speed) = self.speed {
            let due = self.started + self.signal.elapsed().div_f64(speed);
            tokio::time::sleep_until(due).await;
        }
        Ok(self.signal.next_sample())
    }
}

/// Small deterministic PRNG (xorshift64*, seeded through splitmix64), so the
/// tests are reproducible without pulling in `rand`.
#[derive(Debug, Clone)]
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        let mut z = seed.wrapping_add(0x9E37_79B9_7F4A_7C15);
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^= z >> 31;
        Self(if z == 0 { 0x2545_F491_4F6C_DD1D } else { z })
    }

    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// Uniform in [0, 1).
    pub fn uniform(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }

    /// Standard normal (Box-Muller).
    pub fn normal(&mut self) -> f64 {
        let u1 = self.uniform().max(f64::MIN_POSITIVE);
        let u2 = self.uniform();
        (-2.0 * u1.ln()).sqrt() * (TAU * u2).cos()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn collect(cfg: SyntheticConfig) -> Vec<f32> {
        let mut sig = SyntheticSignal::new(cfg, Instant::now(), 0);
        std::iter::from_fn(|| sig.next_sample())
            .map(|s| s.rssi_dbm.unwrap_or(f32::NAN))
            .collect()
    }

    fn std(xs: &[f32]) -> f32 {
        let n = xs.len() as f32;
        let mean = xs.iter().sum::<f32>() / n;
        (xs.iter().map(|x| (x - mean).powi(2)).sum::<f32>() / n).sqrt()
    }

    #[test]
    fn quantized_noise_around_base() {
        let xs = collect(SyntheticConfig {
            duration: Some(Duration::from_secs(60)),
            drift_db: 0.0,
            ..SyntheticConfig::default()
        });
        assert_eq!(xs.len(), 1200);
        assert!(
            xs.iter().all(|x| x.fract() == 0.0),
            "values must be whole dB"
        );
        let mean = xs.iter().sum::<f32>() / xs.len() as f32;
        assert!((mean + 55.0).abs() < 0.3, "mean {mean}");
        let s = std(&xs);
        assert!(s > 0.3 && s < 1.2, "std {s}");
    }

    #[test]
    fn values_are_held_between_updates() {
        let xs = collect(SyntheticConfig {
            duration: Some(Duration::from_secs(30)),
            update_hz: 4.0,
            ..SyntheticConfig::default()
        });
        let changes = xs.windows(2).filter(|w| w[0] != w[1]).count();
        // At most one change per update (4 Hz * 30 s); fewer because a new
        // draw often quantizes to the same value.
        assert!(changes <= 130, "{changes} changes");
        assert!(changes > 20, "{changes} changes");
    }

    #[test]
    fn burst_increases_variance() {
        let xs = collect(SyntheticConfig {
            duration: Some(Duration::from_secs(30)),
            bursts: vec![Burst {
                start: Duration::from_secs(10),
                duration: Duration::from_secs(10),
                std_db: 3.0,
            }],
            ..SyntheticConfig::default()
        });
        let quiet = std(&xs[..180]);
        let busy = std(&xs[220..380]);
        assert!(busy > 2.0 * quiet, "quiet {quiet} busy {busy}");
    }

    #[test]
    fn deterministic_for_a_seed() {
        let cfg = SyntheticConfig {
            duration: Some(Duration::from_secs(5)),
            ..SyntheticConfig::default()
        };
        assert_eq!(collect(cfg.clone()), collect(cfg.clone()));
        let other = collect(SyntheticConfig {
            seed: 2,
            ..cfg.clone()
        });
        assert_ne!(collect(cfg), other);
    }

    #[test]
    fn random_bursts_fit_in_total() {
        let total = Duration::from_secs(120);
        let bursts = random_bursts(7, Duration::from_secs(25), total);
        assert!(!bursts.is_empty());
        for w in bursts.windows(2) {
            assert!(w[1].start >= w[0].start + w[0].duration + Duration::from_secs(10));
        }
        for b in &bursts {
            assert!(b.start >= Duration::from_secs(25));
            assert!(b.start + b.duration <= total);
        }
    }
}
