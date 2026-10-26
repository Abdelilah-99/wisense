# Learning path and exercises

Part of the [wisense documentation](../README.md#learn-how-it-works).

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
   end-to-end testing. *Exercise:* replay a recording such as `walk.csv`
   with `--window-ms 4000` (no `--baseline`, since one saved with the
   defaults is for 2000 ms)
   and compare the events with the 2 s results.
7. **`zones.rs`, `source/esp32.rs`, `source/nl80211.rs`**: the multi-sensor
   parts.

Further exercises:

- **Physics**: measure the distance between your laptop and router and
  compute the Fresnel radius at the middle ([§11](theory.md#11-why-a-moving-body-changes-the-signal)).
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
