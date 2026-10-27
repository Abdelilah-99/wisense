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

Work in progress. The Docker Compose stack, the alarm state machine, the
sensor fusion rules and the MQTT integration work. Arming and disarming come
with the Telegram bot and the dashboard.

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

## How the alarm behaves

| State | Meaning | Leaves when |
|---|---|---|
| `disarmed` | Nothing is watched except smoke and leaks | someone arms |
| `arming` | Exit delay (default 60 s): time to leave | the delay ends → `armed` |
| `armed` (away, home or night) | Active sensors are watched | an active sensor trips |
| `pending` | Entry delay (default 30 s): time to disarm | the delay ends → `triggered` |
| `triggered` | Siren and lights on, critical alert sent | disarm, or auto-reset (default 10 min) re-arms |

- **Modes** decide which sensors count: by default doors and windows in
  every mode, motion sensors only in `away`, cameras in `away` and `night`.
  Override per sensor with `modes = [...]` in
  [config/homeguard.toml](config/homeguard.toml).
- Sensors tripped during the exit delay are ignored, so you can leave.
- A second sensor during the entry delay does not restart the countdown.
- To arm in another mode after an alarm, disarm first.
- **Smoke and water leaks always alert**, whatever the arm state. Smoke also
  starts the siren and turns the lights on. Neither changes the arm state.

## Sensor fusion

A single sensor is easy to fool: a curtain moving in front of a PIR, a fan
disturbing wisense, a shadow on a camera. homeguard combines them before
deciding. Every sensor and camera belongs to a **zone** (`zones = [...]` in
[config/homeguard.toml](config/homeguard.toml)), and these rules apply while
armed:

| What happens | Result |
|---|---|
| Door or window opens | Entry delay, then alarm |
| Motion alone (wisense or PIR) | No alarm. A 20 s confirmation window opens in its zone |
| …and a camera in the same zone sees a person (score ≥ 0.7) within the window | **Alarm now**, with the snapshot |
| …and a *different kind* of sensor (e.g. wisense + PIR) in the same zone within the window | **Alarm now** |
| …and nothing confirms it | Logged; a silent "unconfirmed motion" notice, at most once per zone every 10 min |
| A person on camera alone | Same as motion alone: waits for a PIR or wisense in the zone |
| A door opens within 2 min of a known phone coming home | The owner: no alarm. Disarms if `[presence] auto_disarm = true`, otherwise sends a notice and ignores sensors for 5 min |
| A camera or critical sensor stays offline for 60 s | "Possible tampering" alert |

Two sources of the *same* kind never confirm each other: two wisense links see
the same disturbance, and a sensor repeating itself is no new evidence. A
confirmation window is never extended, so a flapping sensor cannot hold one
open forever.

The same alert (same cause: smoke on one sensor, tampering on one camera) is
sent at most once every 5 minutes; the siren still reacts every time. Every
event is logged whether or not it alerts. All durations and the score
threshold are in the `[fusion]` section.

## MQTT

homeguard logs in to Mosquitto with `MQTT_HOMEGUARD_USER` /
`MQTT_HOMEGUARD_PASSWORD` (the `[mqtt]` section names the variables) and
reconnects with exponential backoff, up to a minute between attempts.

It listens to:

| Topic | From | Becomes |
|---|---|---|
| `zigbee2mqtt/<device>` | Zigbee2MQTT | `contact` → door/window open or closed, `occupancy: true` → PIR motion, `smoke: true`, `water_leak: true` |
| `zigbee2mqtt/<device>/availability` | Zigbee2MQTT | device online / offline |
| `frigate/events` | Frigate | a `person` (new or update, not a false positive) with its best score and event id |
| `frigate/available` | Frigate | every camera online / offline |
| `frigate/<camera>/status/detect` | Frigate | that camera online / offline (`disabled` counts as offline) |
| `homeguard/wisense/<link>` | wisense bridge | wisense motion, heartbeat, offline |

Device names map to sensor ids through `[zigbee.devices]` and
`[wisense.links]`; a name that is not listed is used as the sensor id itself,
so the simplest setup is to rename each Zigbee device to its sensor id in the
Zigbee2MQTT frontend. Messages that do not parse are logged and dropped, and
unknown fields are ignored.

It publishes (for Home Assistant or anything else):

| Topic | Payload | Retained |
|---|---|---|
| `homeguard/availability` | `online` / `offline` (last will) | yes |
| `homeguard/state` | `{"state", "mode", "ha_state", "cause", "sensor", "snapshot_ref"}` | yes |
| `homeguard/actions/alert` | `{"level", "text", "snapshot_ref"}` | no |
| `homeguard/actions/siren` | `ON` / `OFF` | yes |
| `homeguard/actions/lights` | `ON` | no |

`ha_state` uses Home Assistant's alarm panel names (`armed_away`,
`pending`, ...). If `[actuators]` names a Zigbee siren or plugs, homeguard
also drives them directly through `zigbee2mqtt/<device>/set`.

The arm mode is saved in the data directory: after a restart or power cut
homeguard comes back armed in the same mode (an entry delay or alarm in
progress is not resumed; if the intrusion continues, the sensors trip again).

### Running

```sh
docker compose up -d homeguard      # on the hub
docker compose logs -f homeguard
```

Or directly, for development (from `homeguard/`, with the broker running):

```sh
export MQTT_HOMEGUARD_USER=homeguard MQTT_HOMEGUARD_PASSWORD=...
cargo run -- check-config            # validates config/homeguard.toml
cargo run -- run --log debug
```

| Option / variable | Default | |
|---|---|---|
| `--config`, `HOMEGUARD_CONFIG` | `config/homeguard.toml` | configuration file |
| `--data-dir`, `HOMEGUARD_DATA_DIR` | `data/homeguard` | saved arm state, heartbeat |
| `--log`, `HOMEGUARD_LOG` | `info` | log filter, e.g. `debug` or `homeguard=debug,rumqttc=warn` |

Diagnostics go to stderr. `homeguard healthcheck` (used by Docker) succeeds
while the core loop is alive.

## Phone presence

homeguard knows who is home by looking for the household's phones on the
LAN, without root and without an app on the phones. Every `probe_interval`
(30 s) it reads the hub's ARP table (`/proc/net/arp`): a phone whose MAC has
a resolved entry answered the previous round of probes, so it is home. Then
it probes again, pinging each phone's last IP and sending it a one-byte UDP
datagram (either makes the kernel resolve the phone's MAC, even if the phone
ignores pings), and, if `subnet` is set, every address of that subnet, to
find phones that got a new IP.

- A phone is **home** from its first sighting.
- It is **away** only once it has gone unseen for the grace period (10 min
  by default). Phones sleep and stop answering for minutes at a time, so a
  missed probe means nothing.
- After a restart, a phone that is never seen counts as away once the grace
  period has passed.

What presence may do, all **off by default** (`[presence]` in
[config/homeguard.toml](config/homeguard.toml)):

- `auto_arm = true`: arm in away mode once everyone listed has been away past
  the grace period, if the system is disarmed. A manual arm is never changed.
- `auto_disarm = true`: disarm when a known phone comes home, but only from
  away mode (arming, armed or during the entry delay), never from home or
  night mode (someone is already inside) and never once the alarm went off.
- Even with both off, a door opening within 2 minutes of a known phone
  arriving is treated as the owner coming home (see "Sensor fusion").

Configure each person in the config and their phone's MAC in `.env`:

```toml
[[presence.people]]
name = "alice"
mac_env = "HOMEGUARD_PHONE_ALICE"   # .env: HOMEGUARD_PHONE_ALICE=aa:bb:cc:dd:ee:ff
ip = "192.168.1.50"                  # optional: its DHCP reservation
```

A person whose MAC variable is missing or invalid stops homeguard at startup,
because they would otherwise always look away (and could arm the house with
them inside).

> **Turn off MAC randomization for the home network on every phone.**
> Modern phones use a made-up ("private", "random") MAC per network by default,
> and may change it, so homeguard cannot recognize them. On Android:
> *Settings → Network → Wi-Fi → (your network) → Privacy → Use device MAC*.
> On iPhone: *Settings → Wi-Fi → (i) next to your network → Private Wi-Fi
> Address → Off* (or *Fixed*, which stays constant for that network). Then
> read the MAC shown on that same screen. homeguard logs a warning at startup
> for any configured MAC that looks randomized (locally administered).

Presence is a convenience, not a security control: anyone who knows your
WiFi password and a phone's MAC could pretend to be that phone. Keep
`auto_disarm` off if that matters to you.

## wisense bridge

[wisense](../README.md) detects motion from the hub's own WiFi card, so it runs
on the host rather than in Docker. `homeguard-wisense-bridge` (also available
as `homeguard bridge-wisense`) reads its `monitor --json` output and publishes
it on `homeguard/wisense/<link_id>`:

```sh
wisense monitor --json | homeguard-wisense-bridge --zone living-room --link wlp2s0
```

| wisense event | Published |
|---|---|
| `MotionStarted` | `{"type":"motion",...}`: a wisense motion source for the fusion rules |
| `MotionEnded` | `{"type":"motion_ended",...}`: logged only |
| `ZoneChanged` (with a zone) | motion on the link named by the zone |
| `Status`, calibration events | `{"type":"heartbeat"}`, at most every 10 s per link |
| no line for 30 s, end of input, or the bridge killed | `{"type":"offline"}` |

`--link` sets an MQTT last will, so homeguard hears "offline" even if the
bridge itself is killed. A wisense link that stays offline while armed is a
tampering alert. At the end of its input the bridge exits with an error, so
the service manager restarts the pipeline.

The bridge logs in with `MQTT_HOMEGUARD_USER` / `MQTT_HOMEGUARD_PASSWORD`
from its environment (`--username-env` / `--password-env` change the names)
and connects to `127.0.0.1:1883` (`--mqtt-host`, `--mqtt-port`). Other
options: `--silence-timeout` (30 s) and `--heartbeat` (10 s).

In [config/homeguard.toml](config/homeguard.toml), declare the link as a
sensor and map it:

```toml
[[sensors]]
id = "living_room_wisense"
kind = "wisense"
zone = "living-room"

[wisense.links]
wlp2s0 = "living_room_wisense"
```

### As a systemd service on the hub

```sh
# from the repository root: build and install both binaries
cargo build --release
sudo install -m 0755 target/release/wisense /usr/local/bin/
(cd homeguard && cargo build --release --bin homeguard-wisense-bridge)
sudo install -m 0755 homeguard/target/release/homeguard-wisense-bridge /usr/local/bin/

# calibrate wisense once, as the user the service will run as, with the room empty
wisense calibrate

# the MQTT login, readable by root only
sudo install -d -m 0755 /etc/homeguard
sudo install -m 0600 /dev/null /etc/homeguard/wisense-bridge.env
sudoedit /etc/homeguard/wisense-bridge.env
#   MQTT_HOMEGUARD_USER=homeguard
#   MQTT_HOMEGUARD_PASSWORD=<the value from homeguard/.env>

sudo install -m 0644 homeguard/deploy/systemd/homeguard-wisense.service /etc/systemd/system/
sudo systemctl edit homeguard-wisense
#   [Service]
#   User=<your user>
#   Environment=IFACE=wlp2s0 ZONE=living-room
sudo systemctl enable --now homeguard-wisense
journalctl -u homeguard-wisense -f
```

The unit ([deploy/systemd/homeguard-wisense.service](deploy/systemd/homeguard-wisense.service))
runs `wisense monitor --json | homeguard-wisense-bridge` with `Restart=always`;
if either process dies, both restart. MQTT is reachable on `127.0.0.1:1883`
because Compose publishes the broker on the host's loopback.

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
