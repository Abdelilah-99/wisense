//! `/proc/net/arp`, MAC addresses and subnets, for presence detection.
//!
//! ```text
//! IP address       HW type     Flags       HW address            Mask     Device
//! 192.168.1.23     0x1         0x2         02:00:00:00:00:17     *        wlp2s0
//! ```
//!
//! Flags `0x2` (ATF_COM) marks a resolved entry. Failed or in-progress
//! lookups have `0x0` and an all-zero address.

use std::net::Ipv4Addr;

use super::ParseError;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArpEntry {
    pub ip: Ipv4Addr,
    pub mac: Mac,
    /// The neighbour answered (ATF_COM).
    pub complete: bool,
    pub device: String,
}

/// Parses the table, skipping lines that do not look like entries.
pub fn parse_arp_table(text: &str) -> Vec<ArpEntry> {
    text.lines()
        .skip(1)
        .filter_map(|line| {
            let fields: Vec<&str> = line.split_whitespace().collect();
            let [ip, _hw_type, flags, mac, _mask, device] = fields.as_slice() else {
                return None;
            };
            let flags = u32::from_str_radix(flags.trim_start_matches("0x"), 16).ok()?;
            Some(ArpEntry {
                ip: ip.parse().ok()?,
                mac: Mac::parse(mac).ok()?,
                complete: flags & 0x2 != 0,
                device: device.to_string(),
            })
        })
        .collect()
}

/// A MAC address, compared case-insensitively.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Mac([u8; 6]);

impl Mac {
    /// Accepts `aa:bb:cc:dd:ee:ff` or `aa-bb-cc-dd-ee-ff`, any case.
    pub fn parse(text: &str) -> Result<Mac, ParseError> {
        let text = text.trim();
        let parts: Vec<&str> = text.split([':', '-']).collect();
        if parts.len() != 6 {
            return Err(ParseError::UnexpectedValue(text.to_string()));
        }
        let mut bytes = [0u8; 6];
        for (byte, part) in bytes.iter_mut().zip(&parts) {
            if part.len() != 2 {
                return Err(ParseError::UnexpectedValue(text.to_string()));
            }
            *byte = u8::from_str_radix(part, 16)
                .map_err(|_| ParseError::UnexpectedValue(text.to_string()))?;
        }
        Ok(Mac(bytes))
    }

    pub fn is_zero(&self) -> bool {
        self.0 == [0; 6]
    }

    /// Locally administered: the address a phone makes up when MAC
    /// randomization ("private address") is on. Such an address may change,
    /// so it cannot reliably identify the phone.
    pub fn is_locally_administered(&self) -> bool {
        self.0[0] & 0x02 != 0
    }
}

impl std::fmt::Display for Mac {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let [a, b, c, d, e, g] = self.0;
        write!(f, "{a:02x}:{b:02x}:{c:02x}:{d:02x}:{e:02x}:{g:02x}")
    }
}

/// The host addresses of an IPv4 subnet such as `192.168.1.0/24`, for the
/// discovery sweep. Limited to /22 (1022 hosts) so a typo cannot start a
/// sweep of millions of addresses.
pub fn subnet_hosts(cidr: &str) -> Result<Vec<Ipv4Addr>, ParseError> {
    let bad = || ParseError::UnexpectedValue(cidr.to_string());
    let (addr, len) = cidr.trim().split_once('/').ok_or_else(bad)?;
    let addr: Ipv4Addr = addr.parse().map_err(|_| bad())?;
    let len: u32 = len.parse().map_err(|_| bad())?;
    if !(22..=30).contains(&len) {
        return Err(ParseError::UnexpectedValue(format!(
            "{cidr}: use a prefix between /22 and /30"
        )));
    }
    let mask = u32::MAX << (32 - len);
    let network = u32::from(addr) & mask;
    let broadcast = network | !mask;
    Ok((network + 1..broadcast).map(Ipv4Addr::from).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    const TABLE: &str = "\
IP address       HW type     Flags       HW address            Mask     Device
192.168.1.1      0x1         0x2         02:00:00:00:00:01     *        wlp2s0
192.168.1.23     0x1         0x2         02:00:00:00:00:17     *        wlp2s0
192.168.1.40     0x1         0x0         00:00:00:00:00:00     *        wlp2s0
172.18.0.2       0x1         0x2         02:00:00:00:00:a2     *        br-3f1c
garbage line
";

    #[test]
    fn parses_entries_and_flags() {
        let entries = parse_arp_table(TABLE);
        assert_eq!(entries.len(), 4);
        assert_eq!(entries[1].ip, Ipv4Addr::new(192, 168, 1, 23));
        assert_eq!(entries[1].mac, Mac::parse("02:00:00:00:00:17").unwrap());
        assert!(entries[1].complete);
        assert!(!entries[2].complete);
        assert!(entries[2].mac.is_zero());
        assert_eq!(entries[3].device, "br-3f1c");
        assert!(parse_arp_table("").is_empty());
    }

    #[test]
    fn macs_compare_case_insensitively() {
        assert_eq!(
            Mac::parse("02:00:00:00:00:AB").unwrap(),
            Mac::parse("02-00-00-00-00-ab").unwrap()
        );
        assert_eq!(
            Mac::parse(" 02:00:00:00:00:ab ").unwrap().to_string(),
            "02:00:00:00:00:ab"
        );
        for bad in [
            "",
            "02:00:00:00:00",
            "02:00:00:00:00:0g",
            "2:0:0:0:0:1",
            "02:00:00:00:00:00:01",
        ] {
            assert!(Mac::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn detects_randomized_macs() {
        assert!(Mac::parse("02:00:00:00:00:01")
            .unwrap()
            .is_locally_administered());
        // The RFC 7042 documentation range is globally administered.
        assert!(!Mac::parse("00:00:5e:00:53:01")
            .unwrap()
            .is_locally_administered());
    }

    #[test]
    fn subnet_hosts_excludes_network_and_broadcast() {
        let hosts = subnet_hosts("192.168.1.77/24").unwrap();
        assert_eq!(hosts.len(), 254);
        assert_eq!(hosts[0], Ipv4Addr::new(192, 168, 1, 1));
        assert_eq!(hosts[253], Ipv4Addr::new(192, 168, 1, 254));
        assert_eq!(subnet_hosts("10.0.0.0/22").unwrap().len(), 1022);
        assert_eq!(subnet_hosts("10.0.0.4/30").unwrap().len(), 2);
        for bad in [
            "10.0.0.0/8",
            "10.0.0.0/31",
            "10.0.0.0",
            "nope/24",
            "10.0.0.0/x",
        ] {
            assert!(subnet_hosts(bad).is_err(), "{bad}");
        }
    }
}
