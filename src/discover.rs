//! `wisense discover`: the devices on the WiFi subnet.
//!
//! The parsing here is pure, so it is tested with fixed tables:
//!
//! - `/proc/net/arp`: the kernel's neighbour table (IP address -> MAC);
//! - `/proc/net/route`: the WiFi subnet is the interface's directly connected
//!   route (destination + mask), parsed with [`traffic::parse_routes`];
//! - MAC addresses, including whether one is a randomized "private" address.

use std::fmt;
use std::net::Ipv4Addr;

use thiserror::Error;

use crate::config;
use crate::traffic::{self, Route};

pub const ARP_PATH: &str = "/proc/net/arp";

/// `ATF_COM` from `<linux/if_arp.h>`: the entry is complete (resolved).
const ATF_COM: u16 = 0x02;

#[derive(Debug, Error, PartialEq)]
pub enum ArpError {
    #[error("malformed /proc/net/arp line {line:?}: {reason}")]
    Malformed { line: String, reason: String },
}

#[derive(Debug, Error, PartialEq)]
pub enum SubnetError {
    #[error("netmask {0} is not a contiguous prefix")]
    BadMask(Ipv4Addr),
    #[error(
        "{subnet} has {addresses} addresses; wisense probes at most a /{max} unless \
         --allow-large-subnet is given"
    )]
    TooLarge {
        subnet: Subnet,
        addresses: u64,
        max: u8,
    },
}

/// A 48-bit hardware address.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct MacAddr(pub [u8; 6]);

impl MacAddr {
    /// Parse `aa:bb:cc:dd:ee:ff` (either case). `None` for anything else.
    pub fn parse(text: &str) -> Option<Self> {
        let mut bytes = [0u8; 6];
        let mut parts = text.split(':');
        for byte in &mut bytes {
            let part = parts.next()?;
            if part.len() != 2 {
                return None;
            }
            *byte = u8::from_str_radix(part, 16).ok()?;
        }
        parts.next().is_none().then_some(Self(bytes))
    }

    pub fn is_zero(&self) -> bool {
        self.0 == [0; 6]
    }
}

impl fmt::Display for MacAddr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let [a, b, c, d, e, g] = self.0;
        write!(f, "{a:02x}:{b:02x}:{c:02x}:{d:02x}:{e:02x}:{g:02x}")
    }
}

/// True for a locally administered address (bit 0x02 of the first byte).
/// Phones and laptops use such random "private" addresses per network, and
/// may change them, so they are poor long-term identifiers.
pub fn is_randomized_mac(mac: &MacAddr) -> bool {
    mac.0[0] & 0x02 != 0
}

/// One data line of `/proc/net/arp`.
#[derive(Debug, Clone, PartialEq)]
pub struct ArpEntry {
    pub ip: Ipv4Addr,
    pub flags: u16,
    /// `None` for an incomplete entry: the kernel asked but got no answer
    /// (flags `0x0`, MAC `00:00:00:00:00:00`).
    pub mac: Option<MacAddr>,
    pub device: String,
}

/// Parse one data line, e.g.
/// `192.168.1.1  0x1  0x2  f4:fc:49:04:b2:b9  *  wlp2s0`.
/// Columns: IP address, HW type, Flags, HW address, Mask, Device.
pub fn parse_arp_line(line: &str) -> Result<ArpEntry, ArpError> {
    let malformed = |reason: String| ArpError::Malformed {
        line: line.to_string(),
        reason,
    };
    let fields: Vec<&str> = line.split_whitespace().collect();
    let [ip, hw_type, flags, mac, _mask, device] = fields.as_slice() else {
        return Err(malformed(format!(
            "expected 6 columns, got {}",
            fields.len()
        )));
    };
    let hex = |name: &str, s: &str| {
        s.strip_prefix("0x")
            .and_then(|h| u16::from_str_radix(h, 16).ok())
            .ok_or_else(|| malformed(format!("bad {name} {s:?}")))
    };
    let ip = ip
        .parse::<Ipv4Addr>()
        .map_err(|_| malformed(format!("bad IP address {ip:?}")))?;
    hex("HW type", hw_type)?;
    let flags = hex("flags", flags)?;
    let mac = MacAddr::parse(mac).ok_or_else(|| malformed(format!("bad HW address {mac:?}")))?;
    let complete = flags & ATF_COM != 0 && !mac.is_zero();
    Ok(ArpEntry {
        ip,
        flags,
        mac: complete.then_some(mac),
        device: (*device).to_string(),
    })
}

/// Every data line of `/proc/net/arp`, skipping the header.
pub fn parse_arp(text: &str) -> Vec<Result<ArpEntry, ArpError>> {
    text.lines()
        .skip(1)
        .filter(|l| !l.trim().is_empty())
        .map(parse_arp_line)
        .collect()
}

/// An IPv4 subnet, e.g. `192.168.1.0/24`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Subnet {
    /// The network address (host bits zero).
    pub network: Ipv4Addr,
    pub prefix: u8,
}

impl Subnet {
    /// From an address and a netmask such as `255.255.255.0`. The host bits
    /// of `addr` are cleared.
    pub fn from_mask(addr: Ipv4Addr, mask: Ipv4Addr) -> Result<Self, SubnetError> {
        let bits = u32::from(mask);
        if bits.leading_ones() != bits.count_ones() {
            return Err(SubnetError::BadMask(mask));
        }
        Ok(Self {
            network: Ipv4Addr::from(u32::from(addr) & bits),
            prefix: u8::try_from(bits.count_ones()).unwrap_or(32),
        })
    }

    fn mask(&self) -> u32 {
        u32::MAX
            .checked_shl(32 - u32::from(self.prefix))
            .unwrap_or(0)
    }

    /// Number of addresses, network and broadcast included.
    pub fn addresses(&self) -> u64 {
        1u64 << (32 - u32::from(self.prefix))
    }

    pub fn contains(&self, ip: Ipv4Addr) -> bool {
        u32::from(ip) & self.mask() == u32::from(self.network)
    }

    /// The usable host addresses: all but the network and broadcast
    /// addresses, except on /31 and /32 where every address is a host.
    pub fn hosts(&self) -> impl Iterator<Item = Ipv4Addr> {
        let first = u32::from(self.network);
        let last = first | !self.mask();
        let (first, last) = if self.prefix >= 31 {
            (first, last)
        } else {
            (first + 1, last - 1)
        };
        (first..=last).map(Ipv4Addr::from)
    }

    /// Refuse subnets larger than a /`max` unless `allow_large`, so a
    /// misdetected /8 doesn't turn into millions of pings.
    pub fn check_size(&self, max: u8, allow_large: bool) -> Result<(), SubnetError> {
        if self.prefix < max && !allow_large {
            return Err(SubnetError::TooLarge {
                subnet: *self,
                addresses: self.addresses(),
                max,
            });
        }
        Ok(())
    }
}

impl fmt::Display for Subnet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.network, self.prefix)
    }
}

/// The subnet directly reachable on `iface`, from a `/proc/net/route` table:
/// an up route on the interface with a destination and no gateway. If there
/// are several (e.g. a link-local 169.254.0.0/16 next to the LAN), the one
/// holding the interface's default gateway wins, then the lowest metric.
pub fn subnet_for(table: &str, iface: &str) -> Option<Subnet> {
    let routes: Vec<Route> = traffic::parse_routes(table)
        .into_iter()
        .filter(|r| r.iface == iface)
        .collect();
    let gateway = routes.iter().find(|r| r.is_default()).map(|r| r.gateway);
    let mut local: Vec<(Subnet, u32)> = routes
        .iter()
        .filter(|r| r.is_up() && !r.via_gateway() && !r.destination.is_unspecified())
        .filter_map(|r| Some((Subnet::from_mask(r.destination, r.mask).ok()?, r.metric)))
        .filter(|(s, _)| !s.network.is_link_local())
        .collect();
    local.sort_by_key(|(s, metric)| (!gateway.is_some_and(|g| s.contains(g)), *metric));
    local.first().map(|(s, _)| *s)
}

/// [`Subnet::check_size`] with the default limit.
pub fn check_subnet_size(subnet: &Subnet, allow_large: bool) -> Result<(), SubnetError> {
    subnet.check_size(config::DEFAULT_MAX_SUBNET_PREFIX, allow_large)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ARP: &str = "\
IP address       HW type     Flags       HW address            Mask     Device
192.168.1.1      0x1         0x2         f4:fc:49:04:b2:b9     *        wlp2s0
192.168.1.23     0x1         0x2         76:62:94:1a:2b:3c     *        wlp2s0
192.168.1.40     0x1         0x0         00:00:00:00:00:00     *        wlp2s0
192.168.1.7      0x1         0x6         38:91:48:AA:BB:CC     *        wlp2s0
172.17.0.2       0x1         0x2         02:42:ac:11:00:02     *        docker0
";

    fn mac(s: &str) -> MacAddr {
        MacAddr::parse(s).unwrap()
    }

    #[test]
    fn parses_a_real_looking_arp_table() {
        let entries: Vec<ArpEntry> = parse_arp(ARP).into_iter().map(Result::unwrap).collect();
        assert_eq!(entries.len(), 5);
        assert_eq!(
            entries[0],
            ArpEntry {
                ip: Ipv4Addr::new(192, 168, 1, 1),
                flags: 0x2,
                mac: Some(mac("f4:fc:49:04:b2:b9")),
                device: "wlp2s0".into(),
            }
        );
        // Permanent + complete (0x6) counts as complete; case doesn't matter.
        assert_eq!(entries[3].mac, Some(mac("38:91:48:aa:bb:cc")));
        // Other interfaces are kept; callers filter by device.
        assert_eq!(entries[4].device, "docker0");
    }

    #[test]
    fn incomplete_entries_have_no_mac() {
        let e = parse_arp_line("192.168.1.40 0x1 0x0 00:00:00:00:00:00 * wlp2s0").unwrap();
        assert_eq!(e.mac, None);
        // Either sign is enough: a zero MAC even with the complete flag...
        let e = parse_arp_line("192.168.1.41 0x1 0x2 00:00:00:00:00:00 * wlp2s0").unwrap();
        assert_eq!(e.mac, None);
        // ...or a MAC without the complete flag (a stale, unresolved entry).
        let e = parse_arp_line("192.168.1.42 0x1 0x0 38:91:48:aa:bb:cc * wlp2s0").unwrap();
        assert_eq!(e.mac, None);
    }

    #[test]
    fn header_and_empty_tables() {
        assert!(parse_arp("").is_empty());
        assert!(parse_arp(ARP.lines().next().unwrap()).is_empty());
        assert!(parse_arp(&format!("{}\n\n", ARP.lines().next().unwrap())).is_empty());
    }

    #[test]
    fn malformed_arp_lines() {
        for (line, reason) in [
            ("", "expected 6 columns, got 0"),
            (
                "192.168.1.1 0x1 0x2 f4:fc:49:04:b2:b9 *",
                "expected 6 columns, got 5",
            ),
            (
                "192.168.1.1 0x1 0x2 f4:fc:49:04:b2:b9 * wlp2s0 x",
                "expected 6 columns, got 7",
            ),
            (
                "192.168.1.300 0x1 0x2 f4:fc:49:04:b2:b9 * wlp2s0",
                "bad IP address \"192.168.1.300\"",
            ),
            (
                "192.168.1.1 1 0x2 f4:fc:49:04:b2:b9 * wlp2s0",
                "bad HW type \"1\"",
            ),
            (
                "192.168.1.1 0x1 0xZ f4:fc:49:04:b2:b9 * wlp2s0",
                "bad flags \"0xZ\"",
            ),
            (
                "192.168.1.1 0x1 0x2 f4:fc:49:04:b2 * wlp2s0",
                "bad HW address \"f4:fc:49:04:b2\"",
            ),
            (
                "192.168.1.1 0x1 0x2 f4-fc-49-04-b2-b9 * wlp2s0",
                "bad HW address \"f4-fc-49-04-b2-b9\"",
            ),
        ] {
            assert_eq!(
                parse_arp_line(line),
                Err(ArpError::Malformed {
                    line: line.into(),
                    reason: reason.into()
                }),
                "{line}"
            );
        }
        // One bad line doesn't hide the others.
        let results = parse_arp(&format!("{ARP}junk\n"));
        assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 5);
        assert!(results.last().is_some_and(Result::is_err));
    }

    #[test]
    fn arp_parser_never_panics_on_garbage() {
        let mut rng = crate::source::synthetic::Rng::new(5);
        let good = "192.168.1.1      0x1         0x2         f4:fc:49:04:b2:b9     *        wlp2s0";
        for _ in 0..2000 {
            let mut bytes = good.as_bytes().to_vec();
            let i = (rng.next_u64() as usize) % bytes.len();
            bytes[i] = (rng.next_u64() & 0xff) as u8;
            bytes.truncate((rng.next_u64() as usize) % (bytes.len() + 1));
            let _ = parse_arp_line(&String::from_utf8_lossy(&bytes));
        }
    }

    #[test]
    fn mac_parsing_and_display() {
        assert_eq!(mac("F4:FC:49:04:B2:B9").to_string(), "f4:fc:49:04:b2:b9");
        for bad in [
            "",
            "f4:fc:49:04:b2",
            "f4:fc:49:04:b2:b9:00",
            "f4:fc:49:04:b2:zz",
            "f4:fc:49:4:b2:b9",
            "f4:fc:49:004:b2:b9",
        ] {
            assert_eq!(MacAddr::parse(bad), None, "{bad}");
        }
        assert!(mac("00:00:00:00:00:00").is_zero());
    }

    #[test]
    fn randomized_vs_universal_macs() {
        // Locally administered (bit 0x02 of the first byte): private
        // addresses as phones use them.
        for m in [
            "76:62:94:1a:2b:3c",
            "CE:D0:20:11:22:33",
            "02:42:ac:11:00:02",
            "da:a1:19:00:00:01",
        ] {
            assert!(is_randomized_mac(&mac(m)), "{m}");
        }
        // Universally administered: assigned by the manufacturer.
        for m in [
            "38:91:48:aa:bb:cc",
            "00:42:38:01:02:03",
            "f4:fc:49:04:b2:b9",
            "a4:5e:60:00:00:01",
        ] {
            assert!(!is_randomized_mac(&mac(m)), "{m}");
        }
    }

    const ROUTES: &str =
        "Iface\tDestination\tGateway \tFlags\tRefCnt\tUse\tMetric\tMask\t\tMTU\tWindow\tIRTT\n\
        eth0\t00000000\t0100000A\t0003\t0\t0\t100\t00000000\t0\t0\t0\n\
        eth0\t0000000A\t00000000\t0001\t0\t0\t100\t000000FF\t0\t0\t0\n\
        wlp2s0\t00000000\t0101A8C0\t0003\t0\t0\t600\t00000000\t0\t0\t0\n\
        wlp2s0\t0000FEA9\t00000000\t0001\t0\t0\t1000\t0000FFFF\t0\t0\t0\n\
        wlp2s0\t0001A8C0\t00000000\t0001\t0\t0\t600\t00FFFFFF\t0\t0\t0\n";

    #[test]
    #[cfg(target_endian = "little")]
    fn finds_the_wifi_subnet() {
        // The LAN route wins over the link-local one, whatever the order.
        assert_eq!(
            subnet_for(ROUTES, "wlp2s0"),
            Some(Subnet {
                network: Ipv4Addr::new(192, 168, 1, 0),
                prefix: 24
            })
        );
        // Another interface's subnet is not the WiFi one.
        assert_eq!(
            subnet_for(ROUTES, "eth0").map(|s| s.to_string()),
            Some("10.0.0.0/8".to_string())
        );
        assert_eq!(subnet_for(ROUTES, "wlan9"), None);
        // Only a default route: no subnet.
        let only_default = ROUTES.lines().take(4).collect::<Vec<_>>().join("\n");
        assert_eq!(subnet_for(&only_default, "wlp2s0"), None);
        // A link-local route alone isn't a usable LAN.
        let link_local = format!(
            "{}\n{}",
            ROUTES.lines().next().unwrap(),
            ROUTES.lines().nth(4).unwrap()
        );
        assert_eq!(subnet_for(&link_local, "wlp2s0"), None);
    }

    #[test]
    fn subnet_from_mask() {
        let s = Subnet::from_mask(
            Ipv4Addr::new(192, 168, 1, 77),
            Ipv4Addr::new(255, 255, 255, 0),
        )
        .unwrap();
        assert_eq!(s.to_string(), "192.168.1.0/24");
        assert_eq!(s.addresses(), 256);
        assert!(s.contains(Ipv4Addr::new(192, 168, 1, 254)));
        assert!(!s.contains(Ipv4Addr::new(192, 168, 2, 1)));
        let s =
            Subnet::from_mask(Ipv4Addr::new(10, 1, 2, 3), Ipv4Addr::new(255, 255, 252, 0)).unwrap();
        assert_eq!(s.to_string(), "10.1.0.0/22");
        assert_eq!(
            Subnet::from_mask(Ipv4Addr::new(10, 0, 0, 0), Ipv4Addr::new(255, 0, 255, 0)),
            Err(SubnetError::BadMask(Ipv4Addr::new(255, 0, 255, 0)))
        );
        let all = Subnet::from_mask(Ipv4Addr::new(1, 2, 3, 4), Ipv4Addr::UNSPECIFIED).unwrap();
        assert_eq!(all.prefix, 0);
        assert_eq!(all.addresses(), 1 << 32);
        assert!(all.contains(Ipv4Addr::new(8, 8, 8, 8)));
    }

    #[test]
    fn hosts_skip_network_and_broadcast() {
        let s = Subnet::from_mask(
            Ipv4Addr::new(192, 168, 1, 0),
            Ipv4Addr::new(255, 255, 255, 0),
        )
        .unwrap();
        let hosts: Vec<Ipv4Addr> = s.hosts().collect();
        assert_eq!(hosts.len(), 254);
        assert_eq!(hosts[0], Ipv4Addr::new(192, 168, 1, 1));
        assert_eq!(hosts[253], Ipv4Addr::new(192, 168, 1, 254));
        let p2p = Subnet::from_mask(
            Ipv4Addr::new(10, 0, 0, 0),
            Ipv4Addr::new(255, 255, 255, 254),
        )
        .unwrap();
        assert_eq!(p2p.hosts().count(), 2);
        let single = Subnet::from_mask(Ipv4Addr::new(10, 0, 0, 9), Ipv4Addr::BROADCAST).unwrap();
        assert_eq!(
            single.hosts().collect::<Vec<_>>(),
            [Ipv4Addr::new(10, 0, 0, 9)]
        );
    }

    #[test]
    fn refuses_subnets_larger_than_a_slash_22() {
        let subnet = |prefix: u8| Subnet {
            network: Ipv4Addr::new(10, 0, 0, 0),
            prefix,
        };
        assert_eq!(check_subnet_size(&subnet(24), false), Ok(()));
        assert_eq!(check_subnet_size(&subnet(22), false), Ok(()));
        assert_eq!(
            check_subnet_size(&subnet(21), false),
            Err(SubnetError::TooLarge {
                subnet: subnet(21),
                addresses: 2048,
                max: 22
            })
        );
        assert!(check_subnet_size(&subnet(8), false).is_err());
        // Explicitly allowed.
        assert_eq!(check_subnet_size(&subnet(16), true), Ok(()));
        let err = check_subnet_size(&subnet(16), false)
            .unwrap_err()
            .to_string();
        assert!(err.contains("10.0.0.0/16 has 65536 addresses"), "{err}");
        assert!(err.contains("--allow-large-subnet"), "{err}");
    }
}
