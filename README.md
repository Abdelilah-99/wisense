# wisense

[![CI](https://github.com/Abdelilah-99/wisense/actions/workflows/ci.yml/badge.svg)](https://github.com/Abdelilah-99/wisense/actions/workflows/ci.yml)

Detect people moving around your home from the way WiFi signals fluctuate,
using nothing but a laptop's WiFi card, optionally helped by cheap ESP32
boards.

**Only use wisense on networks, devices and premises that you own or are
explicitly allowed to monitor.**

## Contents

1. [What it can and cannot do](#1-what-it-can-and-cannot-do)
2. [Setup on Ubuntu](#2-setup-on-ubuntu)
3. [Commands](#3-commands)
4. [Output](#4-output)
5. [Placement](#5-placement)
6. [Tuning](#6-tuning)
7. [ESP32 boards (CSI)](#7-esp32-boards-csi)
8. [The nl80211 source](#8-the-nl80211-source)
9. [Troubleshooting](#9-troubleshooting)

- [Learn how it works](#learn-how-it-works)
- [Ethics](#ethics)
- [License](#license)

## 1. What it can and cannot do

It can:

- tell you *that* something is moving near the laptop–router path, as yes/no
  plus a score, using only the laptop's WiFi card;
- with ESP32 boards streaming CSI, report which sensor link is the most
  disturbed, e.g. "kitchen" rather than "hallway".

It cannot:

- **locate anyone precisely.** Even with several sensors, the "zone" is just
  the link that is most disturbed. That is not a position.
- **reliably see people who are standing or sitting still.** Motion shows up
  as short-term fluctuation, not as a change in signal level (see
  [§11](docs/theory.md#moving-versus-standing-still)).
- **tell people from other moving things.** Pets, fans, swinging doors,
  curtains in a draft and robot vacuums all trigger it.
- **work equally well everywhere.** Results depend heavily on where the
  laptop, the router and the boards are (see [§5](#5-placement)).

## 2. Setup on Ubuntu

1. Install build tools and `iw`:

   ```sh
   sudo apt install build-essential pkg-config libudev-dev iw
   ```

   `build-essential` provides the C linker that Rust needs. `pkg-config` and
   `libudev-dev` are listed for the `esp32` feature's serial-port library.
   wisense builds that library without its udev support, so they're
   optional, but installing them does no harm. `iw` is used by `list-aps`
   and to find the access point the laptop is associated with; sensing works
   without it.

2. Install Rust with rustup:

   ```sh
   curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
   source "$HOME/.cargo/env"
   ```

   This installs `cargo` (the build tool) and `rustc` (the compiler) for
   your user only; no root needed.

3. Build with the terminal dashboard:

   ```sh
   cargo build --release --features tui
   ```

   This produces `./target/release/wisense`. The optional parts are cargo
   features (see [Rust concepts](docs/rust-concepts.md#cargo-features-and-conditional-compilation)):
   `tui` (dashboard), `esp32` (ESP32 boards) and `nl80211` (nl80211 RSSI
   source). `--all-features` builds all three. A plain
   `cargo build --release` gives the smallest build, with log and JSON output
   only.

4. Check that your WiFi interface is visible:

   ```sh
   ./target/release/wisense list-ifaces
   ```

   This prints each connected wireless interface with its signal level and
   default gateway and, if `iw` is installed, the access point the laptop is
   associated with, e.g.

   ```text
   wlp2s0           signal -52 dBm, link quality 58, status 0x0000, gateway 192.168.1.1
                    associated AP: 192.168.1.1 (BSSID f4:fc:49:04:b2:b9)
   ```

   If it says "not connected", connect to your WiFi first. If the access
   point isn't the gateway, it warns: see [Placement](#5-placement).

5. Calibrate with the room empty:

   ```sh
   ./target/release/wisense calibrate --seconds 60
   ```

   First put the laptop where it will stay; moving it later invalidates the
   baseline. Start the command, then **leave the room** for the full minute,
   taking pets along and switching off fans if you can. The result is saved
   to `~/.config/wisense/baseline.json` together with the settings it was
   made with.

6. Watch:

   ```sh
   ./target/release/wisense monitor --tui
   ```

   Press `q` to quit.

To try the detector without any hardware, run `./target/release/wisense demo`
(add `--tui` for the dashboard). It plays synthetic data with simulated
motion and prints when the motion happens, so you can compare.

## 3. Commands

| Command | What it does |
|---|---|
| `list-ifaces` | Wireless interfaces in `/proc/net/wireless`, with signal, gateway and the access point the laptop is associated with. |
| `list-aps [--iface X]` | Access points the card sees, via `iw scan`. A fresh scan needs root (`sudo`); without it the kernel's cached results are shown. Not used for sensing. |
| `discover [--iface X] [--count 5] [--timeout 1] [--concurrency 64] [--allow-large-subnet] [--json]` | Devices on the WiFi network, ranked as keepalive targets ([below](#finding-devices-on-the-network-discover)). |
| `calibrate [--seconds 60] [--baseline FILE]` | Measures the idle baseline of every link and saves it. |
| `monitor [--baseline FILE] [--json \| --tui]` | Detects motion. |
| `record --out FILE [--seconds N]` | Saves raw samples to CSV (`ts_unix_ms,link_id,rssi_dbm,csi`). |
| `replay FILE [--speed 1.0] [--baseline FILE]` | Runs a recording through the detector. `--speed 0` runs as fast as possible. |
| `demo [--seconds 90] [--speed 1.0] [--seed 1]` | Synthetic data, no hardware needed. |

The same flag groups appear in several commands:

- **Source flags** (`calibrate`, `monitor`, `record`): `--iface`,
  `--rssi-source procfs|nl80211`, `--no-wifi`, `--esp32 PORT=NAME`
  (repeatable), `--baud`, `--poll-hz`, `--ping-interval`, `--no-keepalive`,
  `--keepalive-target IP`.
- **Detector flags** (`calibrate`, `monitor`, `replay`, `demo`):
  `--sensitivity`, `--window-ms`, `--on-ms`, `--off-ms`, `--metric diff|std`.
- **Output flags** (`monitor`, `replay`, `demo`): `--json` or `--tui`.
- **Everywhere**: `-v` for debug logging (`-vv` for even more). Among other
  things it logs how often the RSSI value actually changes.

### Finding devices on the network (`discover`)

**Only run `discover` on networks you own or are allowed to scan.** It pings
every address of the subnet, which is easy to notice and can break the rules
of a network that isn't yours.

`wisense discover` lists the devices on the WiFi interface's subnet, best
keepalive target first. It needs no root:

1. It reads the subnet from `/proc/net/route` (e.g. `192.168.1.0/24`) and
   refuses anything larger than a /22 (1022 addresses) unless you pass
   `--allow-large-subnet`.
2. It pings every address except the laptop's own: `--count` pings each,
   waiting up to `--timeout` seconds per reply, `--concurrency` addresses at
   a time. With the defaults a /24 takes about 20 s.
3. It reads `/proc/net/arp` afterwards, so a device that ignores pings but
   answered ARP still appears, tagged `no ICMP reply`.
4. It ranks the devices by response rate, then jitter (how much the
   round-trip time varies, ping's `mdev`), then median round-trip time.

```text
IP               MAC                     REPLIES     MEDIAN     JITTER  TAGS
192.168.1.1      f4:fc:49:04:b2:b9      5/5 100%     3.9 ms     0.4 ms  gateway, associated AP
192.168.1.20     38:91:48:aa:bb:cc      5/5 100%     5.2 ms     1.1 ms
192.168.1.23     76:62:94:1a:2b:3c      4/5  80%   180.0 ms    95.3 ms  randomized MAC
192.168.1.7      00:42:38:01:02:03      0/5   0%          -          -  no ICMP reply
```

`associated AP` marks the access point the laptop is associated with
(`associated AP (likely)` for a near match; see
[Placement](#5-placement)). `randomized MAC` marks a locally administered
"private" address, as phones use; such devices come and go and sleep.
`--json` prints one object per device instead (`associated_ap` is `null`,
`"exact"` or `"likely"`):

```json
{"rank":1,"ip":"192.168.1.1","mac":"f4:fc:49:04:b2:b9","sent":5,"received":5,"response_rate":1.0,"median_rtt_ms":3.9,"jitter_ms":0.4,"gateway":true,"randomized_mac":false,"no_icmp_reply":false,"associated_ap":"exact","tags":["gateway","associated AP"]}
```

## 4. Output

Events go to **stdout**. Diagnostics (info, warnings, errors) go to
**stderr**, so `wisense monitor --json | some-tool` receives clean JSON.

### Log lines and the live status line

Without `--json`/`--tui`, every event is one line:

```text
16:15:42 wlp2s0 calibrated: idle score 0.570 ± 0.155 dB, threshold 1.190 dB
16:15:50 wlp2s0 MOTION started (score 1.31 dB, thr 1.19)
16:16:01 wlp2s0 motion ended after 8.2 s (peak 3.36 dB)
16:16:20 zone: kitchen (most disturbed link, lead 0.80)
```

On a terminal, the bottom line is redrawn in place twice a second:

```text
16:03:22 wlp2s0 idle   1.128/3.009 dB [###     |       ]
         link   state  score/threshold  bar
```

The bar has 16 cells and the `|` marks the threshold. Each `#` is one eighth
of the threshold, so a full bar is twice the threshold.

### Terminal dashboard (`--tui`)

For each link, the dashboard shows:

- a chart of the score over the last 60 s (cyan) against the threshold
  (red);
- the state: blue = calibrating, green = idle, red = MOTION;
- the current score and threshold.

With two or more links, the header shows the current zone. The bottom pane
lists recent events and log messages. Quit with `q`, `Esc` or Ctrl+C.

### JSON events (`--json`)

One JSON object per line. The `event` field says which kind it is:

| `event` | Fields | When |
|---|---|---|
| `CalibrationStarted` | `link_id, ts_unix_ms, duration_ms` | A link has no usable baseline and starts learning one. |
| `Calibrated` | `link_id, ts_unix_ms, kind, baseline{mean,std}, threshold` | Learning finished; detection is active. |
| `MotionStarted` | `link_id, ts_unix_ms, kind, score, threshold` | MOTION confirmed (after `--on-ms`). |
| `MotionEnded` | `link_id, ts_unix_ms, kind, duration_ms, peak_score` | MOTION over (after `--off-ms`). |
| `Status` | `link_id, ts_unix_ms, kind, score, threshold, state` | Every 500 ms per link. `score` is `null` until the window fills, and `threshold` is `null` while calibrating. |
| `ZoneChanged` | `ts_unix_ms, zone, confidence` | Two or more links only. `zone` is `null` when nothing moves. |

`kind` is `"rssi"` (score in dB) or `"csi"` (score in amplitude², no unit).
`state` is `"calibrating"`, `"idle"` or `"motion"`. Example:

```json
{"event":"MotionStarted","link_id":"wlp2s0","ts_unix_ms":1790000036450,"kind":"rssi","score":0.725,"threshold":0.523}
```

### The baseline file

`~/.config/wisense/baseline.json` holds one entry per link:

```json
{
  "version": 1,
  "links": {
    "wlp2s0": {
      "mean": 0.5700442, "std": 0.15487292,
      "kind": "rssi", "metric": "diff", "window_ms": 2000,
      "poll_hz": 20.0, "ping_interval_s": 0.2,
      "seconds": 60.0, "calibrated_unix_ms": 1793027320246
    }
  }
}
```

`mean` and `std` describe the idle score ([§16](docs/theory.md#16-baseline-threshold-and-false-alarms)).
The other fields record how the baseline was measured. `monitor` refuses an
entry made with a different metric, window or poll rate, because those change
the scale of the score, and says why. A different ping interval only produces
a warning.

## 5. Placement

**Which device is "the router".** In client mode, every frame the laptop
receives comes from the access point it is associated with, so the sensed
radio path runs between the laptop and *that* device. With a mesh system or
a repeater, it is often not the router at the gateway address. `wisense
list-ifaces` shows the associated access point: it matches the access
point's radio address (BSSID, from `iw dev <if> link`) with the MAC
addresses of the devices on the network, exactly or, marked "likely", with
an address one or two values off, as access points often use. When that
device isn't the gateway, `list-ifaces`, `discover`, `calibrate` and
`monitor` print a warning. Below, "the router" means that access point.

The body has to cross the radio path to disturb it ([§11](docs/theory.md#the-fresnel-zone-why-the-body-must-cross-the-path)):

- Put the laptop and the router on **opposite sides** of the area you want
  to watch, so that people walk *between* them.
- Aim the path across the walkway or doorway people actually use. A path
  that runs along a wall nobody walks past sees nothing.
- Keep the laptop still. Moving the laptop is motion too, and it invalidates
  the baseline.
- Fans, pets and moving curtains near the path cause false alarms. Move the
  path away from them if you can.
- With ESP32 boards, give each area its own board, placed so that its path
  to the router crosses that area.

## 6. Tuning

| Flag | Default | Effect |
|---|---|---|
| `--sensitivity` | 4.0 | `k` in `threshold = mean + k × std`. Raise it for fewer false alarms. Lowering it stops helping once the 2 × idle-mean floor applies ([§16](docs/theory.md#the-safety-floors)). |
| `--window-ms` | 2000 | Window the score is computed over. Longer gives a steadier score but reacts more slowly ([§19](docs/theory.md#19-measuring-how-well-it-works)). |
| `--on-ms` | 600 | How long the score must stay above the threshold before MOTION starts. |
| `--off-ms` | 3000 | How long the score must stay below 0.7 × threshold before MOTION ends. |
| `--metric` | diff | `diff` (mean absolute first difference) or `std` (standard deviation). RSSI links only. |
| `--poll-hz` | 20 | How often the laptop's RSSI is read. |
| `--ping-interval` | 0.2 | Seconds between keepalive pings. Older ping versions refuse values below 0.2 s without root; the iputils on the author's laptop allows down to 0.002 s. |
| `--keepalive-target` | gateway | Ping this address instead, e.g. if your router ignores pings. |

After changing `--metric`, `--window-ms` or `--poll-hz`, run `calibrate`
again with the same flags.

### Tuning with record and replay

Walking past the sensor every time you change a flag gets old fast. Instead:

1. Record one session: 20 s quiet, then walking across the path, then quiet:

   ```sh
   ./target/release/wisense record --out walk.csv --seconds 120
   ```

2. Replay it as often as you like with different flags:

   ```sh
   ./target/release/wisense replay walk.csv --speed 0 --baseline ~/.config/wisense/baseline.json
   ./target/release/wisense replay walk.csv --speed 0 --sensitivity 5
   ./target/release/wisense replay walk.csv --speed 0 --json | jq -c 'select(.event != "Status")'
   ```

Without `--baseline`, replay learns a baseline from the first 20 s of the
recording, so start recordings with the room quiet. Write down what you did
when; without that ground truth you can't judge the result
([§19](docs/theory.md#19-measuring-how-well-it-works)).

## 7. ESP32 boards (CSI)

ESP32 boards running [ESP32-CSI-Tool](https://github.com/StevenMHernandez/ESP32-CSI-Tool)
report CSI ([§13](docs/theory.md#13-csi-a-much-richer-view)), many times per second. Each
board becomes one more sensor link, and with two or more links wisense also
reports a zone.

1. **Flash the boards** following the
   [ESP32-CSI-Tool README](https://github.com/StevenMHernandez/ESP32-CSI-Tool#readme).
   Use its `active_sta` project, so that each board connects to your own
   router as a station and measures CSI on the frames it receives from it.
   Set the serial baud rate to 921600 as that README describes.
2. **Build with ESP32 support:** `cargo build --release --features esp32,tui`.
3. **Allow serial access:** `sudo usermod -aG dialout $USER`, then log out
   and back in. Members of the `dialout` group may open `/dev/ttyUSB*` and
   `/dev/ttyACM*`.
4. **Name the boards after where they are.** `--esp32 PORT=NAME` maps a
   serial port to a name, which becomes the link and zone name:

   ```sh
   ./target/release/wisense calibrate --esp32 /dev/ttyUSB0=kitchen --esp32 /dev/ttyUSB1=hallway
   ./target/release/wisense monitor --tui --esp32 /dev/ttyUSB0=kitchen --esp32 /dev/ttyUSB1=hallway
   ```

   The laptop's link is used as well unless you add `--no-wifi`. Use
   `--baud` if your boards use a different speed. Port names can change when
   boards are re-plugged; the paths under `/dev/serial/by-id/` stay the
   same.

## 8. The nl80211 source

`--rssi-source nl80211` (build with `--features nl80211`) reads the laptop's
RSSI through nl80211, the kernel's modern WiFi interface
([Linux and hardware](docs/linux-and-hardware.md#nl80211-and-netlink)), instead of `/proc/net/wireless`. It also
reads the received-packet counter, so it knows when no frames are arriving
instead of guessing from an unchanged value. With `-v` it logs the averaged
signal and the per-antenna signals. It needs no root. Some drivers, such as
Broadcom's proprietary `wl`, don't report station signal this way; wisense
then says so and you can use the default procfs source.

## 9. Troubleshooting

- **"RSSI hasn't changed for 3.0 s"**: the value isn't being refreshed.
  Check that the keepalive ping is running and that your router answers
  pings. If it doesn't, use `--keepalive-target` with another device on your
  network; `wisense discover` lists the ones that answer.
- **"keepalive ping ... exited"**: often `--ping-interval` is below what your
  ping allows without root.
- **Motion never ends, or constant false alarms**: something keeps moving
  near the path, or the baseline was measured while something moved.
  Recalibrate with the room really empty, or raise `--sensitivity`.
- **Walking doesn't trigger**: check placement first; you must cross the
  laptop–router line. Then watch the score while walking (`--tui`), record a
  session and compare settings with `replay`.
- **ESP32: many "malformed CSI lines"**: usually a baud-rate mismatch between
  the board and `--baud`.

## Learn how it works

The [`docs/`](docs/) folder is a study guide: every idea behind wisense, the
code, and the Linux interfaces it talks to. With it you can understand,
change and extend the tool.

- [The ideas behind it](docs/theory.md) (§10–19): radio physics, RSSI and
  CSI, sampling, the motion score, the baseline and threshold, hysteresis,
  zones, and how to measure how well it works.
- [How the code is organized](docs/architecture.md): the pipeline, a tour of
  the files and the design decisions.
- [Rust concepts used in this project](docs/rust-concepts.md), each with where
  to see it in the code.
- [Linux and hardware details](docs/linux-and-hardware.md): the WiFi stack,
  `/proc`, nl80211, ping, signals, terminals and serial ports.
- [Testing and quality](docs/testing.md): the checks and what the tests cover.
- [Learning path and exercises](docs/learning-path.md): an order to read the
  code in, with exercises.
- [Measured on the author's laptop](docs/measurements.md): the numbers quoted
  in these docs.
- [Glossary](docs/glossary.md) of every term.

## Ethics

wisense senses radio signals in the space around your own WiFi. Only run it
on your own network and in your own home, or where everyone affected knows
and agrees. Do not use it to monitor people without their knowledge.

## License

Copyright 2026 Abdelilah Bouchikhi.

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or
  <https://www.apache.org/licenses/LICENSE-2.0>)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or <https://opensource.org/licenses/MIT>)

at your option.

Unless you explicitly state otherwise, any contribution intentionally
submitted for inclusion in the work by you, as defined in the Apache-2.0
license, shall be dual licensed as above, without any additional terms or
conditions.
