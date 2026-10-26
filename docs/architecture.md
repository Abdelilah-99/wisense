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

 Keepalive: a supervised `ping` child -> frames from the access point -> fresh RSSI
   target: the gateway, --keepalive-target IP, or `auto` (the gateway if it
   answers, otherwise the best device found by discovery)

 Discovery (`wisense discover`, `--keepalive-target auto`):
   /proc/net/route -> subnet (max /22) -> one `ping` per host, 64 at a time
                                          | PingStats
                                          v
   /proc/net/arp (read afterwards) -----> merge_arp -> rank_candidates -> Candidates
                                                                          |
   iw dev <if> link -> BSSID -----------> match_access_point <------------+
                                          -> "associated AP" tag, and a warning
                                             if it isn't the gateway
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

Discovery runs before or instead of that pipeline: `wisense discover`
prints the ranked devices, `list-ifaces` shows the associated access point,
and `--keepalive-target auto` uses the ranking to pick the keepalive target
when the gateway ignores pings. Its decisions (parsing, ranking, matching,
the choice of target) are pure functions; only reading `/proc`, running
`ping` and `iw`, and the UDP socket that finds the laptop's own address do
I/O.

## A tour of the files

| File | Lines | Responsibility |
|---|---|---|
| [`src/main.rs`](../src/main.rs) | ~1170 | CLI (clap), opening sources, the keepalive target, baselines, the pipeline, handlers, shutdown, `discover` output. |
| [`src/config.rs`](../src/config.rs) | ~340 | All defaults, `DetectorConfig`, `Metric`, the baseline file (load, save, compatibility checks). |
| [`src/sample.rs`](../src/sample.rs) | ~40 | The `Sample` type every source produces. |
| [`src/detector.rs`](../src/detector.rs) | ~1040 | The core: windows, scores, calibration, thresholds, hysteresis, events. Pure, no I/O. |
| [`src/zones.rs`](../src/zones.rs) | ~200 | Zone selection with hold time. Pure. |
| [`src/traffic.rs`](../src/traffic.rs) | ~480 | `/proc/net/route` rows and the default gateway; the supervised `ping` keepalive; `--keepalive-target auto`. |
| [`src/discover.rs`](../src/discover.rs) | ~1590 | `wisense discover`: ARP table, MAC addresses, the WiFi subnet, `ping` output, probing, ranking, the associated access point. About two thirds tests. |
| [`src/output.rs`](../src/output.rs) | ~550 | Event sinks (human, JSON), the live status line, log capture. |
| [`src/output/tui.rs`](../src/output/tui.rs) | ~320 | The ratatui dashboard (feature `tui`). |
| [`src/record.rs`](../src/record.rs) | ~190 | CSV row format and the recorder; generator of the test fixture. |
| [`src/scan.rs`](../src/scan.rs) | ~290 | `iw`: the scan for `list-aps`, and `iw dev <if> link` for the associated access point. |
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
  - `parse_routes` reads every row of `/proc/net/route` into a `Route`;
    `default_routes` and `gateway_for` pick the gateway of the WiFi
    interface.
  - `Keepalive::start` spawns `ping -i 0.2 -q -n <gateway>` and a supervisor
    task that reports unexpected exits; `stop()` kills it.
  - `parse_keepalive_target` reads `--keepalive-target IP|auto`.
    `auto_target` pings the gateway a few times and keeps it if it answers;
    otherwise it runs a discovery, and the pure `choose_keepalive_target`
    picks the best-ranked device that answered, preferring a fixed MAC.
- **`discover.rs`**:
  - `parse_arp_line` reads one `/proc/net/arp` line into an `ArpEntry`;
    incomplete entries (flags `0x0` or a zero MAC) have `mac: None`.
    `MacAddr` parses and prints addresses; `is_randomized_mac` checks the
    locally administered bit.
  - `subnet_for` finds the WiFi subnet among the `Route`s: up, on the
    interface, no gateway, not link-local; the one holding the gateway wins.
    `Subnet::check_size` refuses anything larger than a /22 unless allowed.
  - `parse_ping_output` reads ping's summary, each reply's time and the
    `mdev`. `probe_hosts` runs one `ping -c N -W T -n` per host with
    `kill_on_drop(true)`, at most `concurrency` at a time through a
    `Semaphore` in a `JoinSet`.
  - `merge_arp` joins the results with the ARP table (devices that only
    answered ARP stay, as "no ICMP reply"); `rank_candidates` sorts by
    response rate, then jitter, then median RTT.
  - `match_access_point` finds the LAN device whose MAC equals the BSSID,
    or is a near match ("likely"); `ap_warning` says when it isn't the
    gateway.
  - `discover` ties it together for the command and for
    `--keepalive-target auto`; `format_table` and `DeviceJson` are the two
    outputs.
- **`scan.rs`**: `parse_iw_scan` reads `iw dev <if> scan`; `parse_iw_link`
  reads `iw dev <if> link` (`Ok(None)` for "Not connected.").
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
- **Discovery is bounded and opt-in.** It never runs unless asked
  (`discover`, or `--keepalive-target auto` when the gateway is silent),
  refuses subnets larger than a /22 without `--allow-large-subnet`, needs no
  root, and kills its `ping` children if it is interrupted.
- **Reliability before latency** when ranking keepalive targets: any device
  works because traffic to it passes through the access point, so what
  matters is that it keeps answering.
