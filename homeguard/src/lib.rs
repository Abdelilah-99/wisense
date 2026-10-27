//! homeguard: a local home security hub. See README.md.
//!
//! - [`core`]: the pure decision logic (state machine, fusion rules).
//! - [`parse`]: pure parsers for the MQTT messages homeguard listens to.
//! - [`publish`]: what homeguard publishes, as a pure function of actions.
//! - [`sim`]: scripted scenarios replayed through the real core.
//! - [`config`], [`mqtt`], [`run`], [`bridge`], [`presence`], [`telegram`],
//!   [`db`], [`web`]: the imperative shell.

pub mod bridge;
pub mod chat;
pub mod config;
pub mod core;
pub mod db;
pub mod delivery;
pub mod discord;
pub mod frigate;
pub mod mqtt;
pub mod notify;
pub mod parse;
pub mod presence;
pub mod publish;
pub mod run;
pub mod sim;
pub mod status;
pub mod telegram;
pub mod web;

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
