//! `homeguard-wisense-bridge`: the same as `homeguard bridge-wisense`, as a
//! small binary of its own for the host, where wisense runs.
//!
//! ```text
//! wisense monitor --json | homeguard-wisense-bridge --zone living-room --link wlp2s0
//! ```

use std::process::ExitCode;

use clap::Parser;
use tracing::error;

use homeguard::bridge::BridgeArgs;

/// Publish `wisense monitor --json` lines from stdin to homeguard over MQTT.
#[derive(Debug, Parser)]
#[command(version, about)]
struct Cli {
    /// Log filter, e.g. `info` or `debug`.
    #[arg(long, env = "HOMEGUARD_LOG", default_value = "info")]
    log: String,
    #[command(flatten)]
    bridge: BridgeArgs,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    homeguard::init_tracing(&cli.log);
    let result = cli
        .bridge
        .into_options(|name| std::env::var(name).ok())
        .and_then(|options| {
            tokio::runtime::Runtime::new()?.block_on(homeguard::bridge::run(options))
        });
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            error!("{e:#}");
            ExitCode::FAILURE
        }
    }
}
