# How the code is organized

Part of the [wisense documentation](../README.md#learn-how-it-works).

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
| [`src/main.rs`](../src/main.rs) | ~1050 | CLI (clap), opening sources, baselines, the pipeline, handlers, shutdown. |
| [`src/config.rs`](../src/config.rs) | ~330 | All defaults, `DetectorConfig`, `Metric`, the baseline file (load, save, compatibility checks). |
| [`src/sample.rs`](../src/sample.rs) | ~40 | The `Sample` type every source produces. |
| [`src/detector.rs`](../src/detector.rs) | ~1040 | The core: windows, scores, calibration, thresholds, hysteresis, events. Pure, no I/O. |
| [`src/zones.rs`](../src/zones.rs) | ~200 | Zone selection with hold time. Pure. |
| [`src/traffic.rs`](../src/traffic.rs) | ~260 | Default gateway from `/proc/net/route`; the supervised `ping` keepalive. |
| [`src/output.rs`](../src/output.rs) | ~550 | Event sinks (human, JSON), the live status line, log capture. |
| [`src/output/tui.rs`](../src/output/tui.rs) | ~320 | The ratatui dashboard (feature `tui`). |
| [`src/record.rs`](../src/record.rs) | ~190 | CSV row format and the recorder; generator of the test fixture. |
| [`src/scan.rs`](../src/scan.rs) | ~130 | `list-aps`: runs `iw` and parses its output. |
| [`src/source/mod.rs`](../src/source/mod.rs) | ~40 | The `SampleSource` trait and the task that drives a source. |
| [`src/source/procfs.rs`](../src/source/procfs.rs) | ~570 | `/proc/net/wireless` parsing, interface selection, staleness tracking, the poller. |
| [`src/source/nl80211.rs`](../src/source/nl80211.rs) | ~290 | nl80211 station info (feature `nl80211`). |
| [`src/source/esp32.rs`](../src/source/esp32.rs) | ~340 | ESP32-CSI-Tool line parser and serial reader (feature `esp32`). |
| [`src/source/replay.rs`](../src/source/replay.rs) | ~120 | Plays back a CSV recording with its original timing. |
| [`src/source/synthetic.rs`](../src/source/synthetic.rs) | ~350 | Realistic fake RSSI for `demo` and tests; a small random number generator. |
| [`tests/replay.rs`](../tests/replay.rs) | ~130 | End-to-end tests running the real binary on a fixture. |

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
  implements [§18](theory.md#18-zones-with-several-sensors).
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
- **Emit every poll** ([§14](theory.md#14-sampling-the-signal)).
- **stdout for events, stderr for diagnostics**, so output can be piped.
- **Baselines carry their settings**, so a stale baseline can't silently
  give nonsense thresholds.
- **Optional features off by default**, so the basic build stays small and
  needs no system libraries.
- **No `unsafe`, no `unwrap()`/`expect()` outside tests.** Every failure is a
  `Result` with context.
- **The keepalive `ping` stays in wisense's process group**, so a Ctrl+C or a
  closed terminal reaches it too, even if wisense dies abruptly. wisense also
  kills it on every normal exit path ([Linux and hardware](linux-and-hardware.md#processes-and-signals)).
