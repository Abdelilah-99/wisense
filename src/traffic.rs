//! Keepalive traffic: RSSI only refreshes when the card receives frames, so
//! while sensing we keep a steady trickle of pings going to the access point.

use std::net::{IpAddr, Ipv4Addr};
use std::path::Path;
use std::process::{ExitStatus, Stdio};

use anyhow::{bail, Context, Result};
use thiserror::Error;
use tokio::io::AsyncReadExt;
use tokio::process::{Child, Command};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DefaultRoute {
    pub iface: String,
    pub gateway: Ipv4Addr,
    pub metric: u32,
}

/// Default routes (destination and mask 0, gateway flag set) in a
/// `/proc/net/route` table, lowest metric first.
pub fn default_routes(table: &str) -> Vec<DefaultRoute> {
    let mut routes: Vec<DefaultRoute> = table
        .lines()
        .skip(1)
        .filter_map(|line| {
            let f: Vec<&str> = line.split_whitespace().collect();
            // Iface Destination Gateway Flags RefCnt Use Metric Mask ...
            let (iface, dest, gw, flags, metric, mask) = (
                f.first()?,
                f.get(1)?,
                f.get(2)?,
                f.get(3)?,
                f.get(6)?,
                f.get(7)?,
            );
            let flags = u16::from_str_radix(flags, 16).ok()?;
            let is_default = *dest == "00000000"
                && *mask == "00000000"
                && flags & (RTF_UP | RTF_GATEWAY) == RTF_UP | RTF_GATEWAY;
            if !is_default {
                return None;
            }
            Some(DefaultRoute {
                iface: (*iface).to_string(),
                gateway: parse_route_addr(gw).ok()?,
                metric: metric.parse().ok()?,
            })
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
}
