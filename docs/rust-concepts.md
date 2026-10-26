# Rust concepts used in this project

Part of the [wisense documentation](../README.md#learn-how-it-works).

Each concept below comes with where to see it in the code.

## Ownership and borrowing

- `Detector::process(&mut self, sample: Sample)` takes the sample **by
  value**, so its CSI vector moves into the window without being copied.
- `Hysteresis::update` matches on `&mut self.state`, copies out what it
  needs, then replaces `self.state`. This is legal because the borrow ends
  after its last use (non-lexical lifetimes).
- In `output/tui.rs`, `terminal.draw(...)` returns a value that *borrows*
  the terminal. The code drops it with `.map(|_| ())` before moving the
  terminal back into the struct. The compiler error that forced this
  (E0505) is a good one to understand.

## Enums and pattern matching

- `Event`, `MotionState`, `Phase`, `HystState`, `Transition`, `Signal` and
  `Staleness` make every state explicit.
- Note `let … else` (`let Some(x) = … else { return … };`), match guards
  (`Some(s) if *t > 0.0`), `matches!`, and binding fields directly in
  patterns (`Event::Status { link_id, score, .. }`).

## Option, Result and `?`

- `?` returns early on error.
- Useful helpers in the code: `is_some_and`, `is_none_or`, `then_some`,
  `ok_or_else`, `get_or_insert`, `transpose`.
- `Esp32Source::next` turns `Option<Result<Sample>>` into
  `Result<Option<Sample>>` with `.transpose()`.

## Error handling: anyhow and thiserror

- **`anyhow`** is for application errors. `.context("…")` and
  `.with_context(|| …)` add a readable chain, e.g. "reading
  /proc/net/wireless: No such file or directory".
- **`thiserror`** is for typed errors that callers and tests match on:
  `WirelessError`, `CsiParseError`, `RouteError`. The tests compare them
  with `assert_eq!`.

## Traits, generics and trait objects

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

## async/await with Tokio

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

## Why the `async-trait` crate

An `async fn` in a trait returns a future whose type and size differ per
implementation. That's fine with generics, but `dyn SampleSource` needs one
known type. `#[async_trait]` rewrites the method to return
`Pin<Box<dyn Future + Send>>`, a boxed future of known size, so trait
objects work.

## Mixing blocking code with async

The serial port API blocks, and blocking inside an async task would freeze
other tasks. So `esp32.rs` reads the port in a **dedicated `std::thread`**
and hands samples over with `blocking_send`. That call is meant for
non-async threads and must never be used inside async code. The procfs
source reads with `tokio::fs`.

## Cargo features and conditional compilation

- `Cargo.toml` declares `nl80211`, `esp32` and `tui`, each enabling optional
  dependencies with the `dep:` syntax.
- `#[cfg(feature = "tui")]` compiles a module or function only when the
  feature is on.
- `main.rs` has *pairs* of functions, the real one and a fallback that
  returns a helpful error ("rebuild with --features esp32"). So the CLI is
  identical in every build.
- Also used: `#[cfg(test)]`, and `#[cfg(target_endian = "little")]` on
  byte-order tests.

## Command-line parsing with clap (derive)

- `#[derive(Parser, Subcommand, Args, ValueEnum)]` turns structs and enums
  into a CLI. Doc comments become the help text.
- `#[command(flatten)]` reuses flag groups (`SourceArgs`, `DetectorArgs`,
  `OutputArgs`).
- `default_value_t = config::DEFAULT_…` keeps defaults in one place.
- `value_parser = parse_esp32_spec` does custom validation, and
  `conflicts_with` / `requires` express rules between flags.
- The test `Cli::command().debug_assert()` checks the whole definition.

## Serialization with serde (JSON and CSV)

- `#[derive(Serialize, Deserialize)]` on events, baselines and CSV rows.
- `#[serde(tag = "event")]` produces internally tagged JSON
  (`{"event":"MotionStarted",…}`).
- `#[serde(rename_all = "lowercase")]` gives `"motion"`, `"rssi"`.
- `#[serde(default, skip_serializing_if = "Option::is_none")]` handles
  optional baseline fields.
- The same `Row` struct is written by `Recorder` and read by `ReplaySource`
  through the `csv` crate.

## Logging with tracing

- `tracing::info!`, `warn!`, `debug!` emit events; `init_tracing` installs a
  subscriber with a level picked by `-v`.
- `output::LogWriter` implements `MakeWriter`, deciding per message where
  the text goes (stderr, or the TUI's buffer).

## Global state without `unsafe`

- `static LIVE_LINE: AtomicBool` and
  `static LOG_BUFFER: Mutex<VecDeque<String>> = Mutex::new(VecDeque::new())`
  are shared safely through atomics and a mutex with `const`
  initialization.
- A poisoned mutex is recovered with `.unwrap_or_else(|e| e.into_inner())`.
- `LogTarget` implements `Drop` to store a captured message when tracing is
  done with it.

## Time

- **`Instant`** is a monotonic clock that never jumps. It's used for
  windows, timers and durations.
- **`SystemTime`** is the wall clock, which can jump when NTP corrects it.
  It's used for timestamps shown to people and written to files.
- `Instant::duration_since` saturates at zero instead of panicking when
  times are out of order.

## Numeric care

`saturating_sub`; `u64::try_from(…).unwrap_or(u64::MAX)` instead of lossy
`as`; `f32::total_cmp` to sort floats (which aren't totally ordered because
of NaN); `f64` accumulators for precision.

## Testing techniques

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

## Tooling

- `cargo fmt` formats the code.
- `cargo clippy --all-targets -- -D warnings` lints it. It caught, for
  example, `x % n == 0`, which it wants written as `x.is_multiple_of(n)`.
- `cargo test --all-features` runs the tests.
