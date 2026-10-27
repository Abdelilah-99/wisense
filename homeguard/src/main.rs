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
        /// Log alerts instead of sending them to Telegram.
        #[arg(long)]
        dry_run: bool,
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
    /// Replay a scenario through the real core: no MQTT, no hardware; alerts
    /// are printed instead of sent.
    Simulate {
        /// The scenario file (see scenarios/).
        scenario: PathBuf,
        /// Replay this many times faster than real time (e.g. 10). Without
        /// it the replay is instant.
        #[arg(long)]
        speed: Option<f64>,
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
        Command::Run { config, dry_run } => {
            let config = AppConfig::load(&config)?;
            let options = homeguard::run::RunOptions {
                data_dir: cli.data_dir,
                dry_run,
            };
            tokio::runtime::Runtime::new()?.block_on(homeguard::run::run(config, options))
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
        Command::Simulate { scenario, speed } => simulate(&scenario, speed),
        Command::BridgeWisense(args) => {
            let options = args.into_options(|name| std::env::var(name).ok())?;
            tokio::runtime::Runtime::new()?.block_on(homeguard::bridge::run(options))
        }
        Command::Healthcheck { max_age } => homeguard::run::healthcheck(&cli.data_dir, max_age),
    }
}

fn simulate(path: &std::path::Path, speed: Option<f64>) -> anyhow::Result<()> {
    use homeguard::sim;

    if let Some(speed) = speed {
        if !(speed.is_finite() && speed > 0.0) {
            anyhow::bail!("--speed must be a positive number, e.g. 10");
        }
    }
    let (scenario, config) = sim::load(path)?;
    let outcome = sim::simulate(&config, &scenario)?;
    println!("scenario: {}", scenario.name);
    if let Some(description) = &scenario.description {
        println!("{}", description.trim());
    }
    println!();
    let mut previous = homeguard::core::Ts::ZERO;
    for record in &outcome.records {
        if let Some(speed) = speed {
            let wait = record.at().since(previous).as_secs_f64() / speed;
            std::thread::sleep(Duration::from_secs_f64(wait));
            previous = record.at();
        }
        println!("{}", sim::format_record(record));
    }
    println!();
    println!("final state: {}", outcome.final_state);
    match &scenario.expect {
        None => Ok(()),
        Some(expect) => {
            let failures = sim::check(&outcome, expect);
            if failures.is_empty() {
                println!("expectations: all met");
                Ok(())
            } else {
                for failure in &failures {
                    println!("FAILED: {failure}");
                }
                anyhow::bail!("{} expectation(s) not met", failures.len())
            }
        }
    }
}
