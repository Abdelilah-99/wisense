//! homeguard: a local home security hub. See README.md.
//!
//! - [`core`]: the pure decision logic (state machine, fusion rules).
//! - [`parse`]: pure parsers for the MQTT messages homeguard listens to.
//! - [`publish`]: what homeguard publishes, as a pure function of actions.
//! - [`config`], [`mqtt`], [`run`]: the imperative shell.

pub mod config;
pub mod core;
pub mod mqtt;
pub mod parse;
pub mod publish;
pub mod run;
