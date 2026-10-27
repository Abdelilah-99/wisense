# Testing

homeguard is built so that almost everything can be tested without hardware:
the decision logic is pure (see [architecture.md](architecture.md)), and the
parts that touch the world are thin.

## Running the checks

From `homeguard/`, the same checks CI runs on every push:

```sh
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
docker compose config -q
scripts/check-secrets.sh
```

Useful variations:

```sh
cargo test core::engine          # one module
cargo test owner                 # every test whose name contains "owner"
cargo test -- --nocapture        # show println! output
cargo test --test scenarios      # only the scenario test
```

## Scenarios

`scenarios/*.toml` are scripted timelines replayed through the real core:

```sh
cargo run -- simulate scenarios/owner_coming_home.toml            # instant
cargo run -- simulate scenarios/owner_coming_home.toml --speed 20 # paced
for f in scenarios/*.toml; do cargo run -q -- simulate "$f" | tail -1; done
```

Each one declares what must happen (`[expect]`: ordered actions with
optional exact times, forbidden actions, counts, final state); `simulate`
exits non-zero if that is not met, and `tests/scenarios.rs` runs them all.
The README's "Simulation" section describes the format. To add one, copy a
scenario, change the steps, run it, and write the `[expect]` you want; the
test picks it up automatically.

| Scenario | Checks |
|---|---|
| `burglar_front_door` | arming 0 s, armed 1 m, pending 5 m, alarm exactly 5 m 30 s |
| `burglar_confirmed_by_camera` | wisense + person in the same zone: alarm at the person's time, no entry delay |
| `motion_without_person` | wrong zone and low score do not confirm; two silent notices, no alarm |
| `owner_coming_home` | phone arrival then door: no pending, no alarm, a notice; disarm |
| `camera_unplugged_while_armed` | one tampering alert at offline + 60 s; a flapping camera is not reported |
| `smoke_while_disarmed` | one critical alert per cause per cooldown, siren on every report, state untouched |
| `flaky_motion_flood` | 361 wisense reports in 30 min: no alarm, exactly 4 notices, 10 min apart |

## What the tests cover

**145 tests**: 135 unit tests next to the code and 10 integration tests in
`tests/` (one of which runs all 7 scenarios).

| Area | Tests | What they pin down |
|---|---:|---|
| `core::machine` | 19 | exit delay to the millisecond; doors ignored while arming; entry delay disarmed in time vs expiring at its exact end (even with a late tick); a second sensor not restarting the delay; immediate intrusions; mode filtering (home, night, away); unknown sensors; auto-reset and re-arming; disarm silencing the siren; re-arm needing a disarm; smoke and leaks in every state; restore after restart |
| `core::engine` | 30 | every fusion rule: door → pending → triggered; repeated contact reports; motion alone; person confirming (either order, threshold score counts, NaN ignored); low score / other zone / late person not confirming; two kinds of source confirming; zones not combining; the fast flaky motion flood and per-zone notice cooldowns; Frigate updates used once; owner arriving (notice, grace, auto-disarm, too-early arrival, repeated presence); tampering after the debounce, flapping, non-critical and disarmed cases, de-duplication; smoke cooldown; auto-arm and auto-disarm rules |
| `core::presence` | 5 | home on first sighting; away exactly at the grace period; a sleeping phone staying home; never seen counting from startup; out-of-order sightings |
| `core::config`, `core::time` | 8 | default modes and criticality per kind; zone and duplicate validation; duration parsing and formatting |
| `parse::*` | 20 | Zigbee contact/occupancy/smoke/leak/availability (both formats), name mapping; Frigate events (best score, end, other labels, false positives), Frigate-wide and per-camera status; wisense bridge messages; ARP table, MAC and subnet parsing; malformed input never panicking |
| `tests/parsers.rs` | 6 | real-looking fixtures from `tests/fixtures/mqtt/` (full Frigate events, typical Aqara/Sonoff/Heiman payloads) through the router, garbage rejected on every topic, and every fixture on every topic without a panic |
| `publish`, `status` | 5 | retained state JSON with Home Assistant states; siren and plug commands; the dashboard/Telegram snapshot |
| `bridge` | 6 | wisense lines to messages, heartbeat rate limit per link, zone changes, silence → offline once then recovery, unknown events and garbage |
| `presence` (shell) | 2 | MACs required from the environment; sightings need a resolved ARP entry |
| `telegram::*` | 7 | command parsing (`/arm@bot away`, case, other bots, errors); allow-list parsing and authorization; status/event texts; path-safety of snapshot ids; credentials; stale commands |
| `web::*` | 13 | Basic auth, constant-time comparison, lockouts; the API with an in-memory database: login required, status, events and limits, arm, disarm PIN, lockouts for passwords and for PINs separately, CSRF (origin, non-JSON bodies), snapshot id validation, self-contained page |
| `db` | 4 | insert, newest-first listing, pruning, batched writer, reopening a file |
| `config`, `run`, `mqtt` | 10 | config defaults, credentials from the environment, unknown keys and bad mappings rejected; heartbeat freshness; arm state persisted across restarts; reconnect backoff |
| `sim` + `tests/scenarios.rs` | 9 | exact deadlines, repeated steps, expectations (order, exact time, never, counts); every scenario passing; determinism |
| `tests/example_config.rs` | 1 | the committed `config/homeguard.toml` stays valid |

## Checked by hand, outside the test suite

These were run once during development against real components on a
laptop, and are worth repeating after significant changes:

- **The Compose stack**: `docker compose config -q` with and without profiles;
  Mosquitto with the generated password file (anonymous refused, logins
  accepted, healthcheck); the homeguard image (non-root, ~150 MB) running
  healthy in the host network with the dashboard and the database.
- **MQTT end to end**: homeguard connected to the real broker, fixtures
  published with `mosquitto_pub`, state and actions observed with
  `mosquitto_sub`, malformed messages dropped, arm state restored after a
  restart.
- **wisense**: the real `wisense monitor --json` on the laptop's card piped
  into the bridge: heartbeats, a real motion event reaching homeguard, offline
  at end of input, and the MQTT last will when the bridge is killed.
- **Presence**: the router's MAC standing in for a phone (seen at once) and a
  fake one (away exactly at the grace period).
- **Telegram**: a local fake of the Bot API (rate limit and `retry_after`,
  multipart photos, foreign and stale commands, the token never in logs).
- **The dashboard**: a headless browser screenshot against a live instance.

## What cannot be tested without the hardware

- Real cameras through Frigate (RTSP paths, detection quality, the exact
  `status/detect` behaviour during an outage).
- Real Zigbee devices: pairing, the exact payloads of each model, the siren's
  response to the `warning` command, availability timing.
- Presence with real phones: how long each phone model stays silent, and
  whether 10 minutes of grace is right for your household.
- Telegram's real API (the fake follows the documented behaviour).
- Behaviour over days: retention pruning, long-running reconnections.

Before trusting the system, do the "first alert" test in
[setup.md](setup.md#12-the-first-alert) and repeat it every few months.
