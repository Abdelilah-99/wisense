//! Time as data. The core never reads a clock: every input carries a [`Ts`],
//! and the shell (or a test, or the simulator) decides what "now" is.

use std::fmt;
use std::ops::Add;
use std::time::Duration;

use serde::{Deserialize, Deserializer, Serialize};

/// A point in time, in milliseconds since an arbitrary origin (process start
/// at runtime, `0` in tests and scenarios). Monotonic by construction: only
/// differences between two `Ts` mean anything.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
pub struct Ts(u64);

impl Ts {
    pub const ZERO: Ts = Ts(0);

    pub const fn from_millis(ms: u64) -> Self {
        Ts(ms)
    }

    pub const fn from_secs(s: u64) -> Self {
        Ts(s * 1000)
    }

    pub const fn as_millis(self) -> u64 {
        self.0
    }

    /// Time elapsed since `earlier`; zero if `earlier` is in the future.
    pub fn since(self, earlier: Ts) -> Duration {
        Duration::from_millis(self.0.saturating_sub(earlier.0))
    }
}

impl Add<Duration> for Ts {
    type Output = Ts;

    fn add(self, d: Duration) -> Ts {
        let ms = u64::try_from(d.as_millis()).unwrap_or(u64::MAX);
        Ts(self.0.saturating_add(ms))
    }
}

impl fmt::Display for Ts {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "t+{}", format_duration(Duration::from_millis(self.0)))
    }
}

/// Parses durations written as in the config and scenarios: `"500ms"`,
/// `"30s"`, `"2m"`, `"1h"`, `"7d"` or a combination such as `"1m30s"`.
pub fn parse_duration(text: &str) -> Result<Duration, String> {
    let text = text.trim();
    if text.is_empty() {
        return Err("empty duration".into());
    }
    let mut total_ms: u64 = 0;
    let mut rest = text;
    while !rest.is_empty() {
        let digits = rest
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(rest.len());
        if digits == 0 {
            return Err(format!("invalid duration {text:?}: expected a number"));
        }
        let value: u64 = rest[..digits]
            .parse()
            .map_err(|_| format!("invalid duration {text:?}: number too large"))?;
        rest = &rest[digits..];
        let unit_len = rest
            .find(|c: char| c.is_ascii_digit())
            .unwrap_or(rest.len());
        let unit_ms = match &rest[..unit_len] {
            "ms" => 1,
            "s" => 1_000,
            "m" | "min" => 60_000,
            "h" => 3_600_000,
            "d" => 86_400_000,
            "" => {
                return Err(format!(
                    "invalid duration {text:?}: missing unit (ms, s, m, h, d)"
                ))
            }
            other => return Err(format!("invalid duration {text:?}: unknown unit {other:?}")),
        };
        rest = &rest[unit_len..];
        total_ms = value
            .checked_mul(unit_ms)
            .and_then(|v| total_ms.checked_add(v))
            .ok_or_else(|| format!("invalid duration {text:?}: too large"))?;
    }
    Ok(Duration::from_millis(total_ms))
}

/// The inverse of [`parse_duration`], for messages: `90s` → `"1m30s"`.
pub fn format_duration(d: Duration) -> String {
    let ms = u64::try_from(d.as_millis()).unwrap_or(u64::MAX);
    if ms == 0 {
        return "0s".into();
    }
    let mut out = String::new();
    let mut rest = ms;
    for (unit, size) in [
        ("d", 86_400_000),
        ("h", 3_600_000),
        ("m", 60_000),
        ("s", 1_000),
    ] {
        if rest >= size {
            out.push_str(&format!("{}{unit}", rest / size));
            rest %= size;
        }
    }
    if rest > 0 {
        out.push_str(&format!("{rest}ms"));
    }
    out
}

/// Serde helpers so config structs can hold `Duration` written as text.
pub mod serde_duration {
    use super::*;

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Duration, D::Error> {
        let text = String::deserialize(d)?;
        parse_duration(&text).map_err(serde::de::Error::custom)
    }

    pub fn serialize<S: serde::Serializer>(d: &Duration, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&format_duration(*d))
    }
}

impl<'de> Deserialize<'de> for Ts {
    /// A scenario timestamp: a duration since the start, e.g. `"1m30s"`.
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Ts, D::Error> {
        serde_duration::deserialize(d).map(|dur| Ts::ZERO + dur)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_units_and_combinations() {
        assert_eq!(parse_duration("500ms"), Ok(Duration::from_millis(500)));
        assert_eq!(parse_duration("30s"), Ok(Duration::from_secs(30)));
        assert_eq!(parse_duration("2m"), Ok(Duration::from_secs(120)));
        assert_eq!(parse_duration("1h"), Ok(Duration::from_secs(3600)));
        assert_eq!(parse_duration("7d"), Ok(Duration::from_secs(7 * 86_400)));
        assert_eq!(parse_duration("1m30s"), Ok(Duration::from_secs(90)));
        assert_eq!(parse_duration(" 0s "), Ok(Duration::ZERO));
    }

    #[test]
    fn rejects_garbage() {
        for bad in ["", "s", "30", "30x", "-5s", "1.5s", "99999999999999999999s"] {
            assert!(parse_duration(bad).is_err(), "{bad:?} should be rejected");
        }
    }

    #[test]
    fn formats_round_trip() {
        for text in ["0s", "250ms", "30s", "1m30s", "2h", "1d1h1m1s1ms"] {
            let d = parse_duration(text).unwrap();
            assert_eq!(format_duration(d), text);
        }
    }

    #[test]
    fn timestamps_add_and_subtract() {
        let t = Ts::from_secs(10);
        assert_eq!(t + Duration::from_millis(1500), Ts::from_millis(11_500));
        assert_eq!(Ts::from_secs(12).since(t), Duration::from_secs(2));
        assert_eq!(t.since(Ts::from_secs(12)), Duration::ZERO);
        assert_eq!(t.to_string(), "t+10s");
    }
}
