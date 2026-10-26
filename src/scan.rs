//! `wisense list-aps`: access points the card can see, via `iw`.
//!
//! Not used for sensing: an active scan needs root, takes seconds and
//! disrupts the link. It only helps to pick a channel or check the signal of
//! your own access point before placing sensors.

use std::process::Command;

use anyhow::{bail, Context, Result};

#[derive(Debug, Clone, PartialEq)]
pub struct AccessPoint {
    pub bssid: String,
    pub ssid: String,
    pub freq_mhz: Option<u32>,
    pub signal_dbm: Option<f32>,
    pub associated: bool,
}

/// Parse `iw dev <if> scan` / `scan dump` output, strongest first.
pub fn parse_iw_scan(text: &str) -> Vec<AccessPoint> {
    let mut aps: Vec<AccessPoint> = Vec::new();
    for raw in text.lines() {
        if let Some(rest) = raw.strip_prefix("BSS ") {
            let bssid: String = rest
                .chars()
                .take_while(|c| c.is_ascii_hexdigit() || *c == ':')
                .collect();
            aps.push(AccessPoint {
                bssid,
                ssid: String::new(),
                freq_mhz: None,
                signal_dbm: None,
                associated: rest.contains("associated"),
            });
            continue;
        }
        let Some(ap) = aps.last_mut() else { continue };
        let line = raw.trim();
        if let Some(v) = line.strip_prefix("freq:") {
            ap.freq_mhz = v.trim().parse::<f32>().ok().map(|f| f.round() as u32);
        } else if let Some(v) = line.strip_prefix("signal:") {
            ap.signal_dbm = v.split_whitespace().next().and_then(|s| s.parse().ok());
        } else if let Some(v) = line.strip_prefix("SSID:") {
            if ap.ssid.is_empty() {
                ap.ssid = v.trim().to_string();
            }
        }
    }
    aps.sort_by(|a, b| {
        b.signal_dbm
            .unwrap_or(f32::MIN)
            .total_cmp(&a.signal_dbm.unwrap_or(f32::MIN))
    });
    aps
}

/// Scan with `iw`. Without root an active scan is refused, so fall back to
/// the kernel's cached results (`scan dump`) and say so.
pub fn scan(iface: &str) -> Result<Vec<AccessPoint>> {
    let fresh = Command::new("iw")
        .args(["dev", iface, "scan"])
        .output()
        .context("running `iw` (install it with: sudo apt install iw)")?;
    if fresh.status.success() {
        return Ok(parse_iw_scan(&String::from_utf8_lossy(&fresh.stdout)));
    }
    let err = String::from_utf8_lossy(&fresh.stderr);
    if !err.contains("Operation not permitted") {
        bail!("iw dev {iface} scan failed: {}", err.trim());
    }
    tracing::warn!("a fresh scan needs root (sudo); showing the kernel's cached scan results");
    let cached = Command::new("iw")
        .args(["dev", iface, "scan", "dump"])
        .output()
        .context("running `iw`")?;
    if !cached.status.success() {
        bail!(
            "iw dev {iface} scan dump failed: {}",
            String::from_utf8_lossy(&cached.stderr).trim()
        );
    }
    Ok(parse_iw_scan(&String::from_utf8_lossy(&cached.stdout)))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SCAN: &str = "\
BSS 11:22:33:44:55:66(on wlp2s0)
\tlast seen: 120 ms ago
\tfreq: 2412.0
\tsignal: -71.00 dBm
\tSSID: Neighbour
BSS f4:fc:49:04:b2:b9(on wlp2s0) -- associated
\tTSF: 1234 usec
\tfreq: 5320
\tbeacon interval: 100 TUs
\tsignal: -56.00 dBm
\tSSID: Home_5G
\tRSN:\t * Version: 1
BSS aa:bb:cc:dd:ee:ff(on wlp2s0)
\tfreq: 5180
\tSSID: \n";

    #[test]
    fn parses_and_sorts_by_signal() {
        let aps = parse_iw_scan(SCAN);
        assert_eq!(aps.len(), 3);
        assert_eq!(
            aps[0],
            AccessPoint {
                bssid: "f4:fc:49:04:b2:b9".into(),
                ssid: "Home_5G".into(),
                freq_mhz: Some(5320),
                signal_dbm: Some(-56.0),
                associated: true,
            }
        );
        assert_eq!(aps[1].ssid, "Neighbour");
        assert_eq!(aps[1].freq_mhz, Some(2412));
        // Hidden SSID, no signal line: sorted last.
        assert_eq!(aps[2].ssid, "");
        assert_eq!(aps[2].signal_dbm, None);
        assert!(!aps[2].associated);
    }

    #[test]
    fn empty_output() {
        assert!(parse_iw_scan("").is_empty());
    }
}
