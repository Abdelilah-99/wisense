//! `iw`: the access points the card can see (`wisense list-aps`), and the
//! one it is associated with (`iw dev <if> link`).
//!
//! A scan is not used for sensing: an active scan needs root, takes seconds
//! and disrupts the link. It only helps to pick a channel or check the
//! signal of your own access point before placing sensors.

use std::process::Command;

use anyhow::{bail, Context, Result};
use thiserror::Error;

use crate::discover::MacAddr;

#[derive(Debug, Clone, PartialEq)]
pub struct AccessPoint {
    pub bssid: String,
    pub ssid: String,
    pub freq_mhz: Option<u32>,
    pub signal_dbm: Option<f32>,
    pub associated: bool,
}

/// The current association, from `iw dev <if> link`.
#[derive(Debug, Clone, PartialEq)]
pub struct LinkInfo {
    /// The access point's radio address. Every frame the laptop receives in
    /// client mode comes from it.
    pub bssid: MacAddr,
    pub ssid: Option<String>,
    pub freq_mhz: Option<u32>,
}

#[derive(Debug, Error, PartialEq)]
pub enum IwLinkError {
    #[error("unexpected `iw link` output {0:?}")]
    Unexpected(String),
    #[error("bad BSSID {0:?} in `iw link` output")]
    BadBssid(String),
}

/// Parse `iw dev <if> link`: `Ok(None)` for "Not connected.", otherwise
/// the BSSID from `Connected to f4:fc:49:04:b2:b9 (on wlp2s0)` plus the SSID
/// and frequency if present. Never panics, whatever the input.
pub fn parse_iw_link(text: &str) -> Result<Option<LinkInfo>, IwLinkError> {
    let first = text
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or_default();
    if first.starts_with("Not connected") {
        return Ok(None);
    }
    let rest = first
        .strip_prefix("Connected to ")
        .ok_or_else(|| IwLinkError::Unexpected(first.to_string()))?;
    let bssid_text = rest.split_whitespace().next().unwrap_or_default();
    let bssid =
        MacAddr::parse(bssid_text).ok_or_else(|| IwLinkError::BadBssid(bssid_text.to_string()))?;
    let mut info = LinkInfo {
        bssid,
        ssid: None,
        freq_mhz: None,
    };
    for line in text.lines().map(str::trim) {
        if let Some(v) = line.strip_prefix("SSID:") {
            info.ssid.get_or_insert_with(|| v.trim().to_string());
        } else if let Some(v) = line.strip_prefix("freq:") {
            info.freq_mhz = v
                .trim()
                .parse::<f32>()
                .ok()
                .filter(|f| f.is_finite() && *f > 0.0)
                .map(|f| f.round() as u32);
        }
    }
    Ok(Some(info))
}

/// Run `iw dev <iface> link` (no root needed).
pub fn associated_ap(iface: &str) -> Result<Option<LinkInfo>> {
    let out = Command::new("iw")
        .args(["dev", iface, "link"])
        .output()
        .context("running `iw` (install it with: sudo apt install iw)")?;
    if !out.status.success() {
        bail!(
            "iw dev {iface} link failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    parse_iw_link(&String::from_utf8_lossy(&out.stdout))
        .with_context(|| format!("reading iw dev {iface} link"))
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

    /// `iw dev wlp2s0 link` on a real laptop (SSID changed).
    const LINK: &str = "\
Connected to f4:fc:49:04:b2:b9 (on wlp2s0)
\tSSID: Home_5G
\tfreq: 5320.0
\tRX: 1915702710 bytes (628253 packets)
\tTX: 85713868 bytes (345474 packets)
\tsignal: -56 dBm
\trx bitrate: 526.6 MBit/s VHT-MCS 6 80MHz VHT-NSS 2
\tbss flags: short-preamble short-slot-time
\tdtim period: 1
\tbeacon int: 100
";

    #[test]
    fn parses_iw_link() {
        assert_eq!(
            parse_iw_link(LINK),
            Ok(Some(LinkInfo {
                bssid: MacAddr::parse("f4:fc:49:04:b2:b9").unwrap(),
                ssid: Some("Home_5G".into()),
                freq_mhz: Some(5320),
            }))
        );
        // Only the first line is required.
        let bare = parse_iw_link("Connected to F4:FC:49:04:B2:B9 (on wlan0)\n").unwrap();
        assert_eq!(
            bare.map(|l| l.bssid.to_string()),
            Some("f4:fc:49:04:b2:b9".into())
        );
    }

    #[test]
    fn iw_link_not_connected() {
        assert_eq!(parse_iw_link("Not connected.\n"), Ok(None));
        assert_eq!(parse_iw_link("\n  Not connected.\n"), Ok(None));
    }

    #[test]
    fn iw_link_garbage() {
        assert_eq!(
            parse_iw_link(""),
            Err(IwLinkError::Unexpected(String::new()))
        );
        assert_eq!(
            parse_iw_link("command failed: No such device (-19)"),
            Err(IwLinkError::Unexpected(
                "command failed: No such device (-19)".into()
            ))
        );
        assert_eq!(
            parse_iw_link("Connected to f4:fc:49 (on wlp2s0)"),
            Err(IwLinkError::BadBssid("f4:fc:49".into()))
        );
        // The line is trimmed first, so a bare prefix is unexpected output.
        assert_eq!(
            parse_iw_link("Connected to "),
            Err(IwLinkError::Unexpected("Connected to".into()))
        );
        assert_eq!(
            parse_iw_link("Connected to  (on wlp2s0)"),
            Err(IwLinkError::BadBssid("(on".into()))
        );
        // An odd frequency is ignored, not fatal.
        let odd = parse_iw_link("Connected to f4:fc:49:04:b2:b9 (on x)\n\tfreq: fast\n").unwrap();
        assert_eq!(odd.and_then(|l| l.freq_mhz), None);
        let mut rng = crate::source::synthetic::Rng::new(13);
        for _ in 0..2000 {
            let mut bytes = LINK.as_bytes().to_vec();
            let i = (rng.next_u64() as usize) % bytes.len();
            bytes[i] = (rng.next_u64() & 0xff) as u8;
            bytes.truncate((rng.next_u64() as usize) % (bytes.len() + 1));
            let _ = parse_iw_link(&String::from_utf8_lossy(&bytes));
        }
    }
}
