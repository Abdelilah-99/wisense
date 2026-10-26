# wisense

Detect people moving around your home from the way WiFi signals fluctuate,
using nothing but a laptop's WiFi card, optionally helped by cheap ESP32
boards.

**Only use wisense on networks, devices and premises that you own or are
explicitly allowed to monitor.**

This README is both the manual and a study guide:

- **Part I** shows how to install and use the tool.
- **Parts II–V** explain every idea behind it: the radio physics, the signal
  processing, the detection statistics, the Rust code, and the Linux
  interfaces it talks to. With them you can understand, change and extend it.
- **Part VI** covers the tests.
- **Part VII** is a suggested learning path with exercises.
- The appendix lists numbers measured on this laptop, and the glossary
  defines every term.

## Contents

- [Part I — Using wisense](#part-i--using-wisense)
  1. [What it can and cannot do](#1-what-it-can-and-cannot-do)
  2. [Setup on Ubuntu](#2-setup-on-ubuntu)
  3. [Commands](#3-commands)
  4. [Output](#4-output)
  5. [Placement](#5-placement)
  6. [Tuning](#6-tuning)
  7. [ESP32 boards (CSI)](#7-esp32-boards-csi)
  8. [The nl80211 source](#8-the-nl80211-source)
  9. [Troubleshooting](#9-troubleshooting)
- [Part II — The ideas behind it](#part-ii--the-ideas-behind-it)
  10. [Radio basics](#10-radio-basics)
  11. [Why a moving body changes the signal](#11-why-a-moving-body-changes-the-signal)
  12. [RSSI in practice](#12-rssi-in-practice)
  13. [CSI: a much richer view](#13-csi-a-much-richer-view)
  14. [Sampling the signal](#14-sampling-the-signal)
  15. [The motion score](#15-the-motion-score)
  16. [Baseline, threshold and false alarms](#16-baseline-threshold-and-false-alarms)
  17. [Hysteresis and debouncing](#17-hysteresis-and-debouncing)
  18. [Zones with several sensors](#18-zones-with-several-sensors)
  19. [Measuring how well it works](#19-measuring-how-well-it-works)
- [Part III — How the code is organized](#part-iii--how-the-code-is-organized)
- [Part IV — Rust concepts used in this project](#part-iv--rust-concepts-used-in-this-project)
- [Part V — Linux and hardware details](#part-v--linux-and-hardware-details)
- [Part VI — Testing and quality](#part-vi--testing-and-quality)
- [Part VII — Learning path and exercises](#part-vii--learning-path-and-exercises)
- [Appendix — Measured on this laptop](#appendix--measured-on-this-laptop)
- [Glossary](#glossary)
- [Ethics](#ethics)

---

# Part I — Using wisense

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
  [§11](#moving-versus-standing-still)).
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
   optional, but installing them does no harm. `iw` is only used by
   `list-aps`.

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
   features (see [Part IV](#cargo-features-and-conditional-compilation)):
   `tui` (dashboard), `esp32` (ESP32 boards) and `nl80211` (nl80211 RSSI
   source). `--all-features` builds all three. A plain
   `cargo build --release` gives the smallest build, with log and JSON output
   only.

4. Check that your WiFi interface is visible:

   ```sh
   ./target/release/wisense list-ifaces
   ```

   This prints each connected wireless interface with its signal level and
   default gateway, e.g.
   `wlp2s0  signal -52 dBm, link quality 58, status 0x0000, gateway 192.168.1.1`.
   If it says "not connected", connect to your WiFi first.

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
| `list-ifaces` | Wireless interfaces in `/proc/net/wireless`, with signal and gateway. |
| `list-aps [--iface X]` | Access points the card sees, via `iw scan`. A fresh scan needs root (`sudo`); without it the kernel's cached results are shown. Not used for sensing. |
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

`mean` and `std` describe the idle score ([§16](#16-baseline-threshold-and-false-alarms)).
The other fields record how the baseline was measured. `monitor` refuses an
entry made with a different metric, window or poll rate, because those change
the scale of the score, and says why. A different ping interval only produces
a warning.

## 5. Placement

The body has to cross the radio path to disturb it ([§11](#the-fresnel-zone-why-the-body-must-cross-the-path)):

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
| `--sensitivity` | 4.0 | `k` in `threshold = mean + k × std`. Raise it for fewer false alarms. Lowering it stops helping once the 2 × idle-mean floor applies ([§16](#the-safety-floors)). |
| `--window-ms` | 2000 | Window the score is computed over. Longer gives a steadier score but reacts more slowly ([§19](#19-measuring-how-well-it-works)). |
| `--on-ms` | 600 | How long the score must stay above the threshold before MOTION starts. |
| `--off-ms` | 3000 | How long the score must stay below 0.7 × threshold before MOTION ends. |
| `--metric` | diff | `diff` (mean absolute first difference) or `std` (standard deviation). RSSI links only. |
| `--poll-hz` | 20 | How often the laptop's RSSI is read. |
| `--ping-interval` | 0.2 | Seconds between keepalive pings. Older ping versions refuse values below 0.2 s without root; the iputils on this laptop allows down to 0.002 s. |
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
([§19](#19-measuring-how-well-it-works)).

## 7. ESP32 boards (CSI)

ESP32 boards running [ESP32-CSI-Tool](https://github.com/StevenMHernandez/ESP32-CSI-Tool)
report CSI ([§13](#13-csi-a-much-richer-view)), many times per second. Each
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
([Part V](#nl80211-and-netlink)), instead of `/proc/net/wireless`. It also
reads the received-packet counter, so it knows when no frames are arriving
instead of guessing from an unchanged value. With `-v` it logs the averaged
signal and the per-antenna signals. It needs no root. Some drivers, such as
Broadcom's proprietary `wl`, don't report station signal this way; wisense
then says so and you can use the default procfs source.

## 9. Troubleshooting

- **"RSSI hasn't changed for 3.0 s"**: the value isn't being refreshed.
  Check that the keepalive ping is running and that your router answers
  pings. If it doesn't, use `--keepalive-target` with another device on your
  network.
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

---

# Part II — The ideas behind it

## 10. Radio basics

### Frequency and wavelength

WiFi uses radio waves around **2.4 GHz** and **5 GHz** (and 6 GHz on newer
gear). A wave's **wavelength** is λ = c / f, the speed of light divided by
the frequency:

| Band | Frequency | Wavelength |
|---|---|---|
| 2.4 GHz | 2.4 × 10⁹ Hz | 12.5 cm |
| 5 GHz (your link: 5320 MHz, channel 64) | 5.32 × 10⁹ Hz | 5.6 cm |

Wavelength matters because it sets the scale of everything that follows.
Moving something by a fraction of a wavelength, a few centimetres, is
already enough to change how the signal adds up at the antenna.

### Decibels and dBm

Radio power covers a huge range, so it is measured on a logarithmic scale.

- **dB** (decibel) compares two powers: +3 dB ≈ twice the power, +10 dB = 10×
  the power, −10 dB = one tenth.
- **dBm** is power relative to 1 milliwatt: P(mW) = 10^(dBm / 10).

| dBm | Power | Typical meaning for WiFi |
|---|---|---|
| −30 | 1 µW | right next to the router |
| −50 | 10 nW | excellent |
| −60 | 1 nW | good |
| −70 | 100 pW | usable |
| −80 | 10 pW | weak |
| −90 | 1 pW | barely connected |

Received WiFi power is tiny, so the numbers are negative. Your link sits
around −48 to −58 dBm. On a log scale a 1 dB change is about a 26% change
in power, and that is the kind of change wisense watches for.

### RSSI

**RSSI** (Received Signal Strength Indicator) is the card's estimate of how
strong a received frame was, reported in dBm. It is **one number per
received frame**, rounded to whole dBm. It's crude, but every WiFi card has
it, which is why wisense starts there.

## 11. Why a moving body changes the signal

### Multipath and fading

The signal from the router doesn't take one path to your laptop. It takes
many at once:

- the direct line of sight (LOS);
- reflections off walls, the floor, furniture, and people.

```text
            wall
   ┌──────────────────────┐
   │   ╲  reflected     ╱  │
   │    ╲              ╱   │
 Router ───── direct ───── Laptop
   │        ╲      ╱       │
   │         person        │
   └──────────────────────┘
```

At the antenna these copies **add up as waves**. Where their peaks line up,
they reinforce; where peaks meet troughs, they cancel. The result depends on
the exact path lengths, compared to the 5.6 cm wavelength. If any path
changes by a couple of centimetres, the sum can swing by several dB. This
is called **fading**.

A human body is mostly water. It absorbs, reflects and blocks 5 GHz waves.
When it moves, it changes path lengths and blocks or unblocks paths, so the
received signal **fluctuates**.

### The Fresnel zone: why the body must cross the path

Most of the energy between two antennas travels inside an elongated
ellipsoid around the straight line between them, called the **first Fresnel
zone**. Its radius at a point that is d1 from one end and d2 from the other
is:

```text
r = √( λ · d1 · d2 / (d1 + d2) )
```

Example: laptop and router 5 m apart on your 5.3 GHz link (λ ≈ 0.056 m):

- at the middle (d1 = d2 = 2.5 m): r = √(0.056 × 2.5 × 2.5 / 5) ≈ **0.26 m**,
  so the ellipsoid is about half a metre thick;
- 0.5 m from the laptop (d1 = 0.5, d2 = 4.5): r ≈ **0.16 m**.

A person (40–50 cm wide) crossing near the middle blocks a large part of it,
and the signal changes a lot. A person walking two metres to the side barely
touches it, so the change is much smaller. This is the physics behind the
placement advice: **people must walk between the laptop and the router.**

### Doppler: how fast the signal changes

When a reflector moves, the reflected path changes length continuously and
the fading pattern sweeps past the antenna. The rate is about f = v / λ:
walking at 1 m/s on 5.3 GHz gives about **18 Hz**, and up to about twice
that for reflections whose path length changes at twice the walking speed.
RSSI sampled 20 times a second can't follow such fast changes exactly, and
it doesn't need to: wisense only asks *how much* the signal is fluctuating,
not what the waveform looks like.

### Moving versus standing still

- A **moving** person keeps changing the paths, so the signal keeps
  fluctuating. This is easy to detect.
- A **still** person changes the paths once, then the new sum stays the same.
  The level may shift by a few dB, but it stops fluctuating, and a small
  level shift is hard to tell from normal drift (sun on a wall, a door left
  open, the router adjusting its power). So wisense treats a still person
  like an empty room.

wisense looks at **short-term fluctuation** (variance), not at the signal
level (mean). Detecting a still person needs much finer measurements, such
as tracking breathing with CSI phase, which is beyond this project.

## 12. RSSI in practice

Real RSSI has four quirks the code has to deal with:

1. **Quantized.** Drivers report whole dBm, so values jump in 1 dB steps.
   Small fluctuations either vanish or appear as a full 1 dB step.
2. **Noisy even when nothing moves.** Frames are sent at different rates and
   power levels, beacons differ from data frames, the card has two antennas,
   and its automatic gain control adjusts all the time. On this laptop an
   empty room still shows about 1.3–1.6 dB of spread between readings.
3. **Only updated when a frame arrives.** The kernel keeps the signal of the
   *last frame received from the router*. If nothing arrives, the value
   freezes and looks perfectly still, even with someone dancing in front of
   it. The router sends beacons about 10 times a second (every 102.4 ms),
   and data frames when traffic flows.
4. **Keepalive traffic keeps it fresh.** wisense runs `ping` against the
   router every 0.2 s. Each reply is a frame from the router, so RSSI gets
   refreshed regularly, whatever else the computer is doing.

Measured on this laptop over 40 s each, with your normal traffic in the
background:

| Keepalive | Frames/s from the router | RSSI changes/s (read at 100 Hz) | Changes/s seen at 20 Hz | Longest time unchanged |
|---|---|---|---|---|
| off | 21 | 7.6 | 6.3 | 0.6 s |
| ping every 0.2 s | 24 | 8.8 | 7.0 | 0.7 s |
| ping every 0.05 s | 43 | 14.2 | 9.1 | 1.0 s |

Notice that RSSI changes less often than frames arrive: consecutive frames
often round to the same whole dBm. Notice also that 20 readings per second
miss some changes when frames come faster ([§14](#14-sampling-the-signal)).

## 13. CSI: a much richer view

### OFDM and subcarriers

Modern WiFi uses **OFDM**: a 20 MHz channel is split into **64 narrow
subcarriers** 312.5 kHz apart, each carrying part of the data. A few at the
edges (guard subcarriers) and one at the centre (DC) carry nothing. Every
subcarrier sees slightly different fading, because the path-length
differences translate into different phase shifts at slightly different
frequencies.

### What CSI is

**Channel State Information** describes, for each subcarrier, how the channel
changed the known training symbols at the start of a frame. For each
subcarrier it's a **complex number** `re + j·im`:

- its **amplitude** `√(re² + im²)` says how strongly that subcarrier got
  through;
- its **phase** `atan2(im, re)` says how much it was delayed.

So where RSSI gives one number per frame, CSI gives 64 amplitudes (and
phases), a "fingerprint" of the channel across frequency.

### How the ESP32 reports it

ESP32-CSI-Tool prints one line per frame:

```text
CSI_DATA,STA,AA:BB:CC:DD:EE:FF,-57,11,1,7,0,1,1,1,0,0,0,-93,0,6,0,3517632,0,48,0,0,3517.632,128,[3 4 0 0 -6 8 ...]
```

That's 25 comma-separated fields (field 4, index 3, is the RSSI), then the
raw CSI buffer in brackets. The buffer holds signed bytes in **(imaginary,
real) pairs**. wisense checked this against the firmware source
(`_components/csi_component.h`) and its `python_utils/parse_csi.py`. So
`[3 4 0 0 -6 8]` means amplitudes 5, 0 and 10. Guard and null subcarriers
come out as zeros.

### Why amplitudes only

Cheap radios don't share a clock with the router. Every frame's phase is
shifted by random offsets (carrier frequency offset, sampling offset, packet
detection delay). Cleaning that up is a research topic of its own, so
wisense uses amplitudes, which don't have this problem.

### Why CSI beats RSSI

A body moving through the room changes some subcarriers a lot and others
hardly at all. With 50+ independent views you're much more likely to catch
the change than with one rounded number. That's why wisense's CSI score
looks at the **most variant quarter** of the subcarriers
([§15](#the-csi-score)).

## 14. Sampling the signal

wisense reads RSSI from `/proc/net/wireless` at a fixed **poll rate**
(20 Hz by default).

- **Zero-order hold.** Between frames, the value just stays the same. Polling
  turns that step-shaped signal into an evenly spaced series of readings.
- **Emit every poll.** A sample is produced on every poll, even if the value
  didn't change. `/proc/net/wireless` has no per-frame counter, so "same
  value" can't be told apart from "no new frame". Throwing away repeats
  would discard real "nothing changed" readings and inflate the scores.
  Even spacing also keeps calibration and monitoring statistically
  identical, provided the poll rate stays the same. That's why the baseline
  file records `poll_hz`.
- **Staleness is tracked separately.** If the value hasn't changed for 3 s,
  wisense warns ([§9](#9-troubleshooting)).
- **Missed changes.** When frames arrive faster than the poll rate, some
  changes happen and revert between two polls and are never seen (the table
  in [§12](#12-rssi-in-practice): 14.2 → 9.1 changes/s). That makes the
  score a little less sensitive, but consistently so, because calibration
  sees the same thing.

ESP32 boards are not polled. Every received frame produces a sample.

## 15. The motion score

### Sliding window

For each link, wisense keeps the samples of the last **2 seconds** in a
queue (a `VecDeque`). New samples go in at the back and samples older than
the window fall out at the front. After every new sample, it computes a
**score** from what's in the window. At 20 Hz a 2 s window holds 41 readings,
so 40 steps between them.

### Mean absolute first difference (`--metric diff`, the default)

```text
score = ( |x₁ − x₀| + |x₂ − x₁| + … + |xₙ − xₙ₋₁| ) / n
```

It's the average size of a step between consecutive readings. Example with
six readings:

```text
readings: -54  -54  -56  -55  -55  -58
steps:        0    2    1    0    3      → (0+2+1+0+3) / 5 = 1.2 dB
```

### Standard deviation (`--metric std`)

```text
mean  = (x₀ + … + xₙ) / (n+1)
score = √( average of (xᵢ − mean)² )
```

For the same six readings: mean −55.33, std ≈ **1.37 dB**.

### Why diff is the default

Compare them on a **slow drift**: the level slides from −50 to −52 dBm over
the 2 s window with no noise at all.

- std ≈ **0.58 dB** (a straight ramp of height h has a std of h / √12);
- diff = 2 dB total / 40 steps = **0.05 dB**.

Slow level changes happen all the time and mean nothing: a door opens across
the house, the router changes its transmit power, someone sits down and
stays still. std reacts to them; diff hardly does. Diff reacts to *fast*
changes, which is what motion produces. Rounding to whole dB also hurts std
more: a single 1 dB step halfway through the window gives std 0.5 dB, but
diff only 1/40 = 0.025 dB.

On your own recording, diff separated the busy phase from the quiet phase
and std couldn't ([§19](#19-measuring-how-well-it-works)).

### Relation to noise

If readings were independent noise with standard deviation σ, the average
step would be E|xᵢ − xᵢ₋₁| = 2σ/√π ≈ **1.13 σ**. Real readings repeat between
frames, so many steps are zero and the diff score is lower. On this laptop,
1.13 × 1.5 dB ≈ 1.7, times the ~40% of polls that see a new value, gives
≈ 0.7 dB. That matches the measured idle scores of 0.55–0.9 dB, and it shows
why the poll rate and ping interval must be the same for calibration and
monitoring.

### The CSI score

For CSI links, for each subcarrier, wisense computes the **variance** of its
amplitude over the window. It skips subcarriers that are zero throughout
(guard and null ones), then averages the **top 25%** of the variances. Motion
usually affects some subcarriers much more than others, and averaging all of
them would dilute it. The unit is amplitude² and has no physical meaning on
its own; only the comparison with the baseline matters. Only CSI rows of the
same length as the newest one are compared, because the subcarrier count can
change with the frame type.

### Warm-up

A score from a handful of samples is unreliable. A window yields a score
only once its oldest sample is at least **75% of the window length** old
(1.5 s for a 2 s window). Until then, `Status` reports `score: null`. The
same rule applies after a gap in the data.

## 16. Baseline, threshold and false alarms

### Calibration

wisense can't know in advance what "quiet" looks like in your room: every
room, laptop position and router differs. So it measures it. During
calibration, every score is collected, and at the end two numbers summarize
them:

- **mean**: the typical idle score (yours: 0.570 dB);
- **std**: how much the idle score itself wanders (yours: 0.155 dB).

They're computed in one pass with **Welford's algorithm**, which is
numerically stable:

```text
for each score x:
    n    ← n + 1
    δ    ← x − mean
    mean ← mean + δ / n
    M2   ← M2 + δ · (x − mean)
variance = M2 / n,   std = √variance
```

### The threshold

```text
threshold = mean + k × std        (k = --sensitivity, default 4)
```

This is a **z-score** rule: "fire when the score is more than k standard
deviations above normal". Your baseline gives 0.570 + 4 × 0.155 = **1.19 dB**.

### False alarms versus missed motion

Every threshold trades two kinds of mistake:

- **too low** gives false alarms: normal noise crosses it now and then;
- **too high** gives missed motion: gentle or distant movement never
  reaches it.

If idle scores were independent and bell-shaped (Gaussian), exceeding
mean + 4σ would happen about 3 times in 100,000. Real scores are worse:

- consecutive scores come from overlapping windows, so they're **strongly
  correlated**, and a spike lasts a whole window;
- the distribution has a **heavier upper tail** than a bell curve.

On synthetic noise ([`src/source/synthetic.rs`](src/source/synthetic.rs)),
k = 4 with the diff metric gave about **1 false start per 100 minutes** after
a 60 s calibration, and about 1 per 33 minutes after a 20 s one. k = 5 gave
none. The `--on-ms` wait ([§17](#17-hysteresis-and-debouncing)) removes many
short spikes.

### Why calibrate for 60 seconds

Scores from overlapping 2 s windows aren't independent. 20 s of calibration
contains only about **10 independent windows**, so the estimated mean and
std can be off by chance. 60 s gives about 30, which is much more reliable.
That's why `calibrate` defaults to 60 s, and why the 20 s auto-calibration
warns you loudly.

### The safety floors

The threshold is also kept above two floors:

```text
threshold = max( mean + k × std,   2 × mean,   0.05 )
```

- **2 × mean.** MOTION ends when the score drops below **0.7 × threshold**
  ([§17](#17-hysteresis-and-debouncing)). If the idle score barely varies
  (tiny std), mean + 4·std could be just 1.3 × mean. 0.7 × 1.3 = 0.91 × mean
  would put the "end" level *below* normal idle scores, and motion would
  never end. A test with CSI data caught exactly this. With the floor, the
  end level is at least 1.4 × mean.
- **0.05.** A perfectly flat calibration (mean = std = 0) would otherwise
  give threshold 0, and the first tiny change would fire.

The consequence for tuning: with your baseline, mean + k·std drops below
2 × mean (1.14) when k is below about 3.7. Lowering `--sensitivity` further
has no effect.

### Auto-calibration

Without a usable saved baseline, a link learns one from its first 20 s and
prints a loud warning to keep still. It needs at least 10 scores to finish.

## 17. Hysteresis and debouncing

A score hovering around the threshold would switch MOTION on and off many
times a second. Two classic tricks prevent that.

**Hysteresis**, the same idea as a thermostat or a Schmitt trigger, uses two
levels instead of one:

```text
                 score > threshold          for on-ms  (600 ms)
        IDLE  ─────────────────────────────────────────────▶  MOTION
          ▲                                                      │
          └──────────────────────────────────────────────────────┘
                 score < 0.7 × threshold    for off-ms (3000 ms)
```

Between 0.7 × threshold and the threshold there's a **dead band**: nothing
changes there, whichever state you're in. With your baseline, MOTION starts
above **1.19** and ends below **0.83**.

**Debouncing** adds the timers: a condition must hold *continuously* for
`on-ms` or `off-ms`. One dip resets the clock.

A typical walk across the path:

```text
t = 0.0 s   you start walking between laptop and router
t ≈ 0.3 s   enough big steps in the 2 s window → score passes the threshold
t ≈ 0.9 s   still above after 600 ms          → "MOTION started"
t = 5.0 s   you stop
t ≈ 7.0 s   your movement has left the window → score drops below 0.83
t ≈ 10.0 s  still below after 3 s             → "motion ended after 6.7 s"
```

The reported duration runs from the first crossing (0.3 s) to the drop below
the end level (7.0 s). It leaves out the two timer waits, but it does include
the time your movement takes to leave the window.

## 18. Zones with several sensors

With two or more links (the laptop plus ESP32 boards):

1. **Normalize** each link: `score / threshold`. A ratio of 1.0 means "at its
   own threshold", which makes links with different units and noise levels
   comparable.
2. While **any** link is in MOTION, the zone is the link with the highest
   ratio.
3. **Hold time**: a different link must stay on top for **1 s** before the
   zone switches, so two similar links don't ping-pong. Entering and leaving
   "no zone" follows MOTION immediately, because MOTION is already debounced.
4. **Confidence** is the top ratio minus the runner-up's: how clearly one link
   wins.

This answers "which sensor path is most disturbed?", which roughly follows
where someone is moving. It's not positioning: a person standing between
two paths, or a large room with one board, will give misleading zones.

## 19. Measuring how well it works

### Ground truth

To judge a detector you need to know **what really happened when**: note the
times you walked, sat still or left. Then compare with the events. Without
ground truth, every result is a guess.

### Separation

A useful single number is the separation between two situations A and B
(say "walking" and "still"):

```text
d' = (mean score in A − mean score in B) / pooled standard deviation
```

Roughly: d' below 1 means the score distributions overlap heavily; around 2
is usable; above 3 separates cleanly.

### Worked example: your `walk.csv`

Your 120 s recording has three clearly different phases. The diff score with
a 2 s window, against your calibration's idle score of 0.57:

| Phase | Mean score | Detector with your baseline (threshold 1.19) |
|---|---|---|
| 0–18 s | 1.94 dB | MOTION from +3.4 s |
| 19–86 s | 0.93 dB | stayed in MOTION (hovered near the threshold) |
| 87–120 s | 0.73 dB | idle |

How well each setting separates the middle phase from the quiet end:

| Window | d' with `diff` | d' with `std` |
|---|---|---|
| 1 s | 0.49 | 0.23 |
| 2 s | 0.74 | 0.19 |
| 4 s | 1.25 | 0.10 |
| 6 s | 1.60 | −0.03 |

Lessons:

- `std` can't tell these phases apart at all on your link; `diff` can.
- **Longer windows separate better.** Averaging over more steps makes the
  score steadier. The price: the score reacts later (the window must fill
  with motion) and MOTION lasts longer after motion stops (it takes a full
  window to leave).
- Which phase was the walking decides what to tune, which is why ground
  truth matters.

---

# Part III — How the code is organized

## Architecture

```text
 +----------------+ +----------------+ +----------------+ +----------------+ +-----------------+
 | ProcfsSource   | | Nl80211Source  | | Esp32Source    | | ReplaySource   | | SyntheticSource |
 | /proc, 20 Hz   | | netlink, 20 Hz | | serial thread  | | CSV file       | | fake data       |
 +-------+--------+ +-------+--------+ +-------+--------+ +-------+--------+ +--------+--------+
         |  Sample          |                  |                  |                   |
         +------------------+---------+--------+------------------+-------------------+
                                      v   one bounded tokio mpsc channel (4096)
                              +---------------+
   Ctrl+C / SIGTERM / SIGHUP->| run_pipeline  |<- 100 ms tick (TUI keys + redraw, deadlines)
                              +-------+-------+
                                      v  Handler
              +-----------------------+-------------------------+
              | DetectHandler                                   |  RecordHandler -> CSV file
              |   Detector --Event--> ZoneTracker --ZoneChanged |
              |       |                                         |
              |       v                                         |
              |   EventSink: HumanSink | JsonSink | TuiSink     |
              +-------------------------------------------------+

 Keepalive: a supervised `ping` child -> frames from the router -> fresh RSSI
```

The flow:

1. Each **source** runs in its own task and produces `Sample`s (link, time,
   RSSI, optional CSI amplitudes). All of them push into **one channel**.
2. `run_pipeline` (in `main.rs`) receives samples and gives them to a
   **handler**. It also ticks every 100 ms and watches for Ctrl+C, SIGTERM
   and SIGHUP. If a source fails, it reports it and keeps the others running.
3. The **detection handler** feeds each sample to the `Detector`. The
   detector returns `Event`s; the `ZoneTracker` turns `Status` events into
   `ZoneChanged` events; an **event sink** displays them all.
4. The **record handler** writes samples to CSV instead.

## A tour of the files

| File | Lines | Responsibility |
|---|---|---|
| [`src/main.rs`](src/main.rs) | ~1050 | CLI (clap), opening sources, baselines, the pipeline, handlers, shutdown. |
| [`src/config.rs`](src/config.rs) | ~330 | All defaults, `DetectorConfig`, `Metric`, the baseline file (load, save, compatibility checks). |
| [`src/sample.rs`](src/sample.rs) | ~40 | The `Sample` type every source produces. |
| [`src/detector.rs`](src/detector.rs) | ~1040 | The core: windows, scores, calibration, thresholds, hysteresis, events. Pure, no I/O. |
| [`src/zones.rs`](src/zones.rs) | ~200 | Zone selection with hold time. Pure. |
| [`src/traffic.rs`](src/traffic.rs) | ~260 | Default gateway from `/proc/net/route`; the supervised `ping` keepalive. |
| [`src/output.rs`](src/output.rs) | ~550 | Event sinks (human, JSON), the live status line, log capture. |
| [`src/output/tui.rs`](src/output/tui.rs) | ~320 | The ratatui dashboard (feature `tui`). |
| [`src/record.rs`](src/record.rs) | ~190 | CSV row format and the recorder; generator of the test fixture. |
| [`src/scan.rs`](src/scan.rs) | ~130 | `list-aps`: runs `iw` and parses its output. |
| [`src/source/mod.rs`](src/source/mod.rs) | ~40 | The `SampleSource` trait and the task that drives a source. |
| [`src/source/procfs.rs`](src/source/procfs.rs) | ~570 | `/proc/net/wireless` parsing, interface selection, staleness tracking, the poller. |
| [`src/source/nl80211.rs`](src/source/nl80211.rs) | ~290 | nl80211 station info (feature `nl80211`). |
| [`src/source/esp32.rs`](src/source/esp32.rs) | ~340 | ESP32-CSI-Tool line parser and serial reader (feature `esp32`). |
| [`src/source/replay.rs`](src/source/replay.rs) | ~120 | Plays back a CSV recording with its original timing. |
| [`src/source/synthetic.rs`](src/source/synthetic.rs) | ~350 | Realistic fake RSSI for `demo` and tests; a small random number generator. |
| [`tests/replay.rs`](tests/replay.rs) | ~130 | End-to-end tests running the real binary on a fixture. |

### Details per file

- **`detector.rs`**:
  - Per link it keeps a `Link` with a `VecDeque<Entry>` window, a `Phase`
    (`Calibrating` with a `RunningStats`, or `Active` with a threshold and a
    `Hysteresis`), and the time of the last `Status`.
  - `Detector::process(sample) -> Vec<Event>` is the only entry point.
  - Scores come from `mean_abs_diff`, `std_dev` or `csi_score`.
  - `Hysteresis::update(score, threshold, now)` is a small state machine
    returning `Transition::Started` or `Ended`.
  - The tests drive it with exact time sequences.
- **`procfs.rs`**:
  - `parse_line` and `lookup` turn text into a `WirelessEntry`.
  - `WirelessEntry::signal()` classifies the level as dBm (negative), a
    relative scale (positive), or no data (0, or −256 and below).
  - `StalenessTracker` notices values that stop changing.
  - `ProcfsSource::next` waits for the next tick, reads the file and returns
    a sample, logging the value-change rate every 10 s at debug level.
- **`traffic.rs`**:
  - `parse_route_addr` turns `0101A8C0` into `192.168.1.1`.
  - `default_routes` and `gateway_for` pick the gateway of the WiFi
    interface.
  - `Keepalive::start` spawns `ping -i 0.2 -q -n <gateway>` and a supervisor
    task that reports unexpected exits; `stop()` kills it.
- **`config.rs`**: `BaselineEntry::check(&LinkSetup)` decides whether a
  saved baseline fits the current settings. `BaselineFile::save` writes to a
  temporary file and then renames it, so a crash never leaves a half-written
  file.
- **`output.rs`**:
  - `HumanSink` prints event lines and redraws the live line; `JsonSink`
    prints one JSON object per line.
  - `LogWriter` is plugged into tracing so log messages first clear the live
    line, or are captured for the TUI.
- **`esp32.rs`**:
  - `parse_csi_line` never panics, whatever the input.
  - `LineSplitter` reassembles lines from partial serial reads and drops
    absurdly long garbage.
  - A dedicated thread reads the port and forwards samples.
- **`zones.rs`**: `ZoneTracker::observe(&Event) -> Option<Event>`
  implements [§18](#18-zones-with-several-sensors).
- **`synthetic.rs`**:
  - `SyntheticSignal` produces quantized Gaussian noise around −55 dBm,
    values held between "frames" (8 per second), a slow sine drift, and
    motion **bursts**.
  - A burst is a correlated random process (Ornstein–Uhlenbeck, 0.35 s
    correlation time) that fades in and out over 0.3 s.
  - `Rng` is xorshift64*, seeded through splitmix64, with Box–Muller for
    normal values.

## Design decisions worth knowing

- **Pure core, imperative shell.** `detector.rs` and `zones.rs` never read a
  clock or touch files: time comes from the samples. The same code runs
  live, on replays at any speed, and in tests with exact timings. All I/O
  lives at the edges (sources, sinks, `main.rs`).
- **Emit every poll** ([§14](#14-sampling-the-signal)).
- **stdout for events, stderr for diagnostics**, so output can be piped.
- **Baselines carry their settings**, so a stale baseline can't silently
  give nonsense thresholds.
- **Optional features off by default**, so the basic build stays small and
  needs no system libraries.
- **No `unsafe`, no `unwrap()`/`expect()` outside tests.** Every failure is a
  `Result` with context.
- **The keepalive `ping` stays in wisense's process group**, so a Ctrl+C or a
  closed terminal reaches it too, even if wisense dies abruptly. wisense also
  kills it on every normal exit path ([Part V](#processes-and-signals)).

---

# Part IV — Rust concepts used in this project

Each concept below comes with where to see it in the code.

### Ownership and borrowing

- `Detector::process(&mut self, sample: Sample)` takes the sample **by
  value**, so its CSI vector moves into the window without being copied.
- `Hysteresis::update` matches on `&mut self.state`, copies out what it
  needs, then replaces `self.state`. This is legal because the borrow ends
  after its last use (non-lexical lifetimes).
- In `output/tui.rs`, `terminal.draw(...)` returns a value that *borrows*
  the terminal. The code drops it with `.map(|_| ())` before moving the
  terminal back into the struct. The compiler error that forced this
  (E0505) is a good one to understand.

### Enums and pattern matching

- `Event`, `MotionState`, `Phase`, `HystState`, `Transition`, `Signal` and
  `Staleness` make every state explicit.
- Note `let … else` (`let Some(x) = … else { return … };`), match guards
  (`Some(s) if *t > 0.0`), `matches!`, and binding fields directly in
  patterns (`Event::Status { link_id, score, .. }`).

### Option, Result and `?`

- `?` returns early on error.
- Useful helpers in the code: `is_some_and`, `is_none_or`, `then_some`,
  `ok_or_else`, `get_or_insert`, `transpose`.
- `Esp32Source::next` turns `Option<Result<Sample>>` into
  `Result<Option<Sample>>` with `.transpose()`.

### Error handling: anyhow and thiserror

- **`anyhow`** is for application errors. `.context("…")` and
  `.with_context(|| …)` add a readable chain, e.g. "reading
  /proc/net/wireless: No such file or directory".
- **`thiserror`** is for typed errors that callers and tests match on:
  `WirelessError`, `CsiParseError`, `RouteError`. The tests compare them
  with `assert_eq!`.

### Traits, generics and trait objects

- **Traits**: `SampleSource` (async), `EventSink` (with a default method
  `tick`), `Handler` (in `main.rs`).
- **Trait objects**: `Vec<Box<dyn SampleSource>>` mixes different source
  types in one list; `&mut dyn EventSink` picks the output at run time.
- **Generics**:
  - `HumanSink<W: Write>` and `JsonSink<W>` write to stdout in real use and
    to a `Vec<u8>` in tests;
  - `StalenessTracker<T = f32>` has a *default type parameter* and is used
    with `u32` packet counters in `nl80211.rs`;
  - `mean_abs_diff(values: impl IntoIterator<Item = f32>)` accepts any
    iterator.

### async/await with Tokio

- `#[tokio::main]` starts the runtime.
- **Tasks** run concurrently: `tokio::spawn`, and `JoinSet` to keep and await
  a group of them (`run_pipeline`).
- **Channels**:
  - `mpsc::channel(4096)` is *bounded*. When the consumer is slow, senders
    wait, so memory can't grow without limit (backpressure).
  - `oneshot` tells the ping supervisor to stop.
- **`tokio::select!`** waits on several things at once and runs whichever
  is ready first. `run_pipeline` waits on samples, finished tasks, the tick
  and shutdown signals. `tokio::pin!` lets the same future be polled again
  on each loop.
- **Timers**:
  - `interval` with `MissedTickBehavior::Skip`, so a stalled poller doesn't
    burst to catch up;
  - `sleep_until` for replay pacing.
- **Signals**: `tokio::signal::ctrl_c()` and
  `signal(SignalKind::terminate())`.
- **Processes**: `tokio::process::Command` with `kill_on_drop(true)`.

### Why the `async-trait` crate

An `async fn` in a trait returns a future whose type and size differ per
implementation. That's fine with generics, but `dyn SampleSource` needs one
known type. `#[async_trait]` rewrites the method to return
`Pin<Box<dyn Future + Send>>`, a boxed future of known size, so trait
objects work.

### Mixing blocking code with async

The serial port API blocks, and blocking inside an async task would freeze
other tasks. So `esp32.rs` reads the port in a **dedicated `std::thread`**
and hands samples over with `blocking_send`. That call is meant for
non-async threads and must never be used inside async code. The procfs
source reads with `tokio::fs`.

### Cargo features and conditional compilation

- `Cargo.toml` declares `nl80211`, `esp32` and `tui`, each enabling optional
  dependencies with the `dep:` syntax.
- `#[cfg(feature = "tui")]` compiles a module or function only when the
  feature is on.
- `main.rs` has *pairs* of functions, the real one and a fallback that
  returns a helpful error ("rebuild with --features esp32"). So the CLI is
  identical in every build.
- Also used: `#[cfg(test)]`, and `#[cfg(target_endian = "little")]` on
  byte-order tests.

### Command-line parsing with clap (derive)

- `#[derive(Parser, Subcommand, Args, ValueEnum)]` turns structs and enums
  into a CLI. Doc comments become the help text.
- `#[command(flatten)]` reuses flag groups (`SourceArgs`, `DetectorArgs`,
  `OutputArgs`).
- `default_value_t = config::DEFAULT_…` keeps defaults in one place.
- `value_parser = parse_esp32_spec` does custom validation, and
  `conflicts_with` / `requires` express rules between flags.
- The test `Cli::command().debug_assert()` checks the whole definition.

### Serialization with serde (JSON and CSV)

- `#[derive(Serialize, Deserialize)]` on events, baselines and CSV rows.
- `#[serde(tag = "event")]` produces internally tagged JSON
  (`{"event":"MotionStarted",…}`).
- `#[serde(rename_all = "lowercase")]` gives `"motion"`, `"rssi"`.
- `#[serde(default, skip_serializing_if = "Option::is_none")]` handles
  optional baseline fields.
- The same `Row` struct is written by `Recorder` and read by `ReplaySource`
  through the `csv` crate.

### Logging with tracing

- `tracing::info!`, `warn!`, `debug!` emit events; `init_tracing` installs a
  subscriber with a level picked by `-v`.
- `output::LogWriter` implements `MakeWriter`, deciding per message where
  the text goes (stderr, or the TUI's buffer).

### Global state without `unsafe`

- `static LIVE_LINE: AtomicBool` and
  `static LOG_BUFFER: Mutex<VecDeque<String>> = Mutex::new(VecDeque::new())`
  are shared safely through atomics and a mutex with `const`
  initialization.
- A poisoned mutex is recovered with `.unwrap_or_else(|e| e.into_inner())`.
- `LogTarget` implements `Drop` to store a captured message when tracing is
  done with it.

### Time

- **`Instant`** is a monotonic clock that never jumps. It's used for
  windows, timers and durations.
- **`SystemTime`** is the wall clock, which can jump when NTP corrects it.
  It's used for timestamps shown to people and written to files.
- `Instant::duration_since` saturates at zero instead of panicking when
  times are out of order.

### Numeric care

`saturating_sub`; `u64::try_from(…).unwrap_or(u64::MAX)` instead of lossy
`as`; `f32::total_cmp` to sort floats (which aren't totally ordered because
of NaN); `f64` accumulators for precision.

### Testing techniques

- Unit tests live next to the code in `#[cfg(test)] mod tests`.
  `#[tokio::test]` handles async tests.
- Integration tests in `tests/` run the **real binary** through
  `env!("CARGO_BIN_EXE_wisense")` and parse its JSON output.
- Fixture paths are built with `concat!(env!("CARGO_MANIFEST_DIR"), …)`.
- An `#[ignore]` test regenerates the fixture on demand.
- A deterministic random number generator keeps synthetic tests reproducible
  without the `rand` crate.
- A tiny fuzz test (`never_panics_on_garbage`) throws 2000 corrupted lines at
  the ESP32 parser.

### Tooling

- `cargo fmt` formats the code.
- `cargo clippy --all-targets -- -D warnings` lints it. It caught, for
  example, `x % n == 0`, which it wants written as `x.is_multiple_of(n)`.
- `cargo test --all-features` runs the tests.

---

# Part V — Linux and hardware details

## The WiFi stack

```text
  wisense ── /proc/net/wireless (legacy "wireless extensions") ──┐
  wisense ── nl80211 over generic netlink (modern API) ──────────┤
                                                                  v
                                cfg80211   kernel WiFi configuration layer
                                    |
                                mac80211   shared "soft MAC" used by most drivers
                                    |
                                rtw88_8822ce   driver for your Realtek RTL8822CE card
```

Your card is a **Realtek RTL8822CE** on the in-kernel **rtw88** driver, which
is built on mac80211. Broadcom's proprietary `wl` driver doesn't use
mac80211, which is why it often exposes less information.

## `/proc/net/wireless`

```text
Inter-| sta-|   Quality        |   Discarded packets               | Missed | WE
 face | tus | link level noise |  nwid  crypt   frag  retry   misc | beacon | 22
wlp2s0: 0000   62.  -48.  -256        0      0      0      0     32        0
```

- Two header lines, then one line per *connected* wireless interface. A
  disconnected interface is simply missing.
- Columns: status (hex), link quality, **signal level**, noise level,
  discarded-packet counters, missed beacons.
- A trailing `.` means "updated" and is stripped before parsing.
- The signal is in dBm when negative. Some drivers report a positive
  0–100/0–255 scale instead; wisense warns and still uses it, since
  fluctuations don't care about units.
- Noise `-256` means "not reported".
- This file is provided by cfg80211's compatibility layer. On mac80211
  drivers the signal is that of the last frame from the router.

## `/proc/net/route` and byte order

```text
Iface   Destination  Gateway   Flags  RefCnt  Use  Metric  Mask      ...
wlp2s0  00000000     0101A8C0  0003   0       0    600     00000000
```

- A **default route** has destination and mask `00000000`.
- Flags `0003` means `RTF_UP | RTF_GATEWAY`.
- The kernel prints the address's raw 32-bit value as hex. On a
  **little-endian** CPU (x86, most ARM) the bytes come out reversed:
  `0101A8C0` → bytes `C0 A8 01 01` → **192.168.1.1**.
- The code parses the hex into a `u32` and takes its *native-endian* bytes,
  which gives the right answer on any CPU.
- It picks the gateway **of the WiFi interface**: pinging a gateway reached
  over Ethernet would create no WiFi frames at all.

## nl80211 and netlink

- **Netlink** is how user programs exchange messages with the kernel through
  a socket. **Generic netlink** hosts named "families"; WiFi's is
  `nl80211`.
- wisense sends `NL80211_CMD_GET_STATION` with the *dump* flag for the
  interface. In client mode, the only "station" is the router.
- Replies are lists of **attributes** in TLV form (type, length, value,
  padded to 4 bytes). Some attributes are **nested** lists themselves.
- `STA_INFO` contains `SIGNAL`, `SIGNAL_AVG`, `CHAIN_SIGNAL` (one value per
  antenna) and `RX_PACKETS`, among others.
- Reading station info needs no root.
- **A real bug found on this laptop**: the `wl-nl80211` crate (0.8) returns
  `CHAIN_SIGNAL` as the raw bytes of its nested attributes,
  `[5,0,0,0,-56,0,0,0, 5,0,1,0,-54,0,0,0]`. That's two attributes: length 5,
  type 0 and 1, carrying −56 and −54 dBm.
  `nl80211::decode_chain_signal` decodes these, and leaves plain values
  alone if a future crate version fixes it.

## ping, ICMP and root

- `ping` sends ICMP *echo request* packets; the router answers with *echo
  replies*, and each reply refreshes RSSI.
- wisense runs `ping -i <interval> -q -n <gateway>`:
  - `-i` sets the interval;
  - `-q` stays quiet;
  - `-n` uses no DNS.
- Older ping versions refuse intervals below 0.2 s without root. The iputils
  on this laptop (20240905) allows down to 2 ms.
- If your router ignores pings, use `--keepalive-target` with another device
  on the network: traffic to it still passes through the router.

## Processes and signals

- **Ctrl+C** makes the terminal send **SIGINT** to the whole *foreground
  process group*, so wisense *and* its `ping` child receive it. wisense
  treats a ping exit with code 0/1 (its normal reaction to SIGINT) as a
  clean stop, and any other exit as a warning.
- **SIGTERM** (from `kill`) and **SIGHUP** (terminal closed) also trigger a
  clean shutdown that kills `ping`.
- **SIGKILL** can't be caught; `ping` could then outlive wisense.
  `kill_on_drop` covers panics and early returns.
- **SIGTTOU**: a program in a *background* process group that tries to
  change terminal settings gets stopped by the kernel. We hit this while
  testing the TUI under `timeout`, which puts its child in the background:
  the process just froze in state `T`. `timeout --foreground` fixes it.

## Terminals

- **Is it a terminal?** `std::io::IsTerminal` decides whether to draw the
  live line, use colours, or allow the TUI.
- **ANSI escape codes** control the terminal: `\r` returns to the line start,
  `\x1b[2K` erases the line, `\x1b[33m` sets a colour. The live line redraws
  itself with `\r\x1b[2K`. Captured logs are stripped of colour codes for
  the TUI.
- **Raw mode** turns off line buffering, echo and signal keys, so Ctrl+C
  arrives as an ordinary key, which the TUI handles.
- The **alternate screen** (`\x1b[?1049h` and `\x1b[?1049l`) is a separate
  screen the TUI draws on, so your scrollback is untouched afterwards.
- The terminal must be **restored** however the program ends.
  `ratatui::try_init` installs a panic hook that restores it before the
  panic message is printed, and `TuiSink` restores it on exit and in `Drop`.

## Serial ports

- An ESP32 talks over a **UART**: bytes framed by a fixed **baud rate**,
  which both sides must agree on. 921600 baud is roughly 92 KB/s.
- USB-serial chips show up as `/dev/ttyUSB0` or `/dev/ttyACM0`. The
  `dialout` group may open them, and `/dev/serial/by-id/…` gives stable
  names.
- Data arrives in arbitrary chunks, not lines, so lines must be reassembled.
  Boards also print boot messages and log lines that must be skipped, and a
  wrong baud rate produces garbage.

## Scanning (`iw scan`)

An active scan makes the card hop across channels and listen. It needs
`CAP_NET_ADMIN` (root), takes seconds, and disturbs the connection, so it's
never used for sensing. `iw … scan dump` shows the kernel's cached results
without root.

---

# Part VI — Testing and quality

```sh
cargo fmt --check                                   # formatting
cargo clippy --all-targets -- -D warnings           # lints, default build
cargo clippy --all-targets --all-features -- -D warnings
cargo test                                          # 57 tests
cargo test --all-features                           # 65 tests (adds ESP32 + nl80211)
cargo test regenerate_replay_fixture -- --ignored   # rewrite tests/fixtures/synthetic_walk.csv
```

| Area | What the tests check |
|---|---|
| Detector | Metric values; CSI score ignores null subcarriers and averages the top quarter; threshold floors; **pure noise never triggers** (5 seeds × 2 metrics × 10 min); synthetic bursts are detected; a burst starts MOTION exactly after `on-ms` and ends after `off-ms`; no score before warm-up; gaps reset the window; links are independent; `Status` every 500 ms; a CSI link detects motion on a few subcarriers. |
| Hysteresis | Starts only after the on-time; hovering at the threshold doesn't flicker; with hysteresis, hovering noise gives at most a fifth as many MOTION starts as without it. |
| procfs | Normal lines, trailing dots, multiple interfaces, positive scale, no-data values, missing interface, malformed lines; staleness reported once per episode; a sample on every poll; invalid poll rates rejected. |
| traffic | Gateway hex → IPv4; bad hex; default routes sorted by metric; WiFi gateway chosen over Ethernet; ping arguments. |
| synthetic | Quantized values around the base level; values held between updates; bursts add variance; deterministic per seed; burst schedules fit. |
| config | Baseline compatibility rules; save/load round trip; corrupt file rejected. |
| zones | One link reports nothing; hold time; no ping-pong. |
| output | Event lines; JSON exactly as expected; zone lines; colour stripping; the status bar. |
| record/replay | CSV round trip; bad CSI values; timing rebuilt from timestamps; malformed rows report their row number. |
| esp32 | Real line layout; amplitude math; CRLF; 11 kinds of malformed lines; 2000 random corruptions without a panic; line reassembly. |
| nl80211 | Station info parsing; the nested chain-signal decoding; "no signal" detection. |
| scan, CLI | `iw` output parsing; `--esp32` spec parsing; clap definition check. |
| End to end | Replaying the fixture gives exactly CalibrationStarted, Calibrated, MotionStarted (35–38 s), MotionEnded (47–58 s) and 120 Status events; a huge sensitivity misses it; a saved baseline skips calibration; a missing file gives a clear error. |

---

# Part VII — Learning path and exercises

Read the code in this order; each step builds on the previous one.

1. **Run `wisense demo`, then read `detector.rs`.** Start with the module
   comment, then `mean_abs_diff`, `Hysteresis`, `Detector::process`, then
   the tests. *Exercise:* change `DEFAULT_ON_MS` to 0 and see which tests
   fail and why.
2. **`sample.rs`, `source/mod.rs`, `source/synthetic.rs`**: the data and how
   fake data is made. *Exercise:* run `wisense demo --seed 3 --speed 10`
   with `--metric std` and compare with `diff`.
3. **`source/procfs.rs` and `traffic.rs`**: the Linux side. *Exercise:* run
   `watch -n 0.2 cat /proc/net/wireless` while walking between the laptop
   and the router, and watch the level column.
4. **`main.rs`**: start at `run_pipeline`, then `open_sources`, then the
   commands. *Exercise:* explain in your own words what each `select!`
   branch does.
5. **`output.rs` and `output/tui.rs`**: sinks, the live line, the dashboard.
   *Exercise:* `wisense replay walk.csv --speed 0 --json | jq -c 'select(.event=="Status") | .score'`
   and plot the scores (a spreadsheet works).
6. **`record.rs`, `source/replay.rs`, `tests/replay.rs`**: tuning and
   end-to-end testing. *Exercise:* replay your `walk.csv` with
   `--window-ms 4000` (no `--baseline`, since your saved one is for 2000 ms)
   and compare the events with the 2 s results.
7. **`zones.rs`, `source/esp32.rs`, `source/nl80211.rs`**: the multi-sensor
   parts.

Further exercises:

- **Physics**: measure the distance between your laptop and router and
  compute the Fresnel radius at the middle ([§11](#11-why-a-moving-body-changes-the-signal)).
  Is your walking route inside it?
- **Code**: add an `--off-ratio` flag (it lives in `DetectorConfig`). Wire it
  through `DetectorArgs::to_config`, then write a test.
- **Statistics**: implement a "robust" baseline using the median and median
  absolute deviation instead of mean and std, and compare false alarms on
  synthetic noise.
- **Signals**: with `--rssi-source nl80211 -v`, compare the two antennas'
  chain signals while walking. Do they react differently?
- **Integration**: pipe `--json` into a small script that turns a light on
  through your home-automation system when `MotionStarted` arrives.

---

# Appendix — Measured on this laptop

| Fact | Value |
|---|---|
| Card / driver | Realtek RTL8822CE, `rtw88_8822ce` (mac80211) |
| Interface / link | `wlp2s0`, 5 GHz channel 64 (5320 MHz), 80 MHz, 802.11ac (VHT) |
| Router | gateway 192.168.1.1, beacon interval 100 TU (≈ 9.8 beacons/s), power save on |
| Signal | about −46 to −58 dBm |
| Frames from the router | 21/s (no keepalive), 24/s (ping 0.2 s), 43/s (ping 0.05 s) |
| RSSI value changes | 7.6 / 8.8 / 14.2 per second (read at 100 Hz) |
| Idle RSSI spread | about 1.3–1.6 dB standard deviation |
| Your calibration (60 s) | idle score 0.570 ± 0.155 dB → threshold 1.19 dB, end level 0.83 dB |
| nl80211 | works without root; two antenna chains |
| ping | iputils 20240905, non-root minimum interval 2 ms |

---

# Glossary

| Term | Meaning |
|---|---|
| **AP / router** | Access point: the device your laptop connects to over WiFi. |
| **STA** | Station: a WiFi client (your laptop, an ESP32 in `active_sta` mode). |
| **SSID / BSSID** | Network name / the access point's hardware address. |
| **dB, dBm** | Logarithmic power ratio / power relative to 1 mW ([§10](#10-radio-basics)). |
| **RSSI** | Received signal strength of a frame, in dBm. |
| **Frame** | One WiFi transmission unit (data, beacon, acknowledgement, …). |
| **Beacon** | Frame the router broadcasts about 10×/s to announce the network. |
| **TU** | Time unit = 1.024 ms (beacon interval 100 TU = 102.4 ms). |
| **DTIM / power save** | Mechanism letting clients sleep between beacons to save power. |
| **OFDM / subcarrier** | Splitting a channel into many narrow carriers ([§13](#13-csi-a-much-richer-view)). |
| **CSI** | Channel State Information: per-subcarrier amplitude and phase. |
| **LLTF / LTF** | Known training symbols at the start of a frame, from which CSI is computed. |
| **LOS / NLOS** | Line of sight / no line of sight between the antennas. |
| **Multipath** | The signal arriving over several paths at once. |
| **Fading** | Strength changes caused by those paths adding and cancelling. |
| **Fresnel zone** | Ellipsoid around the direct path that carries most of the energy. |
| **Doppler** | Frequency shift / fluctuation rate caused by moving reflectors. |
| **VHT / MCS** | 802.11ac "Very High Throughput" / modulation and coding scheme (link speed). |
| **ICMP echo** | What `ping` sends and what the router answers. |
| **Keepalive** | Steady traffic that keeps RSSI fresh. |
| **Poll rate** | How often wisense reads the RSSI. |
| **Zero-order hold** | A sampled value staying constant until the next update. |
| **Quantization** | Rounding to fixed steps (whole dBm). |
| **Window** | The last N seconds of samples a score is computed from. |
| **Score** | Number summarizing how much the signal fluctuates in the window. |
| **Baseline** | Mean and std of the score in an empty, still room. |
| **z-score / k** | "How many standard deviations above normal"; k = `--sensitivity`. |
| **False positive / negative** | False alarm / missed motion. |
| **Hysteresis** | Different levels for switching on and off ([§17](#17-hysteresis-and-debouncing)). |
| **Debounce** | Requiring a condition to hold for some time before acting. |
| **d′** | Separation between two score distributions ([§19](#19-measuring-how-well-it-works)). |
| **Netlink / nl80211** | Kernel–user messaging / its WiFi family ([Part V](#nl80211-and-netlink)). |
| **cfg80211 / mac80211** | Kernel WiFi configuration layer / shared driver framework. |
| **Process group** | Set of processes that receive terminal signals together. |
| **Raw mode / alternate screen** | Terminal modes used by the TUI ([Part V](#terminals)). |
| **UART / baud** | Serial link / its bit rate. |
| **Backpressure** | A full channel making producers wait, bounding memory. |

---

# Ethics

wisense senses radio signals in the space around your own WiFi. Only run it
on your own network and in your own home, or where everyone affected knows
and agrees. Do not use it to monitor people without their knowledge.
