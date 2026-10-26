//! Keepalive traffic: RSSI only refreshes when the card receives frames, so
//! while sensing we keep a steady trickle of pings going to the access point.

use std::net::{IpAddr, Ipv4Addr};
use std::path::Path;
use std::process::{ExitStatus, Stdio};

use anyhow::{anyhow, bail, Context, Result};
use thiserror::Error;
use tokio::io::AsyncReadExt;
use tokio::process::{Child, Command};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

use crate::config;
use crate::discover::{self, Candidate, DiscoverOptions};

pub const ROUTE_PATH: &str = "/proc/net/route";

/// `RTF_UP | RTF_GATEWAY` from `<linux/route.h>`.
const RTF_UP: u16 = 0x0001;
const RTF_GATEWAY: u16 = 0x0002;

#[derive(Debug, Error, PartialEq)]
pub enum RouteError {
    #[error("bad hex address {0:?}")]
    BadHex(String),
}

/// Convert an address from `/proc/net/route` to an IPv4 address.
///
/// The kernel prints the raw in-memory (network byte order) `u32` as a hex
/// number in host byte order, so on little-endian machines `0101A8C0` is
/// 192.168.1.1. Reading the hex as a number and taking its native-endian
/// bytes gives the network-order bytes back on any machine.
pub fn parse_route_addr(hex: &str) -> Result<Ipv4Addr, RouteError> {
    if hex.len() != 8 {
        return Err(RouteError::BadHex(hex.to_string()));
    }
    let raw = u32::from_str_radix(hex, 16).map_err(|_| RouteError::BadHex(hex.to_string()))?;
    Ok(Ipv4Addr::from(raw.to_ne_bytes()))
}

/// One row of `/proc/net/route`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Route {
    pub iface: String,
    pub destination: Ipv4Addr,
    pub gateway: Ipv4Addr,
    pub flags: u16,
    pub metric: u32,
    pub mask: Ipv4Addr,
}

impl Route {
    pub fn is_up(&self) -> bool {
        self.flags & RTF_UP != 0
    }

    /// Reached through a gateway, as opposed to directly on the link.
    pub fn via_gateway(&self) -> bool {
        self.flags & RTF_GATEWAY != 0
    }

    /// Destination and mask 0, up, through a gateway.
    pub fn is_default(&self) -> bool {
        self.destination.is_unspecified()
            && self.mask.is_unspecified()
            && self.is_up()
            && self.via_gateway()
    }
}

/// Every well-formed row of a `/proc/net/route` table (the header and
/// malformed rows are skipped).
pub fn parse_routes(table: &str) -> Vec<Route> {
    table
        .lines()
        .skip(1)
        .filter_map(|line| {
            let f: Vec<&str> = line.split_whitespace().collect();
            // Iface Destination Gateway Flags RefCnt Use Metric Mask ...
            Some(Route {
                iface: (*f.first()?).to_string(),
                destination: parse_route_addr(f.get(1)?).ok()?,
                gateway: parse_route_addr(f.get(2)?).ok()?,
                flags: u16::from_str_radix(f.get(3)?, 16).ok()?,
                metric: f.get(6)?.parse().ok()?,
                mask: parse_route_addr(f.get(7)?).ok()?,
            })
        })
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DefaultRoute {
    pub iface: String,
    pub gateway: Ipv4Addr,
    pub metric: u32,
}

/// Default routes (destination and mask 0, gateway flag set) in a
/// `/proc/net/route` table, lowest metric first.
pub fn default_routes(table: &str) -> Vec<DefaultRoute> {
    let mut routes: Vec<DefaultRoute> = parse_routes(table)
        .into_iter()
        .filter(Route::is_default)
        .map(|r| DefaultRoute {
            iface: r.iface,
            gateway: r.gateway,
            metric: r.metric,
        })
        .collect();
    routes.sort_by_key(|r| r.metric);
    routes
}

/// The default gateway reached through `iface`. Pinging a gateway behind a
/// different interface (e.g. Ethernet) would not generate any WiFi frames.
pub fn gateway_for(table: &str, iface: &str) -> Option<Ipv4Addr> {
    default_routes(table)
        .into_iter()
        .find(|r| r.iface == iface)
        .map(|r| r.gateway)
}

/// Read `/proc/net/route` and find the gateway for `iface`.
pub fn read_gateway(path: &Path, iface: &str) -> Result<Option<Ipv4Addr>> {
    let table =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    Ok(gateway_for(&table, iface))
}

/// `--keepalive-target`: an address, or `auto`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeepaliveTarget {
    Ip(IpAddr),
    /// The gateway if it answers pings, otherwise the best device found by
    /// discovery.
    Auto,
}

/// Parse `--keepalive-target`: `auto` (any case) or an IP address.
pub fn parse_keepalive_target(s: &str) -> Result<KeepaliveTarget, String> {
    if s.trim().eq_ignore_ascii_case("auto") {
        return Ok(KeepaliveTarget::Auto);
    }
    s.trim()
        .parse()
        .map(KeepaliveTarget::Ip)
        .map_err(|_| format!("expected an IP address or `auto`, got {s:?}"))
}

/// The device `--keepalive-target auto` pings when the gateway doesn't
/// answer, from candidates ranked best first (see
/// [`discover::rank_candidates`]), with the reason. Only devices that
/// answered pings qualify. A fixed (universal) MAC is preferred over a
/// randomized one: such devices are usually phones, which come and go and
/// sleep.
pub fn choose_keepalive_target(ranked: &[Candidate]) -> Option<(&Candidate, &'static str)> {
    let answering = || ranked.iter().filter(|c| c.received > 0);
    if let Some(c) = answering().find(|c| !c.randomized_mac) {
        return Some((c, "the most reliable device with a fixed MAC address"));
    }
    answering().next().map(|c| {
        (
            c,
            "the most reliable device; every device that answered uses a randomized MAC, so \
             it may leave or sleep",
        )
    })
}

/// Resolve `--keepalive-target auto` for `iface`: the gateway if it answers
/// a few pings (the same as without the flag), otherwise the device chosen
/// by [`choose_keepalive_target`] after a discovery of the subnet.
pub async fn auto_target(iface: &str, gateway: Option<Ipv4Addr>) -> Result<IpAddr> {
    if let Some(gateway) = gateway {
        let stats = discover::ping_host(
            gateway,
            config::DEFAULT_AUTO_GATEWAY_PINGS,
            config::DEFAULT_DISCOVER_TIMEOUT_S,
        )
        .await?;
        if stats.received > 0 {
            tracing::info!(
                "keepalive auto: the gateway {gateway} answers pings ({}/{}); using it",
                stats.received,
                stats.sent
            );
            return Ok(IpAddr::V4(gateway));
        }
        tracing::info!(
            "keepalive auto: the gateway {gateway} doesn't answer pings; looking for another \
             device on {iface}"
        );
    } else {
        tracing::info!("keepalive auto: no default gateway on {iface}; looking for a device");
    }
    let found = discover::discover(&DiscoverOptions {
        iface: iface.to_string(),
        count: config::DEFAULT_DISCOVER_COUNT,
        timeout_s: config::DEFAULT_DISCOVER_TIMEOUT_S,
        concurrency: config::DEFAULT_DISCOVER_CONCURRENCY,
        allow_large_subnet: false,
    })
    .await?;
    let (chosen, why) = choose_keepalive_target(&found.candidates).ok_or_else(|| {
        anyhow!(
            "no device on {} answered pings, so --keepalive-target auto found no target; run \
             with --no-keepalive, or pass --keepalive-target IP with a device you know answers",
            found.subnet
        )
    })?;
    let ms = |v: Option<f64>| v.map_or_else(|| "?".to_string(), |v| format!("{v:.1} ms"));
    tracing::info!(
        "keepalive auto: using {} ({}/{} replies, median {}, jitter {}): {why}",
        chosen.ip,
        chosen.received,
        chosen.sent,
        ms(chosen.median_rtt_ms),
        ms(chosen.jitter_ms)
    );
    Ok(IpAddr::V4(chosen.ip))
}

/// `ping` arguments for a keepalive at `interval_s` seconds.
pub fn ping_args(target: IpAddr, interval_s: f64) -> Vec<String> {
    vec![
        "-i".to_string(),
        format!("{interval_s}"),
        "-q".to_string(),
        "-n".to_string(),
        target.to_string(),
    ]
}

/// A supervised `ping` child process. Stop it with [`Keepalive::stop`]; if
/// the handle is dropped instead, the child is killed with it.
pub struct Keepalive {
    stop: Option<oneshot::Sender<()>>,
    task: JoinHandle<()>,
}

impl Keepalive {
    pub fn start(target: IpAddr, interval_s: f64) -> Result<Self> {
        if !(interval_s.is_finite() && interval_s > 0.0) {
            bail!("ping interval must be a positive number of seconds");
        }
        let child = Command::new("ping")
            .args(ping_args(target, interval_s))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .context("starting `ping` for keepalive traffic (is iputils-ping installed?)")?;
        let (stop, stopped) = oneshot::channel();
        let task = tokio::spawn(supervise(child, stopped, target));
        Ok(Self {
            stop: Some(stop),
            task,
        })
    }

    /// Kill `ping` and wait for it to exit.
    pub async fn stop(mut self) {
        if let Some(stop) = self.stop.take() {
            // Fails only if the supervisor already finished (ping exited).
            let _ = stop.send(());
        }
        if let Err(e) = (&mut self.task).await {
            tracing::debug!("keepalive supervisor: {e}");
        }
    }
}

/// Wait for either an unexpected exit of `ping` (and report it) or a stop
/// request (and kill it).
async fn supervise(mut child: Child, stop: oneshot::Receiver<()>, target: IpAddr) {
    let mut stderr = child.stderr.take();
    tokio::select! {
        status = child.wait() => {
            let mut message = String::new();
            if let Some(stderr) = stderr.as_mut() {
                // Best effort: only used to explain the exit.
                let _ = stderr.read_to_string(&mut message).await;
            }
            report_exit(status, message.trim(), target);
        }
        _ = stop => {
            if let Err(e) = child.kill().await {
                tracing::warn!("could not stop keepalive ping: {e}");
            }
        }
    }
}

fn report_exit(status: std::io::Result<ExitStatus>, message: &str, target: IpAddr) {
    match status {
        // ping exits 0/1 after printing its summary when interrupted (the
        // terminal sends Ctrl+C to the whole process group): a normal shutdown.
        Ok(s) if matches!(s.code(), Some(0 | 1)) => {
            tracing::debug!("keepalive ping to {target} stopped ({s})");
        }
        Ok(s) => tracing::warn!(
            "keepalive ping to {target} exited ({s}){}{message}; RSSI may stop updating. \
             Check --keepalive-target and --ping-interval (older ping versions need root \
             below 0.2 s)",
            if message.is_empty() { "" } else { ": " }
        ),
        Err(e) => tracing::warn!("waiting for keepalive ping failed: {e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TABLE: &str =
        "Iface\tDestination\tGateway \tFlags\tRefCnt\tUse\tMetric\tMask\t\tMTU\tWindow\tIRTT\n\
        eth0\t00000000\t0100000A\t0003\t0\t0\t100\t00000000\t0\t0\t0\n\
        wlp2s0\t00000000\t0101A8C0\t0003\t0\t0\t600\t00000000\t0\t0\t0\n\
        docker0\t000011AC\t00000000\t0001\t0\t0\t0\t0000FFFF\t0\t0\t0\n\
        wlp2s0\t0001A8C0\t00000000\t0001\t0\t0\t600\t00FFFFFF\t0\t0\t0\n";

    #[test]
    #[cfg(target_endian = "little")]
    fn gateway_hex_to_ipv4() {
        assert_eq!(
            parse_route_addr("0101A8C0"),
            Ok(Ipv4Addr::new(192, 168, 1, 1))
        );
        assert_eq!(
            parse_route_addr("FE01A8C0"),
            Ok(Ipv4Addr::new(192, 168, 1, 254))
        );
        assert_eq!(parse_route_addr("0100000A"), Ok(Ipv4Addr::new(10, 0, 0, 1)));
        assert_eq!(parse_route_addr("00000000"), Ok(Ipv4Addr::UNSPECIFIED));
    }

    #[test]
    fn bad_hex() {
        for bad in ["", "0101A8C", "0101A8C0FF", "ZZ01A8C0"] {
            assert_eq!(parse_route_addr(bad), Err(RouteError::BadHex(bad.into())));
        }
    }

    #[test]
    #[cfg(target_endian = "little")]
    fn finds_default_routes_by_metric() {
        let routes = default_routes(TABLE);
        assert_eq!(
            routes,
            vec![
                DefaultRoute {
                    iface: "eth0".into(),
                    gateway: Ipv4Addr::new(10, 0, 0, 1),
                    metric: 100
                },
                DefaultRoute {
                    iface: "wlp2s0".into(),
                    gateway: Ipv4Addr::new(192, 168, 1, 1),
                    metric: 600
                },
            ]
        );
        // The WiFi gateway, even though Ethernet is the preferred route.
        assert_eq!(
            gateway_for(TABLE, "wlp2s0"),
            Some(Ipv4Addr::new(192, 168, 1, 1))
        );
        assert_eq!(gateway_for(TABLE, "wlan9"), None);
        assert_eq!(gateway_for("Iface\tDestination\n", "wlp2s0"), None);
    }

    #[test]
    #[cfg(target_endian = "little")]
    fn parses_every_route_row() {
        let routes = parse_routes(TABLE);
        assert_eq!(routes.len(), 4);
        assert!(routes[1].is_default());
        // The WiFi subnet itself: up, on the link, not a default route.
        let subnet = &routes[3];
        assert_eq!(subnet.iface, "wlp2s0");
        assert_eq!(subnet.destination, Ipv4Addr::new(192, 168, 1, 0));
        assert_eq!(subnet.mask, Ipv4Addr::new(255, 255, 255, 0));
        assert_eq!(subnet.metric, 600);
        assert!(subnet.is_up() && !subnet.via_gateway() && !subnet.is_default());
        // Malformed rows are skipped, not fatal.
        assert!(parse_routes("Iface\tDestination\nwlp2s0\tZZ\n").is_empty());
        assert!(parse_routes("Iface\tDestination\nwlp2s0 0001A8C0 00000000 0001 0 0\n").is_empty());
    }

    #[test]
    fn ping_arguments() {
        let target = IpAddr::V4(Ipv4Addr::new(192, 168, 1, 1));
        assert_eq!(
            ping_args(target, 0.2),
            ["-i", "0.2", "-q", "-n", "192.168.1.1"]
        );
        assert_eq!(
            ping_args(target, 1.0),
            ["-i", "1", "-q", "-n", "192.168.1.1"]
        );
    }

    #[test]
    fn keepalive_target_values() {
        assert_eq!(parse_keepalive_target("auto"), Ok(KeepaliveTarget::Auto));
        assert_eq!(parse_keepalive_target("AUTO"), Ok(KeepaliveTarget::Auto));
        assert_eq!(
            parse_keepalive_target("192.168.1.2"),
            Ok(KeepaliveTarget::Ip(IpAddr::V4(Ipv4Addr::new(
                192, 168, 1, 2
            ))))
        );
        assert!(matches!(
            parse_keepalive_target("fe80::1"),
            Ok(KeepaliveTarget::Ip(IpAddr::V6(_)))
        ));
        for bad in ["", "gateway", "192.168.1", "auto2"] {
            assert!(parse_keepalive_target(bad).is_err(), "{bad}");
        }
    }

    fn candidate(last: u8, received: u32, randomized_mac: bool) -> Candidate {
        Candidate {
            ip: Ipv4Addr::new(192, 168, 1, last),
            mac: None,
            sent: 5,
            received,
            response_rate: f64::from(received) / 5.0,
            median_rtt_ms: (received > 0).then_some(4.0),
            jitter_ms: (received > 0).then_some(1.0),
            gateway: false,
            randomized_mac,
            no_icmp_reply: received == 0,
            associated_ap: None,
        }
    }

    fn chosen(ranked: &[Candidate]) -> Option<u8> {
        choose_keepalive_target(ranked).map(|(c, _)| c.ip.octets()[3])
    }

    #[test]
    fn auto_target_prefers_a_fixed_mac() {
        // Ranked best first: a phone answers best, but a fixed device that
        // answers reliably enough is the safer target.
        let ranked = [
            candidate(71, 5, true),
            candidate(2, 5, false),
            candidate(9, 4, false),
        ];
        assert_eq!(chosen(&ranked), Some(2));
        let (_, why) = choose_keepalive_target(&ranked).unwrap();
        assert!(why.contains("fixed MAC"), "{why}");
        // Among fixed MACs, the ranking decides.
        let ranked = [candidate(9, 4, false), candidate(2, 3, false)];
        assert_eq!(chosen(&ranked), Some(9));
    }

    #[test]
    fn auto_target_falls_back_to_randomized_macs() {
        let ranked = [candidate(71, 5, true), candidate(110, 2, true)];
        assert_eq!(chosen(&ranked), Some(71));
        let (_, why) = choose_keepalive_target(&ranked).unwrap();
        assert!(why.contains("randomized"), "{why}");
    }

    #[test]
    fn auto_target_needs_a_device_that_answers_pings() {
        // A fixed-MAC device that only answered ARP can't be pinged.
        let ranked = [candidate(71, 3, true), candidate(5, 0, false)];
        assert_eq!(chosen(&ranked), Some(71));
        assert_eq!(
            chosen(&[candidate(5, 0, false), candidate(6, 0, true)]),
            None
        );
        assert_eq!(chosen(&[]), None);
    }
}
