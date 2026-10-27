# Hardware guide

What to buy for a homeguard setup, and what to check before buying. Nothing
here is required to start: `homeguard simulate` runs without any hardware, and
each category below can be added later.

Before buying any Zigbee device, look it up in the
[Zigbee2MQTT supported devices list](https://www.zigbee2mqtt.io/supported-devices/).
The model names below are examples known to work there, not endorsements.

## 1. The hub

The hub runs Docker (Mosquitto, Frigate, Zigbee2MQTT, homeguard) and, if you
use wisense, the WiFi motion detection.

| Option | Good for | Watch out for |
|---|---|---|
| **Mini PC with an Intel N100/N97 or newer** | the best all-rounder: Frigate can use the integrated GPU (OpenVINO) for detection and video decoding | pick one with a WiFi card if you want wisense, and an SSD slot |
| **An old laptop** | free, has a battery (a built-in UPS) and a WiFi card for wisense | detection on its CPU limits you to 1–2 cameras; keep the lid open or set it not to suspend |
| **Raspberry Pi 5 (8 GB)** | small and quiet | the CPU detector struggles beyond one camera: add an AI accelerator (Google Coral USB, or the Hailo-8L AI Kit supported by Frigate); boot from an SSD (NVMe HAT or USB), not an SD card |

Check before buying:

- **64-bit Linux support** (Ubuntu 24.04 LTS or Debian 12/13 is the reference).
- **An SSD.** Recordings and databases write constantly; SD cards and cheap
  USB sticks wear out and corrupt.
- **USB ports** for the Zigbee coordinator (and a Coral, if any).
- **Wired Ethernet**, so the hub does not depend on WiFi to reach the cameras
  (and so the WiFi card is free for wisense).
- **For wisense:** a WiFi card wisense can read. See the
  [wisense README](../../README.md) (laptop cards with `/proc/net/wireless`
  work; ESP32 boards are optional).

### Storage

Frigate here records only around detected people (7 days by default). As a
rough guide, a camera's 1080p stream at 2–4 Mbit/s is about 1–2 GB per hour of
recording. With events only, a 256 GB SSD is plenty for several cameras; for
continuous recording, plan at least 1 TB, or a separate disk mounted as
`data/frigate`.

## 2. Cameras

homeguard needs cameras that stream **locally** over RTSP (most also support
ONVIF). Frigate connects to the camera directly on your LAN; no cloud account
should be needed once the camera is set up.

Check before buying:

- **RTSP is supported and can be enabled locally.** Search for the exact model
  plus "RTSP" before buying.
- **A substream** (a lower-resolution second stream, e.g. 640×360 or 1280×720)
  for detection; Frigate detects on the substream and records the main one.
- **Power over Ethernet (PoE)** for outdoor cameras: one cable for data and
  power, no WiFi to jam, and a single PoE switch to put on a UPS.
- **Night vision** (IR) for the areas you care about at night.
- **Wired, not battery-powered.** Battery cameras sleep to save power and
  usually have no RTSP at all.

| Brand | Notes |
|---|---|
| **Reolink** (wired/PoE models) | RTSP and ONVIF on most wired models; enable them in the camera's network settings. Battery models (Argus, etc.) have no RTSP. |
| **TP-Link Tapo** (C100/C200/C210/C310 and similar) | RTSP after creating a "camera account" in the Tapo app (Advanced settings). Streams: `/stream1` (main), `/stream2` (sub). |
| **Dahua** (and rebrands such as Amcrest, Lorex) | RTSP path `/cam/realmonitor?channel=1&subtype=1` for the substream. |
| **Hikvision** (and Annke and other rebrands) | RTSP path `/Streaming/Channels/102` for the substream. |

**Avoid app-only cameras**: devices that only stream through the
manufacturer's cloud app (for example Ring, Blink, most Wyze firmware, many
no-name "smart" cameras). They cannot feed Frigate, and they send your video
off-site.

Put the camera's address in `.env` (`FRIGATE_CAMERA_FRONT_HOST`), its login in
`FRIGATE_RTSP_USER` / `FRIGATE_RTSP_PASSWORD`, and its RTSP path in
`config/frigate/config.yml`. Give each camera a fixed IP (a DHCP reservation
in your router).

## 3. Zigbee

### Coordinator (USB dongle)

One USB coordinator connects the hub to every Zigbee device.

| Model | Chip | `adapter` in the Zigbee2MQTT config |
|---|---|---|
| Sonoff ZBDongle-P | TI CC2652P | `zstack` |
| Sonoff ZBDongle-E | Silicon Labs EFR32MG21 | `ember` |

Check before buying:

- It is listed as a **recommended adapter** by Zigbee2MQTT.
- Plan a **USB extension cable** (1 m or more): a coordinator plugged straight
  into the hub, especially next to USB 3 ports or an SSD, suffers from
  interference.

### Sensors

| Device | homeguard role | Examples | Check |
|---|---|---|---|
| **Door/window contact** | entry delay and alarm | Aqara door and window sensor, Sonoff SNZB-04 | reports `contact` |
| **PIR motion sensor** | motion source | Aqara motion sensor, Sonoff SNZB-03 | reports `occupancy`; note its "no motion" timeout |
| **Smoke detector** | always alerts, siren | Heiman HS1SA, Frient smoke alarm | reports `smoke`; mains-powered or long battery life |
| **Water leak sensor** | always alerts | Aqara water leak sensor, Sonoff SNZB-05 | reports `water_leak` |
| **Siren** | sounds on alarm and smoke | Heiman HS2WD-E, Frient indoor siren | supports the IAS `warning` command; ideally with a backup battery |
| **Smart plug** | turns lights on during an alarm | IKEA Tretakt, Sonoff S26R2ZB | mains-powered plugs also act as **Zigbee routers**, extending the mesh |

Battery devices talk through routers (mains-powered devices). Put a smart plug
or two between the hub and far rooms so every sensor has a strong link.

Pairing and naming are covered in [setup.md](setup.md#9-pair-zigbee-devices).

## 4. Optional: an ESP32 + HC-SR501 PIR

A cheap DIY motion sensor: an ESP32 development board, an HC-SR501 PIR module,
and [ESPHome](https://esphome.io/). It talks MQTT over WiFi, so enable the
`mqtt-lan` Compose profile, and give it its own MQTT account (add an
`MQTT_ESP_USER` / `MQTT_ESP_PASSWORD` pair to `.env` and `ESP` to the account
list at the top of `scripts/mqtt-passwd.sh`).

homeguard reads Zigbee2MQTT-style topics, so make the ESP32 publish like a
Zigbee PIR:

```yaml
# esphome/hall-pir.yaml (excerpt)
mqtt:
  broker: 192.168.1.10          # HUB_LAN_IP
  username: !secret mqtt_user
  password: !secret mqtt_password

binary_sensor:
  - platform: gpio
    pin: GPIO13
    name: "Hall PIR"
    device_class: motion
    on_press:
      - mqtt.publish:
          topic: zigbee2mqtt/esp_hall_pir
          payload: '{"occupancy": true}'
```

Then declare it in `config/homeguard.toml` as a sensor with
`id = "esp_hall_pir"` and `kind = "pir"`. An HC-SR501 has two trimmers
(sensitivity and hold time) and is prone to false triggers from heat sources,
which is exactly what the fusion rules are for.

## 5. Power and network

- **A UPS** (uninterruptible power supply) for the hub, the router and the
  internet box, and the PoE switch if you have one. Alarms happen during power
  cuts too, and an alert cannot leave without the internet. See
  [hardening.md](hardening.md#9-power).
- **A router that can block a device from the internet** or create a separate
  network for cameras. See [hardening.md](hardening.md#4-isolate-the-cameras).
- **Optional: a 4G/LTE backup** (a router with a SIM, or a phone hotspot the
  router fails over to), because Telegram alerts need the internet.
