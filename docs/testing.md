# Testing and quality

Part of the [wisense documentation](../README.md#learn-how-it-works).

```sh
cargo fmt --check                                   # formatting
cargo clippy --all-targets -- -D warnings           # lints, default build
cargo clippy --all-targets --all-features -- -D warnings
cargo test                                          # 94 tests
cargo test --all-features                           # 102 tests (adds ESP32 + nl80211)
cargo test regenerate_replay_fixture -- --ignored   # rewrite tests/fixtures/synthetic_walk.csv
```

GitHub Actions runs the first five commands on every push and pull request
([`.github/workflows/ci.yml`](../.github/workflows/ci.yml)).

| Area | What the tests check |
|---|---|
| Detector | Metric values; CSI score ignores null subcarriers and averages the top quarter; threshold floors; **pure noise never triggers** (5 seeds × 2 metrics × 10 min); synthetic bursts are detected; a burst starts MOTION exactly after `on-ms` and ends after `off-ms`; no score before warm-up; gaps reset the window; links are independent; `Status` every 500 ms; a CSI link detects motion on a few subcarriers. |
| Hysteresis | Starts only after the on-time; hovering at the threshold doesn't flicker; with hysteresis, hovering noise gives at most a fifth as many MOTION starts as without it. |
| procfs | Normal lines, trailing dots, multiple interfaces, positive scale, no-data values, missing interface, malformed lines; staleness reported once per episode; a sample on every poll; invalid poll rates rejected. |
| traffic | Gateway hex → IPv4; bad hex; every route row parsed and malformed rows skipped; default routes sorted by metric; WiFi gateway chosen over Ethernet; ping arguments; `--keepalive-target` values (`auto`, IPv4, IPv6, garbage); the `auto` choice prefers a fixed MAC, falls back to randomized ones, and skips devices that don't answer pings. |
| discover | Real-looking ARP tables; incomplete entries; other interfaces; 8 kinds of malformed lines; 2000 random corruptions without a panic; MAC parsing; randomized vs universal MACs; the WiFi subnet from routes (link-local and other interfaces ignored, gateway's subnet preferred); masks, hosts and the /22 limit; ping output (normal, 100% loss, errors, partial loss with duplicates, BusyBox, garbage and 2000 corruptions); median and spread; probe arguments; a fast but flaky host ranks below a slightly slower reliable one; jitter then median break ties; no-ICMP devices last; the ARP merge and the gateway mark; the own address isn't probed; the table and the JSON object. |
| access point | Exact and near ("likely") BSSID matches, the closest winning, unrelated vendors rejected; the candidate tagged; the ARP neighbours of an interface; the warning only when the AP isn't the gateway. |
| synthetic | Quantized values around the base level; values held between updates; bursts add variance; deterministic per seed; burst schedules fit. |
| config | Baseline compatibility rules; save/load round trip; corrupt file rejected. |
| zones | One link reports nothing; hold time; no ping-pong. |
| output | Event lines; JSON exactly as expected; zone lines; colour stripping; the status bar. |
| record/replay | CSV round trip; bad CSI values; timing rebuilt from timestamps; malformed rows report their row number. |
| esp32 | Real line layout; amplitude math; CRLF; 11 kinds of malformed lines; 2000 random corruptions without a panic; line reassembly. |
| nl80211 | Station info parsing; the nested chain-signal decoding; "no signal" detection. |
| scan, CLI | `iw` scan output parsing; `iw link` (connected, "Not connected.", garbage and 2000 corruptions); `--esp32` spec parsing; clap definition check. |
| End to end | Replaying the fixture gives exactly CalibrationStarted, Calibrated, MotionStarted (35–38 s), MotionEnded (47–58 s) and 120 Status events; a huge sensitivity misses it; a saved baseline skips calibration; a missing file gives a clear error. |
