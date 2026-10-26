# homeguard

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

Work in progress. Nothing works yet: this is the repository skeleton.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option.

Copyright 2026 Abdelilah Bouchikhi.
