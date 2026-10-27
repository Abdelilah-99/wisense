//! homeguard: a local home security hub. See README.md.
//!
//! - [`core`]: the pure decision logic (state machine, fusion rules).
//! - [`parse`]: pure parsers for the MQTT messages homeguard listens to.
//! - [`publish`]: what homeguard publishes, as a pure function of actions.
//! - [`config`], [`mqtt`], [`run`], [`bridge`]: the imperative shell.

pub mod bridge;
pub mod config;
pub mod core;
pub mod mqtt;
pub mod parse;
pub mod publish;
pub mod run;

/// Diagnostics go to stderr, filtered by `filter` (e.g. `info`, `debug`,
/// `homeguard=debug,rumqttc=warn`); stdout stays free for command output.
pub fn init_tracing(filter: &str) {
    let filter = tracing_subscriber::EnvFilter::try_new(filter)
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .init();
}
