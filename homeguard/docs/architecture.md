# Architecture

How homeguard is put together and why. For using it, see the
[README](../README.md); for the tests, [testing.md](testing.md).

## The big picture

```
 ┌──────────────────────────── the hub (Linux) ────────────────────────────────┐
 │                                                                             │
 │  Docker network "internal"                         host                     │
 │  ┌─────────────┐  ┌───────────┐                    ┌───────────────────┐    │
 │  │ Zigbee2MQTT │  │  Frigate  │◄── RTSP ── cameras │ wisense monitor   │    │
 │  │ (USB dongle)│  │ (detector)│                    │   --json          │    │
 │  └──────┬──────┘  └─────┬─────┘                    │      │ stdin      │    │
 │         │ MQTT          │ MQTT    API :5000 (lo)   │ homeguard-wisense │    │
 │         ▼               ▼              ▲           │   -bridge         │    │
 │  ┌─────────────────────────────────┐   │           └────────┬──────────┘    │
 │  │   Mosquitto (logins required)   │◄──┼────── 127.0.0.1:1883 ┘             │
 │  └────────────────┬────────────────┘   │                                    │
 │                   │ 127.0.0.1:1883     │                                    │
 │  ┌────────────────▼────────────────────┴───────────────┐  /proc/net/arp     │
 │  │ homeguard (host network)                            │◄─ ping / UDP ─ phones
 │  │                                                     │                    │
 │  │  MQTT in ─► parse ─► ┌───────────────────────┐      │                    │
 │  │  presence probe ───► │ core loop (one task)  │      │                    │
 │  │  Telegram cmds ────► │  Engine: machine +    │ ─► actions                │
 │  │  dashboard cmds ───► │  fusion + presence    │      │                    │
 │  │  ticker (500 ms) ──► └───────────────────────┘      │                    │
 │  │                         │ snapshot (watch)          │                    │
 │  │  actions ─► MQTT out (state, siren, lights)         │                    │
 │  │          ─► alert queue ─► Telegram worker ─────────┼──► api.telegram.org│
 │  │          ─► event log queue ─► SQLite               │                    │
 │  │  axum dashboard / API on HUB_LAN_IP:8080 ◄──────────┼──── LAN browsers   │
 │  └─────────────────────────────────────────────────────┘                    │
 └─────────────────────────────────────────────────────────────────────────────┘
```

Everything runs on the hub. The only traffic leaving the house is homeguard's
outgoing connections to Telegram and Discord (HTTPS, and the Discord bot's
gateway WebSocket), whichever are configured. Alerts fan out to one queue per
channel, so a slow or failing service never holds up another.

## MQTT topics

| Topic | Publisher | Payload | Retained | homeguard |
|---|---|---|---|---|
| `zigbee2mqtt/<device>` | Zigbee2MQTT | device state JSON (`contact`, `occupancy`, `smoke`, `water_leak`, …) | per device | reads |
| `zigbee2mqtt/<device>/availability` | Zigbee2MQTT | `{"state":"online"\|"offline"}` | yes | reads |
| `zigbee2mqtt/<device>/set` | homeguard | siren `warning` command, plug `{"state":"ON"}` | no | writes |
| `frigate/events` | Frigate | tracked object `new`/`update`/`end` with `before`/`after` | no | reads (`person` only) |
| `frigate/available` | Frigate | `online` / `offline` (will) / `stopped` | yes | reads |
| `frigate/<camera>/status/detect` | Frigate | `online` / `offline` / `disabled` | yes | reads |
| `homeguard/wisense/<link>` | wisense bridge | `{"type":"motion"\|"motion_ended"\|"heartbeat"\|"offline","zone",…}` | no | reads |
| `homeguard/availability` | homeguard | `online` / `offline` (will) | yes | writes |
| `homeguard/state` | homeguard | `{"state","mode","ha_state","cause","sensor","snapshot_ref"}` | yes | writes |
| `homeguard/actions/alert` | homeguard | `{"level","text","snapshot_ref"}` | no | writes |
| `homeguard/actions/siren` | homeguard | `ON` / `OFF` | yes | writes |
| `homeguard/actions/lights` | homeguard | `ON` | no | writes |

`ha_state` uses Home Assistant's alarm panel vocabulary (`armed_away`,
`pending`, `triggered`, …), so an MQTT alarm panel can display homeguard's
state.

## A tour of the code

```
src/
├── core/            the pure core: no clock, no I/O
│   ├── time.rs      Ts (milliseconds since an origin), durations as text
│   ├── event.rs     Input, Event, Command, Mode, Action, AlertLevel
│   ├── state.rs     State (Disarmed … Triggered) and Cause
│   ├── config.rs    CoreConfig, the sensor/camera/zone Catalog
│   ├── machine.rs   AlarmMachine: arm/disarm, exit/entry delays, auto-reset, safety
│   ├── engine.rs    Engine: fusion rules on top of the machine (the entry point)
│   └── presence.rs  PresenceTracker: home/away with a grace period
├── parse/           pure parsers: MQTT payloads → core events
│   ├── zigbee.rs, frigate.rs, wisense.rs, arp.rs
│   └── mod.rs       Mapping: topic routing and device-name → sensor-id mapping
├── publish.rs       pure: actions → outgoing MQTT messages
├── status.rs        pure: the snapshot shown by Telegram and the dashboard
├── sim.rs           scenario replay through the real Engine (pure, plus a loader)
├── config.rs        the whole homeguard.toml (core + shell sections)
├── run.rs           `homeguard run`: the core loop, dispatch, persistence, healthcheck
├── mqtt.rs          connection, reconnect with backoff, publish without waiting
├── presence.rs      LAN probing (ARP table, ping, UDP), feeds the tracker
├── bridge.rs        the wisense bridge (pure Bridge + stdin/MQTT shell)
├── notify.rs        pure: bot commands for any prefix, authorization, alert and status texts
├── chat.rs          runs a bot command against the core (shared by both bots)
├── delivery.rs      retry with backoff and rate limits, secrets scrubbed from errors
├── frigate.rs       Frigate snapshots (alerts, /snapshot, the dashboard proxy)
├── telegram/        Bot API: alert worker, long-polling command loop
├── discord/         gateway.rs (pure protocol: frames, heartbeats, authorization),
│                    mod.rs (webhook and bot REST, alert worker, gateway session)
├── db.rs            SQLite event log (sqlx, migrations/), batched writer, pruning
├── web/             axum API and dashboard; auth.rs (pure Basic auth, throttling)
├── main.rs          CLI: run, check-config, simulate, bridge-wisense, healthcheck
└── bin/homeguard-wisense-bridge.rs
```

### Life of an event

1. Zigbee2MQTT publishes `zigbee2mqtt/front_door` `{"contact": false, …}`.
2. `mqtt::run_incoming` receives it; `Mapping::parse` routes it to
   `parse::zigbee`, which returns `ContactChanged { sensor: "front_door",
   open: true }` (a malformed payload is logged and dropped instead).
3. The event goes over a channel to the core loop, which stamps it with the
   monotonic clock (`Input { at, event }`), writes it to the event log queue
   and calls `Engine::handle`.
4. The engine applies the fusion rules (owner arriving? grace period?) and
   asks the machine for an intrusion with an entry delay. The machine moves to
   `Pending { until: at + 30 s }` and returns `[StateChanged, SendAlert(High)]`.
5. The dispatcher logs each action, saves the arm mode to disk, queues the
   alert for Telegram (`try_send`, never waiting), publishes
   `homeguard/state` and `homeguard/actions/alert`, and queues both for the
   event log.
6. The core loop rebuilds the status snapshot, which the dashboard and
   `/status` read.
7. 30 s later a tick reaches the engine, `next_deadline` has passed, the
   machine moves to `Triggered` and returns `SirenOn`, `LightsOn` and a
   critical alert; the dispatcher turns `SirenOn` into
   `zigbee2mqtt/<siren>/set`.

## Design decisions

### A pure core, an imperative shell

Everything that decides (the state machine, the fusion rules, presence
logic, parsers, the bridge's line mapping, command parsing, authorization)
is plain code that never reads the clock and never does I/O. Time arrives as
a value (`Ts`) inside each input, and decisions leave as `Action` values.
Everything that touches the world (MQTT, HTTP, files, processes, the clock)
stays at the edges, in a few thin modules.

Why:

- **Exact-timing tests.** "The alarm goes off exactly 30 s after the door
  opens, not 29.999 s" is a one-line assertion, with no sleeps and no flaky
  timing.
- **Simulation for free.** `homeguard simulate` replays a timeline through the
  same `Engine` the hub runs, jumping straight to each deadline the engine
  reports. It is not a model of the system, it *is* the system.
- **One owner of the state.** A single task owns the engine. Everything else
  sends it messages, so there are no locks around the alarm state and no
  races between, say, a Telegram `/disarm` and a door opening.
- **Slow I/O cannot delay a decision.** The core never awaits the network or
  the disk. Alerts, MQTT messages and log rows are queued with `try_send`;
  if a queue is full the item is dropped with an error log rather than
  blocking the alarm.

### Why fusion rules

Single sensors lie: a PIR sees a curtain in a draught or a heater, wisense
sees a fan, a camera sees a shadow or a cat. An alarm that goes off for
those is soon ignored or switched off, which is worse than no alarm. So:

- **Doors and windows are trusted** (a magnet moving is rarely spurious) but
  get an **entry delay** so the owner can disarm.
- **Motion alone is not an alarm.** It opens a short **confirmation window**
  (20 s) in its **zone**. Confirmation is either a **person on a camera** in
  the same zone (Frigate's detector, with a score threshold) or **motion from a
  different kind of sensor** in the same zone. Independent physics (heat,
  radio, image) rarely fail together.
- **Same-kind sources never confirm each other.** Two wisense links see the
  same radio disturbance, and a sensor repeating itself is no new evidence.
- **Windows are never extended**, so a flapping sensor cannot hold one open
  forever, and the "unconfirmed motion" notice is rate-limited per zone, so a
  faulty sensor produces a few silent notices, not hundreds.
- **Zones** are what make confirmation meaningful: a person in the garden
  says nothing about motion in the bedroom.
- **Tampering:** a camera or critical sensor that goes offline while armed is
  reported. Offline reports are **debounced** (60 s) because Frigate's camera
  status flaps while it restarts a stream.
- **One cause, one alert per cooldown.** Smoke detectors repeat their state
  every few seconds; the siren reacts every time, Telegram hears it once per
  5 minutes.

### Why presence uses a grace period

Phones are bad at being present. To save battery they sleep, drop off WiFi,
stop answering pings for minutes, and randomize their MAC. A naive "unseen =
away" flips between home and away all evening, which with auto-arm would arm
the house with people inside.

So a phone is **home on the first sighting** (an arrival is reliable) but
**away only after the grace period** (10 min) without one (a departure must be
confirmed by sustained absence). The asymmetry is deliberate: arriving
quickly matters (auto-disarm, owner detection), leaving late is harmless.

Probing reads the ARP table *before* sending the next round of probes, so
each phone's entry has a full interval to resolve or fail. A phone that left
does not look present from a stale entry. A person whose MAC is not
configured stops startup, because they would always look away, and both
auto-arm and auto-disarm are off by default.

### Other decisions

- **homeguard runs in the host network.** Presence needs the hub's ARP table,
  which a bridged container cannot see. In exchange, MQTT and the Frigate API
  are published on the hub's loopback (not the LAN), and the dashboard binds
  to `HUB_LAN_IP` only.
- **The arm mode survives restarts.** It is written to the data directory on
  every state change; after a power cut homeguard comes back armed (an entry
  delay or alarm in progress is not resumed: if the intrusion continues, the
  sensors trip again).
- **Secrets are named, not written.** Config files name environment
  variables (`password_env = "…"`); values live in `.env` only. Telegram
  errors are stripped of their URL, which contains the bot token.
- **Chat commands have no PIN, the dashboard does.** The Telegram and Discord
  allow-lists and the phone's own lock are the protection; a browser on the
  LAN gets a password and a PIN, with lockouts. Commands queued while the hub
  was down are ignored after 2 minutes (Telegram's message date, Discord's
  snowflake timestamp).
- **Discord: webhook for alerts, bot for commands.** A webhook can only post,
  so commands need a bot. It uses the gateway WebSocket (outbound) with
  prefix commands rather than slash-command interactions, which keeps it
  simple, at the cost of the privileged Message Content intent (a toggle
  for a bot in your own server).
- **Unknown sensors never trigger.** An event for an id not in the config is
  logged and ignored, so a typo cannot cause a false alarm (it can hide a
  sensor; `check-config` and the dashboard show what is configured).
- **wisense talks through a bridge with its own small message format**, so
  homeguard does not depend on wisense's internal event types, and wisense can
  run on the host while homeguard runs in Docker.
