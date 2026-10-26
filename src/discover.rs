//! `wisense discover`: the devices on the WiFi subnet.
//!
//! The parsing here is pure, so it is tested with fixed tables:
//!
//! - `/proc/net/arp`: the kernel's neighbour table (IP address -> MAC);
//! - `/proc/net/route`: the WiFi subnet is the interface's directly connected
//!   route (destination + mask), parsed with [`traffic::parse_routes`];
//! - MAC addresses, including whether one is a randomized "private" address;
//! - `ping`'s output, and the ranking of the hosts that answered.
//!
//! Probing spawns one `ping` per address (at most `concurrency` at a time,
//! killed if wisense stops), then reads the ARP table again: a device that
//! ignores pings still had to answer ARP, so it shows up as "no ICMP reply".

use std::cmp::Ordering;
use std::fmt;
use std::net::{IpAddr, Ipv4Addr, UdpSocket};
use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;

use anyhow::{anyhow, bail, Context, Result};
use thiserror::Error;
use tokio::process::Command;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;

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

// ------------------------------------------------------------------- ping

#[derive(Debug, Error, PartialEq)]
pub enum PingParseError {
    #[error("no \"packets transmitted\" summary in ping's output")]
    NoSummary,
    #[error("malformed ping summary {0:?}")]
    BadSummary(String),
}

/// What one `ping -c N` run measured.
#[derive(Debug, Clone, PartialEq)]
pub struct PingStats {
    pub sent: u32,
    pub received: u32,
    /// Round-trip time of each reply in ms, duplicates excluded.
    pub rtts_ms: Vec<f64>,
    /// ping's `mdev`: the standard deviation of the round-trip times.
    pub mdev_ms: Option<f64>,
}

impl PingStats {
    /// Replies per request, 0 to 1.
    pub fn response_rate(&self) -> f64 {
        if self.sent == 0 {
            return 0.0;
        }
        f64::from(self.received.min(self.sent)) / f64::from(self.sent)
    }

    pub fn median_ms(&self) -> Option<f64> {
        median(&self.rtts_ms)
    }

    /// How much the round-trip time varies: ping's `mdev`, or the standard
    /// deviation of the replies if ping printed no `rtt` line.
    pub fn jitter_ms(&self) -> Option<f64> {
        self.mdev_ms.or_else(|| population_std(&self.rtts_ms))
    }
}

fn median(values: &[f64]) -> Option<f64> {
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    let mid = sorted.len() / 2;
    match sorted.len() {
        0 => None,
        n if n % 2 == 1 => sorted.get(mid).copied(),
        _ => Some((sorted.get(mid - 1)? + sorted.get(mid)?) / 2.0),
    }
}

fn population_std(values: &[f64]) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    let n = values.len() as f64;
    let mean = values.iter().sum::<f64>() / n;
    Some((values.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / n).sqrt())
}

/// Parse the output of `ping -c N -n <ip>` (iputils; BusyBox's wording is
/// accepted too):
///
/// ```text
/// 64 bytes from 192.168.1.1: icmp_seq=1 ttl=64 time=4.78 ms
/// ...
/// 3 packets transmitted, 3 received, 0% packet loss, time 2003ms
/// rtt min/avg/max/mdev = 4.784/4.914/5.068/0.117 ms
/// ```
///
/// With no reply there is no `rtt` line. Never panics, whatever the input.
pub fn parse_ping_output(text: &str) -> Result<PingStats, PingParseError> {
    let summary = text
        .lines()
        .find(|l| l.contains("packets transmitted"))
        .ok_or(PingParseError::NoSummary)?;
    let bad = || PingParseError::BadSummary(summary.trim().to_string());
    // "5 packets transmitted, 3 received, +2 duplicates, 40% packet loss, ..."
    let count = |part: Option<&str>, word: &str| -> Option<u32> {
        let (n, rest) = part?.trim().split_once(' ')?;
        rest.ends_with(word).then(|| n.parse().ok()).flatten()
    };
    let mut parts = summary.split(',');
    let sent = count(parts.next(), "transmitted").ok_or_else(bad)?;
    let received = count(parts.next(), "received").ok_or_else(bad)?;
    let rtts_ms = text
        .lines()
        .filter(|l| l.contains(" bytes from ") && !l.contains("DUP!"))
        .filter_map(reply_time_ms)
        .collect();
    Ok(PingStats {
        sent,
        received,
        rtts_ms,
        mdev_ms: text.lines().find_map(rtt_mdev_ms),
    })
}

/// `time=4.78 ms` in a reply line.
fn reply_time_ms(line: &str) -> Option<f64> {
    let value = line.split_once("time=")?.1.split_whitespace().next()?;
    value
        .parse::<f64>()
        .ok()
        .filter(|v| v.is_finite() && *v >= 0.0)
}

/// The `mdev` of `rtt min/avg/max/mdev = 4.784/4.914/5.068/0.117 ms`.
fn rtt_mdev_ms(line: &str) -> Option<f64> {
    let line = line.trim();
    if !(line.starts_with("rtt ") || line.starts_with("round-trip ")) {
        return None;
    }
    let (names, values) = line.split_once('=')?;
    let names = names.split_whitespace().last()?;
    let index = names.split('/').position(|n| n == "mdev")?;
    let value = values.split_whitespace().next()?.split('/').nth(index)?;
    value
        .parse::<f64>()
        .ok()
        .filter(|v| v.is_finite() && *v >= 0.0)
}

/// `ping` arguments for probing one host: `count` requests, waiting at most
/// `timeout_s` for each reply, numeric output.
pub fn probe_args(ip: Ipv4Addr, count: u32, timeout_s: f64) -> Vec<String> {
    vec![
        "-c".to_string(),
        count.to_string(),
        "-W".to_string(),
        format!("{timeout_s}"),
        "-n".to_string(),
        ip.to_string(),
    ]
}

// ---------------------------------------------------------------- ranking

/// One probed host, with what the ARP table knows about it.
#[derive(Debug, Clone, PartialEq)]
pub struct ProbeResult {
    pub ip: Ipv4Addr,
    pub ping: PingStats,
    /// From the ARP table, if the host answered ARP.
    pub mac: Option<MacAddr>,
    pub gateway: bool,
}

/// A device as `wisense discover` reports it.
#[derive(Debug, Clone, PartialEq)]
pub struct Candidate {
    pub ip: Ipv4Addr,
    pub mac: Option<MacAddr>,
    pub sent: u32,
    pub received: u32,
    pub response_rate: f64,
    pub median_rtt_ms: Option<f64>,
    pub jitter_ms: Option<f64>,
    pub gateway: bool,
    pub randomized_mac: bool,
    /// Answered ARP but no ping.
    pub no_icmp_reply: bool,
}

impl From<&ProbeResult> for Candidate {
    fn from(r: &ProbeResult) -> Self {
        Self {
            ip: r.ip,
            mac: r.mac,
            sent: r.ping.sent,
            received: r.ping.received,
            response_rate: r.ping.response_rate(),
            median_rtt_ms: r.ping.median_ms(),
            jitter_ms: r.ping.jitter_ms(),
            gateway: r.gateway,
            randomized_mac: r.mac.as_ref().is_some_and(is_randomized_mac),
            no_icmp_reply: r.ping.received == 0,
        }
    }
}

/// Join ping results with the ARP table of `iface`. Hosts that answered
/// neither ping nor ARP are dropped: nothing is there.
pub fn merge_arp(
    probes: Vec<(Ipv4Addr, PingStats)>,
    arp: &[ArpEntry],
    iface: &str,
    gateway: Option<Ipv4Addr>,
) -> Vec<ProbeResult> {
    probes
        .into_iter()
        .filter_map(|(ip, ping)| {
            let mac = arp
                .iter()
                .find(|e| e.ip == ip && e.device == iface)
                .and_then(|e| e.mac);
            (ping.received > 0 || mac.is_some()).then_some(ProbeResult {
                ip,
                ping,
                mac,
                gateway: gateway == Some(ip),
            })
        })
        .collect()
}

/// Best keepalive targets first: the highest response rate, then the
/// steadiest round-trip time (lowest jitter), then the lowest median RTT.
/// Reliability comes before speed: a target only has to keep frames
/// flowing, and a fast host that drops replies leaves gaps.
pub fn rank_candidates(results: &[ProbeResult]) -> Vec<Candidate> {
    let mut ranked: Vec<Candidate> = results.iter().map(Candidate::from).collect();
    ranked.sort_by(|a, b| {
        b.response_rate
            .total_cmp(&a.response_rate)
            .then_with(|| missing_last(a.jitter_ms, b.jitter_ms))
            .then_with(|| missing_last(a.median_rtt_ms, b.median_rtt_ms))
            .then_with(|| a.ip.cmp(&b.ip))
    });
    ranked
}

/// Ascending, with `None` after every value.
fn missing_last(a: Option<f64>, b: Option<f64>) -> Ordering {
    match (a, b) {
        (Some(a), Some(b)) => a.total_cmp(&b),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    }
}

/// Every host of `subnet` except the laptop's own address.
pub fn hosts_to_probe(subnet: &Subnet, own: Option<Ipv4Addr>) -> Vec<Ipv4Addr> {
    subnet.hosts().filter(|ip| Some(*ip) != own).collect()
}

// ---------------------------------------------------------------- probing

/// Settings of one discovery run.
#[derive(Debug, Clone, PartialEq)]
pub struct DiscoverOptions {
    pub iface: String,
    pub count: u32,
    pub timeout_s: f64,
    pub concurrency: usize,
    pub allow_large_subnet: bool,
}

/// The result of [`discover`].
#[derive(Debug, Clone, PartialEq)]
pub struct Discovery {
    pub subnet: Subnet,
    pub gateway: Option<Ipv4Addr>,
    pub own_ip: Option<Ipv4Addr>,
    /// Best first.
    pub candidates: Vec<Candidate>,
}

/// Find and rank the devices on the WiFi subnet of `opts.iface`.
pub async fn discover(opts: &DiscoverOptions) -> Result<Discovery> {
    if opts.count == 0 {
        bail!("--count must be at least 1");
    }
    if !(opts.timeout_s.is_finite() && opts.timeout_s > 0.0) {
        bail!("--timeout must be a positive number of seconds");
    }
    if opts.concurrency == 0 {
        bail!("--concurrency must be at least 1");
    }
    let routes = std::fs::read_to_string(traffic::ROUTE_PATH)
        .with_context(|| format!("reading {}", traffic::ROUTE_PATH))?;
    let subnet = subnet_for(&routes, &opts.iface).with_context(|| {
        format!(
            "no local subnet route on {} in {}; is it connected?",
            opts.iface,
            traffic::ROUTE_PATH
        )
    })?;
    check_subnet_size(&subnet, opts.allow_large_subnet)?;
    let gateway = traffic::gateway_for(&routes, &opts.iface);
    let own_ip = match local_ip_towards(gateway.or_else(|| subnet.hosts().next())) {
        Ok(ip) => ip.filter(|ip| subnet.contains(*ip)),
        Err(e) => {
            tracing::warn!("{e:#}; the laptop's own address may be probed too");
            None
        }
    };

    let hosts = hosts_to_probe(&subnet, own_ip);
    let rounds = hosts.len().div_ceil(opts.concurrency) as f64;
    tracing::info!(
        "probing {} addresses in {subnet} on {} ({} pings each, {} at a time); this takes \
         about {:.0} s",
        hosts.len(),
        opts.iface,
        opts.count,
        opts.concurrency,
        rounds * (f64::from(opts.count - 1) + opts.timeout_s)
    );
    let probes = probe_hosts(hosts, opts.count, opts.timeout_s, opts.concurrency).await?;
    // Read after probing: the pings made the kernel resolve every address.
    let arp = read_arp(Path::new(ARP_PATH))?;
    let results = merge_arp(probes, &arp, &opts.iface, gateway);
    Ok(Discovery {
        subnet,
        gateway,
        own_ip,
        candidates: rank_candidates(&results),
    })
}

/// Read and parse `/proc/net/arp`, warning about malformed lines.
pub fn read_arp(path: &Path) -> Result<Vec<ArpEntry>> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let mut entries = Vec::new();
    for entry in parse_arp(&text) {
        match entry {
            Ok(e) => entries.push(e),
            Err(e) => tracing::warn!("{e}"),
        }
    }
    Ok(entries)
}

/// The laptop's own address on the route to `target`. Connecting a UDP
/// socket sends nothing; the kernel only picks the route and source address.
fn local_ip_towards(target: Option<Ipv4Addr>) -> Result<Option<Ipv4Addr>> {
    let Some(target) = target else {
        return Ok(None);
    };
    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).context("opening a UDP socket")?;
    socket
        .connect((target, 9))
        .with_context(|| format!("finding the laptop's address towards {target}"))?;
    match socket
        .local_addr()
        .context("reading the local address")?
        .ip()
    {
        IpAddr::V4(ip) => Ok(Some(ip)),
        IpAddr::V6(_) => Ok(None),
    }
}

/// Ping every host, at most `concurrency` at a time. The first failure to
/// run `ping` stops the probe; the remaining children are killed.
pub async fn probe_hosts(
    hosts: Vec<Ipv4Addr>,
    count: u32,
    timeout_s: f64,
    concurrency: usize,
) -> Result<Vec<(Ipv4Addr, PingStats)>> {
    let permits = Arc::new(Semaphore::new(concurrency));
    let mut tasks = JoinSet::new();
    for ip in hosts {
        let permits = Arc::clone(&permits);
        tasks.spawn(async move {
            let _permit = permits
                .acquire_owned()
                .await
                .context("probe limiter closed")?;
            ping_host(ip, count, timeout_s).await.map(|s| (ip, s))
        });
    }
    let mut results = Vec::new();
    while let Some(joined) = tasks.join_next().await {
        let probe = joined.map_err(|e| anyhow!("a probe task failed: {e}"))??;
        results.push(probe);
    }
    results.sort_by_key(|(ip, _)| *ip);
    Ok(results)
}

/// Ping one host `count` times. ping exits 1 when nothing answered; that is
/// a result here (0 received), not an error.
async fn ping_host(ip: Ipv4Addr, count: u32, timeout_s: f64) -> Result<PingStats> {
    let output = Command::new("ping")
        .args(probe_args(ip, count, timeout_s))
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .output()
        .await
        .context("running `ping` (is iputils-ping installed?)")?;
    parse_ping_output(&String::from_utf8_lossy(&output.stdout)).with_context(|| {
        format!(
            "ping {ip} ({}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        )
    })
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

    // --------------------------------------------------------------- ping

    /// Real iputils 20240905 output (`ping -c 3 -W 1 -n 192.168.1.1`).
    const PING_OK: &str = "\
PING 192.168.1.1 (192.168.1.1) 56(84) bytes of data.
64 bytes from 192.168.1.1: icmp_seq=1 ttl=64 time=4.78 ms
64 bytes from 192.168.1.1: icmp_seq=2 ttl=64 time=4.89 ms
64 bytes from 192.168.1.1: icmp_seq=3 ttl=64 time=5.07 ms

--- 192.168.1.1 ping statistics ---
3 packets transmitted, 3 received, 0% packet loss, time 2003ms
rtt min/avg/max/mdev = 4.784/4.914/5.068/0.117 ms
";

    /// Real output for an address nobody answers.
    const PING_LOST: &str = "\
PING 192.168.1.253 (192.168.1.253) 56(84) bytes of data.

--- 192.168.1.253 ping statistics ---
2 packets transmitted, 0 received, 100% packet loss, time 1035ms

";

    #[test]
    fn parses_a_normal_ping_run() {
        let stats = parse_ping_output(PING_OK).unwrap();
        assert_eq!(
            stats,
            PingStats {
                sent: 3,
                received: 3,
                rtts_ms: vec![4.78, 4.89, 5.07],
                mdev_ms: Some(0.117),
            }
        );
        assert_eq!(stats.response_rate(), 1.0);
        assert_eq!(stats.median_ms(), Some(4.89));
        assert_eq!(stats.jitter_ms(), Some(0.117));
    }

    #[test]
    fn parses_total_loss() {
        let stats = parse_ping_output(PING_LOST).unwrap();
        assert_eq!((stats.sent, stats.received), (2, 0));
        assert_eq!(stats.response_rate(), 0.0);
        assert_eq!(stats.median_ms(), None);
        assert_eq!(stats.jitter_ms(), None);
        // Unreachable errors and their counter don't confuse it.
        let errors = "\
From 192.168.1.50 icmp_seq=1 Destination Host Unreachable
--- 192.168.1.99 ping statistics ---
5 packets transmitted, 0 received, +3 errors, 100% packet loss, time 4085ms
";
        let stats = parse_ping_output(errors).unwrap();
        assert_eq!((stats.sent, stats.received), (5, 0));
        assert!(stats.rtts_ms.is_empty());
    }

    #[test]
    fn partial_loss_duplicates_and_busybox() {
        let text = "\
64 bytes from 10.0.0.2: icmp_seq=1 ttl=64 time=12.0 ms
64 bytes from 10.0.0.2: icmp_seq=1 ttl=64 time=13.0 ms (DUP!)
64 bytes from 10.0.0.2: icmp_seq=3 ttl=64 time=2.0 ms
--- 10.0.0.2 ping statistics ---
4 packets transmitted, 2 received, +1 duplicates, 50% packet loss, time 3004ms
rtt min/avg/max/mdev = 2.000/7.000/12.000/5.000 ms
";
        let stats = parse_ping_output(text).unwrap();
        assert_eq!((stats.sent, stats.received), (4, 2));
        assert_eq!(stats.rtts_ms, vec![12.0, 2.0]);
        assert_eq!(stats.response_rate(), 0.5);
        assert_eq!(stats.median_ms(), Some(7.0));
        assert_eq!(stats.jitter_ms(), Some(5.0));

        // BusyBox: "packets received", "round-trip" and no mdev; the jitter
        // then comes from the replies themselves.
        let busybox = "\
64 bytes from 10.0.0.3: seq=0 ttl=64 time=1.000 ms
64 bytes from 10.0.0.3: seq=1 ttl=64 time=3.000 ms
--- 10.0.0.3 ping statistics ---
2 packets transmitted, 2 packets received, 0% packet loss
round-trip min/avg/max = 1.000/2.000/3.000 ms
";
        let stats = parse_ping_output(busybox).unwrap();
        assert_eq!((stats.sent, stats.received), (2, 2));
        assert_eq!(stats.mdev_ms, None);
        assert_eq!(stats.jitter_ms(), Some(1.0));
        assert_eq!(stats.median_ms(), Some(2.0));
    }

    #[test]
    fn garbage_ping_output() {
        assert_eq!(parse_ping_output(""), Err(PingParseError::NoSummary));
        assert_eq!(
            parse_ping_output("ping: connect: Network is unreachable\n"),
            Err(PingParseError::NoSummary)
        );
        for summary in [
            "x packets transmitted, 3 received",
            "3 packets transmitted",
            "3 packets transmitted, many received",
            "3 packets transmitted, 3 lost",
        ] {
            assert_eq!(
                parse_ping_output(summary),
                Err(PingParseError::BadSummary(summary.into())),
                "{summary}"
            );
        }
        // Unparsable reply times and rtt values are ignored, not fatal.
        let odd = "\
64 bytes from 10.0.0.2: icmp_seq=1 ttl=64 time=fast ms
1 packets transmitted, 1 received, 0% packet loss
rtt min/avg/max/mdev = a/b/c/d ms
";
        let stats = parse_ping_output(odd).unwrap();
        assert!(stats.rtts_ms.is_empty());
        assert_eq!(stats.mdev_ms, None);

        let mut rng = crate::source::synthetic::Rng::new(11);
        for _ in 0..2000 {
            let mut bytes = PING_OK.as_bytes().to_vec();
            for _ in 0..3 {
                let i = (rng.next_u64() as usize) % bytes.len();
                bytes[i] = (rng.next_u64() & 0xff) as u8;
            }
            bytes.truncate((rng.next_u64() as usize) % (bytes.len() + 1));
            let _ = parse_ping_output(&String::from_utf8_lossy(&bytes));
        }
    }

    #[test]
    fn median_and_spread() {
        assert_eq!(median(&[]), None);
        assert_eq!(median(&[3.0]), Some(3.0));
        assert_eq!(median(&[9.0, 1.0, 5.0]), Some(5.0));
        assert_eq!(median(&[4.0, 1.0, 3.0, 2.0]), Some(2.5));
        assert_eq!(population_std(&[]), None);
        assert_eq!(population_std(&[7.0]), Some(0.0));
        assert_eq!(
            population_std(&[2.0, 4.0, 4.0, 4.0, 5.0, 5.0, 7.0, 9.0]),
            Some(2.0)
        );
    }

    #[test]
    fn probe_arguments() {
        assert_eq!(
            probe_args(Ipv4Addr::new(192, 168, 1, 7), 5, 1.0),
            ["-c", "5", "-W", "1", "-n", "192.168.1.7"]
        );
        assert_eq!(
            probe_args(Ipv4Addr::new(10, 0, 0, 2), 3, 0.5),
            ["-c", "3", "-W", "0.5", "-n", "10.0.0.2"]
        );
    }

    // ------------------------------------------------------------ ranking

    fn stats(sent: u32, rtts: &[f64], mdev: Option<f64>) -> PingStats {
        PingStats {
            sent,
            received: u32::try_from(rtts.len()).unwrap(),
            rtts_ms: rtts.to_vec(),
            mdev_ms: mdev,
        }
    }

    fn probe(last: u8, ping: PingStats, mac: Option<&str>) -> ProbeResult {
        ProbeResult {
            ip: Ipv4Addr::new(192, 168, 1, last),
            ping,
            mac: mac.map(super::tests::mac),
            gateway: false,
        }
    }

    fn order(ranked: &[Candidate]) -> Vec<u8> {
        ranked.iter().map(|c| c.ip.octets()[3]).collect()
    }

    #[test]
    fn reliable_beats_fast_but_flaky() {
        let fast_flaky = probe(10, stats(5, &[1.0, 1.1, 0.9], Some(0.1)), None);
        let reliable = probe(20, stats(5, &[4.0, 4.2, 3.9, 4.1, 4.0], Some(0.1)), None);
        assert_eq!(order(&rank_candidates(&[fast_flaky, reliable])), [20, 10]);
    }

    #[test]
    fn steadier_then_faster_breaks_ties() {
        let jittery = probe(1, stats(5, &[2.0; 5], Some(3.0)), None);
        let steady_slow = probe(2, stats(5, &[9.0; 5], Some(0.2)), None);
        let steady_fast = probe(3, stats(5, &[3.0; 5], Some(0.2)), None);
        let ranked = rank_candidates(&[jittery, steady_slow, steady_fast]);
        assert_eq!(order(&ranked), [3, 2, 1]);
        assert_eq!(ranked[0].median_rtt_ms, Some(3.0));
        assert_eq!(ranked[0].jitter_ms, Some(0.2));
        assert_eq!(ranked[0].response_rate, 1.0);
    }

    #[test]
    fn no_icmp_reply_ranks_last_and_flags_are_set() {
        let silent = probe(5, stats(5, &[], None), Some("38:91:48:aa:bb:cc"));
        let phone = probe(
            9,
            stats(5, &[30.0; 5], Some(9.0)),
            Some("76:62:94:1a:2b:3c"),
        );
        let ranked = rank_candidates(&[silent, phone]);
        assert_eq!(order(&ranked), [9, 5]);
        assert!(ranked[0].randomized_mac && !ranked[0].no_icmp_reply);
        assert!(!ranked[1].randomized_mac && ranked[1].no_icmp_reply);
        assert_eq!(ranked[1].median_rtt_ms, None);
    }

    #[test]
    #[cfg(target_endian = "little")]
    fn merges_arp_and_marks_the_gateway() {
        let arp: Vec<ArpEntry> = parse_arp(ARP).into_iter().map(Result::unwrap).collect();
        let ip = |last: u8| Ipv4Addr::new(192, 168, 1, last);
        let probes = vec![
            (ip(1), stats(5, &[3.0; 5], Some(0.1))),
            // Ignores ping, but answered ARP: kept.
            (ip(7), stats(5, &[], None)),
            // Ignores ping and ARP is incomplete: nobody there.
            (ip(40), stats(5, &[], None)),
            // Not in the ARP table at all.
            (ip(99), stats(5, &[], None)),
            // Answers ping; its ARP entry is on another interface.
            (ip(23), stats(5, &[8.0; 5], Some(1.0))),
        ];
        let results = merge_arp(probes, &arp, "wlp2s0", Some(ip(1)));
        let ips: Vec<u8> = results.iter().map(|r| r.ip.octets()[3]).collect();
        assert_eq!(ips, [1, 7, 23]);
        assert!(results[0].gateway && !results[1].gateway);
        assert_eq!(results[0].mac, Some(mac("f4:fc:49:04:b2:b9")));
        assert_eq!(results[1].mac, Some(mac("38:91:48:aa:bb:cc")));
        assert_eq!(results[2].mac, Some(mac("76:62:94:1a:2b:3c")));
        let other_iface = merge_arp(
            vec![(ip(23), stats(5, &[8.0; 5], None))],
            &arp,
            "eth0",
            None,
        );
        assert_eq!(other_iface[0].mac, None);
    }

    #[test]
    fn own_address_is_not_probed() {
        let subnet = Subnet {
            network: Ipv4Addr::new(192, 168, 1, 0),
            prefix: 29,
        };
        let own = Ipv4Addr::new(192, 168, 1, 4);
        let hosts = hosts_to_probe(&subnet, Some(own));
        assert_eq!(hosts.len(), 5);
        assert!(!hosts.contains(&own));
        assert_eq!(hosts_to_probe(&subnet, None).len(), 6);
    }
}
