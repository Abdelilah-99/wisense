use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use clap::{Parser, Subcommand};
use tracing::error;

use homeguard::config::AppConfig;
use homeguard::core::time::parse_duration;

/// Local home security hub: alarm state machine, sensor fusion and alerts.
#[derive(Debug, Parser)]
#[command(version, about)]
struct Cli {
    /// Log filter, e.g. `info`, `debug` or `homeguard=debug,rumqttc=warn`.
    #[arg(long, global = true, env = "HOMEGUARD_LOG", default_value = "info")]
    log: String,

    /// Where homeguard keeps its state (arm state, heartbeat).
    #[arg(
        long,
        global = true,
        env = "HOMEGUARD_DATA_DIR",
        default_value = "data/homeguard"
    )]
    data_dir: PathBuf,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run the hub service.
    Run {
        #[arg(
            long,
            short,
            env = "HOMEGUARD_CONFIG",
            default_value = "config/homeguard.toml"
        )]
        config: PathBuf,
    },
    /// Check a configuration file and exit.
    CheckConfig {
        #[arg(
            long,
            short,
            env = "HOMEGUARD_CONFIG",
            default_value = "config/homeguard.toml"
        )]
        config: PathBuf,
    },
    /// Publish `wisense monitor --json` lines from stdin to MQTT.
    BridgeWisense(homeguard::bridge::BridgeArgs),
    /// Exit 0 if a running `homeguard run` is alive (for Docker).
    Healthcheck {
        /// Maximum heartbeat age.
        #[arg(long, default_value = "30s", value_parser = parse_duration)]
        max_age: Duration,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    homeguard::init_tracing(&cli.log);
    match run(cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            error!("{e:#}");
            ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> anyhow::Result<()> {
    match cli.command {
        Command::Run { config } => {
            let config = AppConfig::load(&config)?;
            tokio::runtime::Runtime::new()?.block_on(homeguard::run::run(config, cli.data_dir))
        }
        Command::CheckConfig { config } => {
            let loaded = AppConfig::load(&config)?;
            let engine = loaded.engine()?;
            println!(
                "{}: ok ({} zones, {} sensors, {} cameras)",
                config.display(),
                engine.catalog().zones().count(),
                engine.catalog().sensors().count(),
                engine.catalog().cameras().count()
            );
            Ok(())
        }
        Command::BridgeWisense(args) => {
            let options = args.into_options(|name| std::env::var(name).ok())?;
            tokio::runtime::Runtime::new()?.block_on(homeguard::bridge::run(options))
        }
        Command::Healthcheck { max_age } => homeguard::run::healthcheck(&cli.data_dir, max_age),
    }
}
