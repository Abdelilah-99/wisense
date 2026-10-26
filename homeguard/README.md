# homeguard

[![homeguard CI](https://github.com/Abdelilah-99/wisense/actions/workflows/homeguard.yml/badge.svg)](https://github.com/Abdelilah-99/wisense/actions/workflows/homeguard.yml)

A local, self-hosted home security system. It runs entirely on a hub in your
home (an old laptop, a mini PC or a Raspberry Pi 5 running Linux). Nothing
depends on the cloud except Telegram, which delivers the alerts.

**Only monitor property you own or are allowed to monitor, and tell everyone
who lives in or visits the home about the cameras and sensors.**

## What it is

homeguard combines off-the-shelf open-source components with one custom
service:

| Component | Role |
|---|---|
| [Mosquitto](https://mosquitto.org/) | MQTT broker: every component talks through it |
| [Frigate](https://frigate.video/) | Camera NVR with person detection |
| [Zigbee2MQTT](https://www.zigbee2mqtt.io/) | Door/window, PIR motion, smoke and leak sensors, siren, plugs |
| [wisense](../README.md) | WiFi motion detection from the hub's own WiFi card |
| Home Assistant (optional) | Home automation UI, if you want one |
| **homeguard** (this crate) | Owns **all** security decisions |

The `homeguard` service is written in Rust and owns:

- the arm/disarm state machine (exit and entry delays, modes);
- sensor fusion rules that cut false alarms (motion must be confirmed);
- phone presence detection (who is home);
- alerts over Telegram, the event log, and a small web dashboard and API.

## Architecture

```
  Zigbee sensors        IP cameras (RTSP)        hub WiFi card
  door, PIR, smoke,          │                        │
  leak, siren, plugs         ▼                        ▼
        │              ┌──────────┐           ┌──────────────┐
        ▼              │ Frigate  │           │   wisense    │ (host, not Docker)
  ┌─────────────┐      │ person   │           │ monitor      │
  │ Zigbee2MQTT │      │ detection│           │   --json     │
  └──────┬──────┘      └────┬─────┘           └──────┬───────┘
         │                  │                        │ stdin
         │                  │                 ┌──────▼───────┐
         │                  │                 │ wisense      │
         │                  │                 │ bridge       │
         │                  │                 └──────┬───────┘
         ▼                  ▼                        ▼
  ┌──────────────────────────────────────────────────────────┐
  │                   Mosquitto (MQTT broker)                │
  └──────────────────────────────┬───────────────────────────┘
                                 │
                       ┌─────────▼──────────┐   LAN probe   ┌────────┐
                       │     homeguard      │◄─────────────►│ phones │
                       │ ┌────────────────┐ │               └────────┘
                       │ │ pure core:     │ │
                       │ │ state machine, │ │──► Telegram (alerts, commands)
                       │ │ fusion rules,  │ │
                       │ │ presence       │ │──► SQLite event log
                       │ └────────────────┘ │
                       │  MQTT · HTTP · DB  │──► web dashboard (LAN only)
                       └────────────────────┘
```

## Status

Work in progress. The Docker Compose stack is in place; the `homeguard`
service itself is still a placeholder.

## Quick start

On the hub (Linux with Docker Engine and the Compose plugin installed):

```sh
git clone https://github.com/Abdelilah-99/wisense.git
cd wisense/homeguard
cp .env.example .env
$EDITOR .env              # replace every change-me, set HUB_LAN_IP
scripts/init.sh           # data dirs, MQTT password file, Zigbee2MQTT config
docker compose up -d
docker compose ps         # every service should become "healthy"
```

Then:

- the dashboard is at `http://HUB_LAN_IP:8080`;
- the Frigate UI is at `https://HUB_LAN_IP:8971` (the first login password is
  printed in `docker compose logs frigate`);
- edit [config/frigate/config.yml](config/frigate/config.yml) for your
  cameras and [config/homeguard.toml](config/homeguard.toml) for your sensors;
- attach the Zigbee USB coordinator by uncommenting the `devices:` lines of
  the `zigbee2mqtt` service.

### What is exposed

| Port | Bound to | Service |
|---|---|---|
| 8080 | `HUB_LAN_IP` | homeguard dashboard |
| 8971 | `HUB_LAN_IP` | Frigate UI (login required) |
| 1883 | loopback | MQTT, for homeguard and the wisense bridge on the host |
| 5000 | loopback | Frigate API (no login), for homeguard's snapshots |
| 8081 | loopback | Zigbee2MQTT frontend; use `ssh -L 8081:127.0.0.1:8081 hub` |
| 1883 | `HUB_LAN_IP` | MQTT, only with the `mqtt-lan` profile |
| 8123 | `HUB_LAN_IP` | Home Assistant, only with the `ha` profile |

Optional services are Compose profiles, enabled in `.env` with
`COMPOSE_PROFILES=mqtt-lan,ha`:

- `mqtt-lan` exposes MQTT on the LAN (for ESP32 boards and other devices
  elsewhere in the house). Logins are still required.
- `ha` starts Home Assistant. Add an `MQTT_HA_USER` / `MQTT_HA_PASSWORD`
  account in `.env`, re-run `scripts/mqtt-passwd.sh`, and point its MQTT
  integration at broker `mosquitto`, port 1883.

The `homeguard` container uses the host network because presence detection
needs the hub's ARP table; it reaches MQTT and Frigate over loopback.

MQTT never accepts anonymous clients. To change a password, edit `.env`, run
`scripts/mqtt-passwd.sh`, then `docker compose restart mosquitto` and the
services that use that account.

## Development

Every commit must pass, from `homeguard/`:

```sh
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
docker compose config -q
scripts/check-secrets.sh
```

CI ([.github/workflows/homeguard.yml](../.github/workflows/homeguard.yml))
runs the same checks on every push and pull request that touches `homeguard/`.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option.

Copyright 2026 Abdelilah Bouchikhi.
