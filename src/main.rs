//! `wisense`: detect motion from WiFi signal fluctuations.

mod config;
mod detector;
mod discover;
mod output;
mod record;
mod sample;
mod scan;
mod source;
mod traffic;
mod zones;

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io::{IsTerminal, Write};
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use clap::{ArgAction, Args, Parser, Subcommand, ValueEnum};
use tokio::signal::unix::{signal, SignalKind};
use tokio::sync::mpsc;
use tokio::task::{JoinError, JoinSet};
use tokio::time::MissedTickBehavior;

use config::{BaselineEntry, BaselineFile, DetectorConfig, LinkSetup, Metric};
use detector::{Baseline, Detector, Event, ScoreKind};
use output::{EventSink, HumanSink, JsonSink};
use record::Recorder;
use sample::Sample;
use source::procfs::{self, ProcfsConfig, ProcfsSource, Signal};
use source::replay::ReplaySource;
use source::synthetic::{random_bursts, SyntheticConfig, SyntheticSource};
use source::SampleSource;
use traffic::{Keepalive, KeepaliveTarget};
use zones::ZoneTracker;

#[derive(Parser)]
#[command(
    name = "wisense",
    version,
    about = "Detect motion from WiFi signal fluctuations",
    after_help = "Only use wisense on networks and premises you own or may monitor."
)]
struct Cli {
    /// More log output (-v debug, -vv trace).
    #[arg(short, long, action = ArgAction::Count, global = true)]
    verbose: u8,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// List the wireless interfaces found in /proc/net/wireless.
    ListIfaces,
    /// List access points visible to the card (uses `iw scan`; a fresh scan
    /// needs root). Not used for sensing.
    ListAps(ListApsArgs),
    /// Find the devices on the WiFi network and rank them as keepalive
    /// targets. Only scan networks you own or are allowed to scan.
    Discover(DiscoverArgs),
    /// Measure the idle baseline with the room empty and save it.
    Calibrate(CalibrateArgs),
    /// Watch the links and report motion.
    Monitor(MonitorArgs),
    /// Record raw samples to CSV, for tuning with `replay`.
    Record(RecordArgs),
    /// Feed a recording through the detector.
    Replay(ReplayArgs),
    /// Run the detector on synthetic data; needs no hardware.
    Demo(DemoArgs),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum RssiSource {
    /// /proc/net/wireless; works with every driver.
    Procfs,
    /// nl80211 GET_STATION (build with --features nl80211).
    Nl80211,
}

/// `--esp32 PORT=NAME`
#[derive(Debug, Clone, PartialEq)]
struct Esp32Spec {
    port: String,
    name: String,
}

fn parse_esp32_spec(s: &str) -> Result<Esp32Spec, String> {
    let (port, name) = s
        .rsplit_once('=')
        .ok_or_else(|| format!("expected PORT=NAME, e.g. /dev/ttyUSB0=kitchen, got {s:?}"))?;
    let name = name.trim();
    let valid = !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
    if port.trim().is_empty() || !valid {
        return Err(format!(
            "expected PORT=NAME with a name of letters, digits, '-', '_' or '.', got {s:?}"
        ));
    }
    Ok(Esp32Spec {
        port: port.trim().to_string(),
        name: name.to_string(),
    })
}

/// Which links to sense with and how to keep them busy.
#[derive(Args, Clone)]
struct SourceArgs {
    /// Wireless interface to sense on [default: first one in /proc/net/wireless].
    #[arg(long)]
    iface: Option<String>,
    /// Where the laptop's RSSI is read from.
    #[arg(long, value_enum, default_value_t = RssiSource::Procfs)]
    rssi_source: RssiSource,
    /// Don't use the laptop's WiFi card; only ESP32 boards.
    #[arg(long, requires = "esp32")]
    no_wifi: bool,
    /// ESP32 board running ESP32-CSI-Tool, as PORT=NAME (e.g.
    /// /dev/ttyUSB0=kitchen). NAME becomes the link and zone name. Repeatable.
    #[arg(long = "esp32", value_name = "PORT=NAME", value_parser = parse_esp32_spec)]
    esp32: Vec<Esp32Spec>,
    /// Serial speed for the ESP32 boards.
    #[arg(long, default_value_t = config::DEFAULT_BAUD)]
    baud: u32,
    /// How often the laptop's RSSI is read, in Hz.
    #[arg(long, default_value_t = config::DEFAULT_POLL_HZ)]
    poll_hz: f32,
    /// Seconds between keepalive pings. Older ping versions need root below 0.2.
    #[arg(long, default_value_t = config::DEFAULT_PING_INTERVAL_S)]
    ping_interval: f64,
    /// Don't ping the access point. RSSI then only updates when other traffic
    /// or beacons arrive, and may look perfectly still.
    #[arg(long)]
    no_keepalive: bool,
    /// Ping this address instead of the interface's default gateway. `auto`
    /// keeps the gateway if it answers pings, and otherwise picks the most
    /// reliable device on the network (see `wisense discover`).
    #[arg(
        long,
        value_name = "IP|auto",
        conflicts_with = "no_keepalive",
        value_parser = traffic::parse_keepalive_target
    )]
    keepalive_target: Option<KeepaliveTarget>,
}

/// Detector tuning flags shared by every command that detects motion.
#[derive(Args, Clone)]
struct DetectorArgs {
    /// Threshold = baseline mean + SENSITIVITY * baseline std. Higher means
    /// fewer false alarms but less sensitive.
    #[arg(long, default_value_t = config::DEFAULT_SENSITIVITY)]
    sensitivity: f32,
    /// Length of the sliding window the score is computed over.
    #[arg(long, default_value_t = config::DEFAULT_WINDOW_MS)]
    window_ms: u64,
    /// The score must stay above the threshold this long to start MOTION.
    #[arg(long, default_value_t = config::DEFAULT_ON_MS)]
    on_ms: u64,
    /// The score must stay below 0.7 * threshold this long to end MOTION.
    #[arg(long, default_value_t = config::DEFAULT_OFF_MS)]
    off_ms: u64,
    /// Motion score for RSSI links: mean absolute first difference, or
    /// standard deviation.
    #[arg(long, value_enum, default_value_t = Metric::Diff)]
    metric: Metric,
}

impl DetectorArgs {
    fn to_config(&self) -> Result<DetectorConfig> {
        if !self.sensitivity.is_finite() || self.sensitivity < 0.0 {
            bail!("--sensitivity must be a non-negative number");
        }
        if self.window_ms < 100 {
            bail!("--window-ms must be at least 100");
        }
        Ok(DetectorConfig {
            window: Duration::from_millis(self.window_ms),
            metric: self.metric,
            sensitivity: self.sensitivity,
            on: Duration::from_millis(self.on_ms),
            off: Duration::from_millis(self.off_ms),
            ..DetectorConfig::default()
        })
    }
}

#[derive(Args, Clone)]
struct OutputArgs {
    /// One JSON object per event on stdout (status updates included).
    #[arg(long, conflicts_with = "tui")]
    json: bool,
    /// Live terminal dashboard (build with --features tui).
    #[arg(long)]
    tui: bool,
}

#[derive(Args)]
struct ListApsArgs {
    /// Interface to scan with [default: first wireless interface].
    #[arg(long)]
    iface: Option<String>,
}

#[derive(Args)]
struct DiscoverArgs {
    /// Wireless interface whose subnet is scanned [default: first one in
    /// /proc/net/wireless].
    #[arg(long)]
    iface: Option<String>,
    /// Pings sent to each address.
    #[arg(long, default_value_t = config::DEFAULT_DISCOVER_COUNT)]
    count: u32,
    /// Seconds to wait for each reply.
    #[arg(long, default_value_t = config::DEFAULT_DISCOVER_TIMEOUT_S)]
    timeout: f64,
    /// Addresses probed at the same time.
    #[arg(long, default_value_t = config::DEFAULT_DISCOVER_CONCURRENCY)]
    concurrency: usize,
    /// Probe a subnet larger than a /22 (more than 1022 addresses).
    #[arg(long)]
    allow_large_subnet: bool,
    /// One JSON object per device on stdout, best first.
    #[arg(long)]
    json: bool,
}

#[derive(Args)]
struct CalibrateArgs {
    /// How long to measure. Keep the room empty and still the whole time.
    #[arg(long, default_value_t = config::DEFAULT_CALIBRATE_SECONDS)]
    seconds: u64,
    /// Baseline file to update [default: ~/.config/wisense/baseline.json].
    #[arg(long)]
    baseline: Option<PathBuf>,
    #[command(flatten)]
    source: SourceArgs,
    #[command(flatten)]
    detector: DetectorArgs,
}

#[derive(Args)]
struct MonitorArgs {
    /// Baseline file [default: ~/.config/wisense/baseline.json].
    #[arg(long)]
    baseline: Option<PathBuf>,
    #[command(flatten)]
    source: SourceArgs,
    #[command(flatten)]
    detector: DetectorArgs,
    #[command(flatten)]
    output: OutputArgs,
}

#[derive(Args)]
struct RecordArgs {
    /// CSV file to write.
    #[arg(long)]
    out: PathBuf,
    /// Stop after this many seconds [default: until Ctrl+C].
    #[arg(long)]
    seconds: Option<u64>,
    #[command(flatten)]
    source: SourceArgs,
}

#[derive(Args)]
struct ReplayArgs {
    /// Recording made with `wisense record`.
    file: PathBuf,
    /// Playback speed; 2.0 is twice real time, 0 is as fast as possible.
    #[arg(long, default_value_t = 1.0)]
    speed: f64,
    /// Use this baseline file. Without it, the first seconds of the
    /// recording are used to calibrate.
    #[arg(long)]
    baseline: Option<PathBuf>,
    #[command(flatten)]
    detector: DetectorArgs,
    #[command(flatten)]
    output: OutputArgs,
}

#[derive(Args)]
struct DemoArgs {
    /// Length of the synthetic recording, in seconds.
    #[arg(long, default_value_t = 90)]
    seconds: u64,
    /// Playback speed; 2.0 plays twice as fast as real time.
    #[arg(long, default_value_t = 1.0)]
    speed: f64,
    /// Seed for the noise and the motion schedule.
    #[arg(long, default_value_t = 1)]
    seed: u64,
    #[command(flatten)]
    detector: DetectorArgs,
    #[command(flatten)]
    output: OutputArgs,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    init_tracing(cli.verbose);
    match cli.command {
        Command::ListIfaces => cmd_list_ifaces(),
        Command::ListAps(args) => cmd_list_aps(args),
        Command::Discover(args) => cmd_discover(args).await,
        Command::Calibrate(args) => cmd_calibrate(args).await,
        Command::Monitor(args) => cmd_monitor(args).await,
        Command::Record(args) => cmd_record(args).await,
        Command::Replay(args) => cmd_replay(args).await,
        Command::Demo(args) => cmd_demo(args).await,
    }
}

fn init_tracing(verbose: u8) {
    let level = match verbose {
        0 => tracing::Level::INFO,
        1 => tracing::Level::DEBUG,
        _ => tracing::Level::TRACE,
    };
    tracing_subscriber::fmt()
        .with_max_level(level)
        .with_target(false)
        .without_time()
        .with_ansi(std::io::stderr().is_terminal())
        .with_writer(output::LogWriter)
        .init();
}

// ---------------------------------------------------------------- commands

fn cmd_list_ifaces() -> Result<()> {
    let entries = procfs::read_entries(Path::new(procfs::WIRELESS_PATH))?;
    let routes = std::fs::read_to_string(traffic::ROUTE_PATH).unwrap_or_default();
    for e in &entries {
        let signal = match e.signal() {
            Some(Signal::Dbm(v)) => format!("{v} dBm"),
            Some(Signal::Relative(v)) => format!("{v} (relative scale, not dBm)"),
            None => "no data".to_string(),
        };
        let noise = e
            .noise_dbm()
            .map_or_else(String::new, |n| format!(", noise {n} dBm"));
        let gateway_ip = traffic::gateway_for(&routes, &e.iface);
        let gateway = gateway_ip.map_or_else(
            || ", no default gateway".to_string(),
            |g| format!(", gateway {g}"),
        );
        println!(
            "{:<16} signal {signal}, link quality {}{noise}, status 0x{:04x}{gateway}",
            e.iface, e.link_quality, e.status
        );
        if let Some((link, ap)) = discover::read_associated_ap(&e.iface) {
            println!("{:<16} {}", "", discover::describe_ap(&link, ap.as_ref()));
            if let Some(warning) = ap.and_then(|ap| discover::ap_warning(&ap, gateway_ip)) {
                tracing::warn!("{}: {warning}", e.iface);
            }
        }
    }
    let listed: BTreeSet<String> = entries.iter().map(|e| e.iface.clone()).collect();
    let unconnected: Vec<String> = procfs::sysfs_wireless_ifaces()
        .difference(&listed)
        .cloned()
        .collect();
    for iface in &unconnected {
        println!("{iface:<16} not connected");
    }
    if entries.is_empty() && unconnected.is_empty() {
        println!("no wireless interfaces found");
    }
    Ok(())
}

fn cmd_list_aps(args: ListApsArgs) -> Result<()> {
    let iface = match args.iface {
        Some(iface) => iface,
        None => procfs::read_entries(Path::new(procfs::WIRELESS_PATH))?
            .into_iter()
            .map(|e| e.iface)
            .chain(procfs::sysfs_wireless_ifaces())
            .next()
            .context("no wireless interface found; pass --iface")?,
    };
    let aps = scan::scan(&iface)?;
    if aps.is_empty() {
        println!("no access points found");
        return Ok(());
    }
    println!("  {:>8}  {:>6}  {:<17}  SSID", "SIGNAL", "MHz", "BSSID");
    for ap in aps {
        println!(
            "{} {:>8}  {:>6}  {:<17}  {}",
            if ap.associated { '*' } else { ' ' },
            ap.signal_dbm
                .map_or_else(|| "?".to_string(), |s| format!("{s:.0} dBm")),
            ap.freq_mhz
                .map_or_else(|| "?".to_string(), |f| f.to_string()),
            ap.bssid,
            if ap.ssid.is_empty() {
                "(hidden)"
            } else {
                &ap.ssid
            }
        );
    }
    Ok(())
}

async fn cmd_discover(args: DiscoverArgs) -> Result<()> {
    let iface =
        procfs::resolve_iface(Path::new(procfs::WIRELESS_PATH), args.iface.as_deref())?.iface;
    let opts = discover::DiscoverOptions {
        iface,
        count: args.count,
        timeout_s: args.timeout,
        concurrency: args.concurrency,
        allow_large_subnet: args.allow_large_subnet,
    };
    // Dropping the probe on Ctrl+C kills the ping children (kill_on_drop).
    let found = tokio::select! {
        found = discover::discover(&opts) => found?,
        signal = shutdown_signal() => {
            signal?;
            tracing::info!("interrupted, stopping the probe");
            return Ok(());
        }
    };
    tracing::info!(
        "{}: subnet {}, gateway {}, this laptop {}; {} device(s) answered",
        opts.iface,
        found.subnet,
        found
            .gateway
            .map_or_else(|| "none".to_string(), |g| g.to_string()),
        found
            .own_ip
            .map_or_else(|| "unknown".to_string(), |ip| ip.to_string()),
        found.candidates.len()
    );
    if let Some(warning) = found
        .associated
        .and_then(|ap| discover::ap_warning(&ap, found.gateway))
    {
        tracing::warn!("{}: {warning}", opts.iface);
    }
    if args.json {
        let mut out = std::io::stdout().lock();
        for (i, candidate) in found.candidates.iter().enumerate() {
            let device = discover::DeviceJson {
                rank: i + 1,
                candidate,
                tags: discover::tags(candidate),
            };
            serde_json::to_writer(&mut out, &device).context("writing JSON")?;
            writeln!(out).context("writing JSON")?;
        }
        return Ok(());
    }
    if found.candidates.is_empty() {
        println!("no devices answered on {}", found.subnet);
        return Ok(());
    }
    for line in discover::format_table(&found.candidates) {
        println!("{line}");
    }
    Ok(())
}

async fn cmd_calibrate(args: CalibrateArgs) -> Result<()> {
    if args.seconds < 5 {
        bail!("--seconds must be at least 5");
    }
    let mut detector_cfg = args.detector.to_config()?;
    detector_cfg.auto_calibration = Duration::from_secs(args.seconds);
    let path = baseline_path(args.baseline)?;
    let opened = open_sources(&args.source, &args.detector).await?;
    tracing::warn!(
        "calibrating for {} s: put the laptop where it will stay, then LEAVE THE ROOM \
         (pets and fans too, if you can) until this finishes",
        args.seconds
    );

    let links: BTreeSet<String> = opened.setups.keys().cloned().collect();
    let mut sink = HumanSink::stdout();
    let mut handler = DetectHandler::new(
        Detector::new(detector_cfg.clone(), HashMap::new()),
        &mut sink,
    );
    handler.stop_when_calibrated = Some(links.clone());
    // Generous: a link that sends nothing must not block the others forever.
    handler.deadline = Some(Instant::now() + Duration::from_secs(args.seconds + 30));
    let result = run_pipeline(opened.sources, &mut handler).await;
    if let Some(keepalive) = opened.keepalive {
        keepalive.stop().await;
    }
    result?;

    let calibrated = std::mem::take(&mut handler.calibrated);
    if calibrated.is_empty() {
        bail!("calibration did not finish; nothing saved");
    }
    let mut file = BaselineFile::load(&path)?.unwrap_or_default();
    for (link, (baseline, kind)) in &calibrated {
        let Some(setup) = opened.setups.get(link) else {
            continue;
        };
        file.links.insert(
            link.clone(),
            BaselineEntry {
                mean: baseline.mean,
                std: baseline.std,
                kind: *kind,
                metric: setup.metric,
                window_ms: setup.window_ms,
                poll_hz: setup.poll_hz,
                ping_interval_s: setup.ping_interval_s,
                seconds: args.seconds as f64,
                calibrated_unix_ms: sample::unix_ms_now(),
            },
        );
        let unit = if *kind == ScoreKind::Rssi { " dB" } else { "" };
        println!(
            "{link}: idle score {:.3} ± {:.3}{unit} -> threshold {:.3}{unit} at sensitivity {}",
            baseline.mean,
            baseline.std,
            baseline.threshold(&detector_cfg),
            detector_cfg.sensitivity
        );
    }
    for missing in links.iter().filter(|l| !calibrated.contains_key(*l)) {
        tracing::warn!("{missing}: no calibration (no samples?); not saved");
    }
    file.save(&path)?;
    println!("saved to {}", path.display());
    Ok(())
}

async fn cmd_monitor(args: MonitorArgs) -> Result<()> {
    let detector_cfg = args.detector.to_config()?;
    let path = baseline_path(args.baseline)?;
    let opened = open_sources(&args.source, &args.detector).await?;
    let saved = BaselineFile::load(&path)?;
    let baselines = usable_baselines(saved.as_ref(), &opened.setups, &path);
    for link in opened.setups.keys().filter(|l| !baselines.contains_key(*l)) {
        tracing::warn!(
            "no usable saved baseline for {link}: it will auto-calibrate from its first {} s. \
             For a better one, run `wisense calibrate` once with the room empty",
            detector_cfg.auto_calibration.as_secs()
        );
    }
    let detector = Detector::new(detector_cfg, baselines);
    let result = run_detection(opened.sources, detector, &args.output).await;
    if let Some(keepalive) = opened.keepalive {
        keepalive.stop().await;
    }
    result
}

async fn cmd_record(args: RecordArgs) -> Result<()> {
    let opened = open_sources(&args.source, &DetectorArgs::default_values()).await?;
    let mut recorder = Recorder::create(&args.out)?;
    tracing::info!(
        "recording to {}{}; Ctrl+C stops",
        args.out.display(),
        args.seconds
            .map_or_else(String::new, |s| format!(" for {s} s"))
    );
    let mut handler = RecordHandler {
        recorder: &mut recorder,
        until: args
            .seconds
            .map(|s| Instant::now() + Duration::from_secs(s)),
        last_report: Instant::now(),
        per_link: BTreeMap::new(),
    };
    let result = run_pipeline(opened.sources, &mut handler).await;
    let per_link = std::mem::take(&mut handler.per_link);
    if let Some(keepalive) = opened.keepalive {
        keepalive.stop().await;
    }
    recorder.flush()?;
    result?;
    for (link, n) in &per_link {
        tracing::info!("{link}: {n} samples");
    }
    println!(
        "wrote {} samples to {}",
        recorder.rows(),
        args.out.display()
    );
    Ok(())
}

async fn cmd_replay(args: ReplayArgs) -> Result<()> {
    if !(args.speed.is_finite() && args.speed >= 0.0) {
        bail!("--speed must be 0 (as fast as possible) or a positive number");
    }
    let detector_cfg = args.detector.to_config()?;
    let speed = (args.speed > 0.0).then_some(args.speed);
    let source = ReplaySource::open(&args.file, speed)?;
    let baselines = match &args.baseline {
        Some(path) => {
            let file = BaselineFile::load(path)?
                .with_context(|| format!("{} does not exist", path.display()))?;
            // The recording's sampling is unknown; check what we can.
            let setups = file
                .links
                .iter()
                .map(|(link, e)| {
                    let setup = LinkSetup {
                        kind: e.kind,
                        metric: args.detector.metric,
                        window_ms: args.detector.window_ms,
                        poll_hz: e.poll_hz,
                        ping_interval_s: e.ping_interval_s,
                    };
                    (link.clone(), setup)
                })
                .collect();
            usable_baselines(Some(&file), &setups, path)
        }
        None => HashMap::new(),
    };
    let detector = Detector::new(detector_cfg, baselines);
    run_detection(vec![Box::new(source)], detector, &args.output).await
}

async fn cmd_demo(args: DemoArgs) -> Result<()> {
    if !(args.speed.is_finite() && args.speed > 0.0) {
        bail!("--speed must be a positive number");
    }
    let detector_cfg = args.detector.to_config()?;
    let total = Duration::from_secs(args.seconds);
    let quiet = detector_cfg.auto_calibration + Duration::from_secs(5);
    let synthetic = SyntheticConfig {
        bursts: random_bursts(args.seed, quiet, total),
        duration: Some(total),
        seed: args.seed,
        ..SyntheticConfig::default()
    };
    let plan: Vec<String> = synthetic
        .bursts
        .iter()
        .map(|b| {
            format!(
                "+{:.1} s for {:.1} s",
                b.start.as_secs_f64(),
                b.duration.as_secs_f64()
            )
        })
        .collect();
    tracing::info!(
        "demo: synthetic link {}, {} dBm with {} dB noise; simulated motion at: {}",
        synthetic.link_id,
        synthetic.base_dbm,
        synthetic.noise_std_db,
        if plan.is_empty() {
            "none (use a longer --seconds)".to_string()
        } else {
            plan.join(", ")
        }
    );
    let source = SyntheticSource::new(synthetic, Some(args.speed));
    let detector = Detector::new(detector_cfg, HashMap::new());
    run_detection(vec![Box::new(source)], detector, &args.output).await
}

impl DetectorArgs {
    /// The defaults, for commands that open sources without detecting.
    fn default_values() -> Self {
        Self {
            sensitivity: config::DEFAULT_SENSITIVITY,
            window_ms: config::DEFAULT_WINDOW_MS,
            on_ms: config::DEFAULT_ON_MS,
            off_ms: config::DEFAULT_OFF_MS,
            metric: Metric::default(),
        }
    }
}

// ----------------------------------------------------------------- sources

struct OpenedSources {
    sources: Vec<Box<dyn SampleSource>>,
    /// How each link is sampled, for checking and saving baselines.
    setups: BTreeMap<String, LinkSetup>,
    keepalive: Option<Keepalive>,
}

/// The laptop link (unless `--no-wifi`) plus one source per ESP32 board.
async fn open_sources(args: &SourceArgs, detector: &DetectorArgs) -> Result<OpenedSources> {
    let mut sources: Vec<Box<dyn SampleSource>> = Vec::new();
    let mut setups = BTreeMap::new();
    let mut keepalive = None;

    if !args.no_wifi {
        match procfs::resolve_iface(Path::new(procfs::WIRELESS_PATH), args.iface.as_deref()) {
            Ok(entry) => {
                let iface = entry.iface.clone();
                warn_if_ap_is_not_gateway(&iface);
                keepalive = start_keepalive(args, &iface).await?;
                let source = open_wifi(args, &iface, keepalive.is_some()).await?;
                tracing::info!(
                    "sensing on {iface} ({:?}, signal {}) at {} Hz",
                    args.rssi_source,
                    entry
                        .signal()
                        .map_or_else(|| "n/a".to_string(), |s| s.value().to_string()),
                    args.poll_hz
                );
                sources.push(source);
                setups.insert(
                    iface,
                    LinkSetup {
                        kind: ScoreKind::Rssi,
                        metric: detector.metric,
                        window_ms: detector.window_ms,
                        poll_hz: Some(args.poll_hz),
                        ping_interval_s: keepalive.as_ref().map(|_| args.ping_interval),
                    },
                );
            }
            Err(e) if !args.esp32.is_empty() && args.iface.is_none() => {
                tracing::warn!("{e:#}; continuing with the ESP32 boards only");
            }
            Err(e) => return Err(e),
        }
    }

    for spec in &args.esp32 {
        if setups.contains_key(&spec.name) {
            bail!("link name {:?} is used twice", spec.name);
        }
        sources.push(open_esp32(spec, args.baud)?);
        tracing::info!("sensing on ESP32 '{}' at {}", spec.name, spec.port);
        setups.insert(
            spec.name.clone(),
            LinkSetup {
                kind: ScoreKind::Csi,
                metric: detector.metric,
                window_ms: detector.window_ms,
                poll_hz: None,
                ping_interval_s: None,
            },
        );
    }
    if sources.is_empty() {
        bail!("nothing to sense with");
    }
    Ok(OpenedSources {
        sources,
        setups,
        keepalive,
    })
}

async fn open_wifi(
    args: &SourceArgs,
    iface: &str,
    keepalive: bool,
) -> Result<Box<dyn SampleSource>> {
    match args.rssi_source {
        RssiSource::Procfs => Ok(Box::new(ProcfsSource::new(ProcfsConfig {
            iface: iface.to_string(),
            poll_hz: args.poll_hz,
            path: PathBuf::from(procfs::WIRELESS_PATH),
            keepalive,
        })?)),
        RssiSource::Nl80211 => open_nl80211(args, iface, keepalive).await,
    }
}

#[cfg(feature = "nl80211")]
async fn open_nl80211(
    args: &SourceArgs,
    iface: &str,
    keepalive: bool,
) -> Result<Box<dyn SampleSource>> {
    use source::nl80211::{Nl80211Config, Nl80211Source};
    let source = Nl80211Source::open(Nl80211Config {
        iface: iface.to_string(),
        poll_hz: args.poll_hz,
        keepalive,
    })
    .await?;
    Ok(Box::new(source))
}

#[cfg(not(feature = "nl80211"))]
async fn open_nl80211(
    _args: &SourceArgs,
    _iface: &str,
    _keepalive: bool,
) -> Result<Box<dyn SampleSource>> {
    bail!(
        "this build has no nl80211 support; rebuild with `cargo build --release --features \
         nl80211`, or use --rssi-source procfs"
    )
}

#[cfg(feature = "esp32")]
fn open_esp32(spec: &Esp32Spec, baud: u32) -> Result<Box<dyn SampleSource>> {
    use source::esp32::{Esp32Config, Esp32Source};
    let source = Esp32Source::open(Esp32Config {
        port: spec.port.clone(),
        name: spec.name.clone(),
        baud,
    })?;
    Ok(Box::new(source))
}

#[cfg(not(feature = "esp32"))]
fn open_esp32(_spec: &Esp32Spec, _baud: u32) -> Result<Box<dyn SampleSource>> {
    bail!("this build has no ESP32 support; rebuild with `cargo build --release --features esp32`")
}

/// The sensed path runs to the access point the laptop is associated with;
/// say so when that is not the router at the gateway address.
fn warn_if_ap_is_not_gateway(iface: &str) {
    let Some((_, Some(ap))) = discover::read_associated_ap(iface) else {
        return;
    };
    let gateway = traffic::read_gateway(Path::new(traffic::ROUTE_PATH), iface)
        .ok()
        .flatten();
    if let Some(warning) = discover::ap_warning(&ap, gateway) {
        tracing::warn!("{iface}: {warning}");
    }
}

/// Start pinging the access point unless disabled. Without a gateway on the
/// interface (and no explicit target) we warn and carry on without.
async fn start_keepalive(args: &SourceArgs, iface: &str) -> Result<Option<Keepalive>> {
    if args.no_keepalive {
        tracing::warn!("keepalive disabled: RSSI only updates when frames happen to arrive");
        return Ok(None);
    }
    let gateway = traffic::read_gateway(Path::new(traffic::ROUTE_PATH), iface)?;
    let target = match args.keepalive_target {
        Some(KeepaliveTarget::Ip(target)) => target,
        Some(KeepaliveTarget::Auto) => traffic::auto_target(iface, gateway).await?,
        None => match gateway {
            Some(gateway) => IpAddr::V4(gateway),
            None => {
                tracing::warn!(
                    "no default gateway on {iface}, so no keepalive traffic; pass \
                     --keepalive-target <IP of your access point>"
                );
                return Ok(None);
            }
        },
    };
    let keepalive = Keepalive::start(target, args.ping_interval)?;
    tracing::info!("keepalive: pinging {target} every {} s", args.ping_interval);
    Ok(Some(keepalive))
}

// --------------------------------------------------------------- baselines

fn baseline_path(explicit: Option<PathBuf>) -> Result<PathBuf> {
    match explicit {
        Some(path) => Ok(path),
        None => config::default_baseline_path(),
    }
}

/// Saved baselines that fit how each link is sampled now; explains the rest.
fn usable_baselines(
    file: Option<&BaselineFile>,
    setups: &BTreeMap<String, LinkSetup>,
    path: &Path,
) -> HashMap<String, Baseline> {
    let mut usable = HashMap::new();
    let Some(file) = file else {
        return usable;
    };
    for (link, setup) in setups {
        let Some(entry) = file.links.get(link) else {
            continue;
        };
        match entry.check(setup) {
            Ok(warnings) => {
                for w in warnings {
                    tracing::warn!("{link}: {w}");
                }
                tracing::info!(
                    "{link}: using baseline from {} (idle score {:.3} ± {:.3})",
                    path.display(),
                    entry.mean,
                    entry.std
                );
                usable.insert(link.clone(), entry.baseline());
            }
            Err(reason) => tracing::warn!(
                "{link}: ignoring the saved baseline because {reason}; recalibrate with the \
                 same settings"
            ),
        }
    }
    usable
}

// ---------------------------------------------------------------- pipeline

enum Flow {
    Continue,
    Stop,
}

/// What to do with the sample stream.
trait Handler {
    fn sample(&mut self, sample: Sample) -> Result<Flow>;
    /// About ten times a second, also when no samples arrive.
    fn tick(&mut self) -> Result<Flow>;
    fn finish(&mut self) -> Result<()>;
}

/// Detector + zone tracker + event output.
struct DetectHandler<'a> {
    detector: Detector,
    zones: ZoneTracker,
    sink: &'a mut dyn EventSink,
    /// Calibration results seen so far.
    calibrated: BTreeMap<String, (Baseline, ScoreKind)>,
    /// Stop once all of these links have calibrated (`wisense calibrate`).
    stop_when_calibrated: Option<BTreeSet<String>>,
    deadline: Option<Instant>,
}

impl<'a> DetectHandler<'a> {
    fn new(detector: Detector, sink: &'a mut dyn EventSink) -> Self {
        Self {
            detector,
            zones: ZoneTracker::new(config::DEFAULT_ZONE_HOLD_MS),
            sink,
            calibrated: BTreeMap::new(),
            stop_when_calibrated: None,
            deadline: None,
        }
    }
}

impl Handler for DetectHandler<'_> {
    fn sample(&mut self, sample: Sample) -> Result<Flow> {
        for event in self.detector.process(sample) {
            let zone = self.zones.observe(&event);
            if let Event::Calibrated {
                link_id,
                baseline,
                kind,
                ..
            } = &event
            {
                self.calibrated.insert(link_id.clone(), (*baseline, *kind));
            }
            self.sink.handle(&event)?;
            if let Some(zone) = zone {
                self.sink.handle(&zone)?;
            }
        }
        let done = self
            .stop_when_calibrated
            .as_ref()
            .is_some_and(|links| links.iter().all(|l| self.calibrated.contains_key(l)));
        Ok(if done { Flow::Stop } else { Flow::Continue })
    }

    fn tick(&mut self) -> Result<Flow> {
        if self.sink.tick()? {
            return Ok(Flow::Stop);
        }
        if self.deadline.is_some_and(|d| Instant::now() >= d) {
            tracing::warn!("giving up waiting for links that send no samples");
            return Ok(Flow::Stop);
        }
        Ok(Flow::Continue)
    }

    fn finish(&mut self) -> Result<()> {
        self.sink.finish()
    }
}

/// Writes every sample to CSV.
struct RecordHandler<'a> {
    recorder: &'a mut Recorder<std::fs::File>,
    until: Option<Instant>,
    last_report: Instant,
    per_link: BTreeMap<String, u64>,
}

impl Handler for RecordHandler<'_> {
    fn sample(&mut self, sample: Sample) -> Result<Flow> {
        self.recorder.write(&sample)?;
        *self.per_link.entry(sample.link_id).or_default() += 1;
        Ok(Flow::Continue)
    }

    fn tick(&mut self) -> Result<Flow> {
        let now = Instant::now();
        if self.until.is_some_and(|u| now >= u) {
            return Ok(Flow::Stop);
        }
        if now.duration_since(self.last_report) >= Duration::from_secs(10) {
            self.last_report = now;
            tracing::info!("{} samples recorded", self.recorder.rows());
        }
        Ok(Flow::Continue)
    }

    fn finish(&mut self) -> Result<()> {
        self.recorder.flush()
    }
}

/// Detection with the output chosen by `--json` / `--tui`.
async fn run_detection(
    sources: Vec<Box<dyn SampleSource>>,
    detector: Detector,
    output: &OutputArgs,
) -> Result<()> {
    let mut sink = make_sink(output)?;
    let mut handler = DetectHandler::new(detector, sink.as_mut());
    run_pipeline(sources, &mut handler).await
}

fn make_sink(output: &OutputArgs) -> Result<Box<dyn EventSink>> {
    if output.json {
        return Ok(Box::new(JsonSink::new(std::io::stdout())));
    }
    if output.tui {
        return tui_sink();
    }
    Ok(Box::new(HumanSink::stdout()))
}

#[cfg(feature = "tui")]
fn tui_sink() -> Result<Box<dyn EventSink>> {
    Ok(Box::new(output::tui::TuiSink::start()?))
}

#[cfg(not(feature = "tui"))]
fn tui_sink() -> Result<Box<dyn EventSink>> {
    bail!("this build has no terminal UI; rebuild with `cargo build --release --features tui`")
}

/// Run every source in its own task and feed their samples to `handler`
/// until all sources end, the handler says stop, or the user interrupts.
/// A failing source is reported; the others keep running.
async fn run_pipeline(
    sources: Vec<Box<dyn SampleSource>>,
    handler: &mut dyn Handler,
) -> Result<()> {
    let (tx, mut rx) = mpsc::channel(4096);
    let mut tasks = JoinSet::new();
    for s in sources {
        tasks.spawn(source::run(s, tx.clone()));
    }
    drop(tx);

    let mut ticker = tokio::time::interval(Duration::from_millis(100));
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let shutdown = shutdown_signal();
    tokio::pin!(shutdown);
    // (error, already logged)
    let mut failures: Vec<(anyhow::Error, bool)> = Vec::new();

    let result = loop {
        tokio::select! {
            sample = rx.recv() => match sample {
                Some(sample) => match handler.sample(sample) {
                    Ok(Flow::Continue) => {}
                    Ok(Flow::Stop) => break Ok(()),
                    Err(e) => break Err(e),
                },
                None => break Ok(()),
            },
            Some(joined) = tasks.join_next() => {
                if let Some(e) = task_error(joined) {
                    // With other sources still running, say so now.
                    let log_now = !tasks.is_empty();
                    if log_now {
                        tracing::error!("{e:#}; continuing with the other sources");
                    }
                    failures.push((e, log_now));
                }
            }
            _ = ticker.tick() => match handler.tick() {
                Ok(Flow::Continue) => {}
                Ok(Flow::Stop) => break Ok(()),
                Err(e) => break Err(e),
            },
            signal = &mut shutdown => {
                if let Err(e) = signal {
                    break Err(e);
                }
                tracing::info!("interrupted, shutting down");
                break Ok(());
            }
        }
    };
    let finished = handler.finish();

    tasks.abort_all();
    while let Some(joined) = tasks.join_next().await {
        if let Some(e) = task_error(joined) {
            failures.push((e, false));
        }
    }
    result?;
    finished?;
    let logged = failures.iter().filter(|(_, logged)| *logged).count();
    match failures.into_iter().find(|(_, logged)| !logged) {
        Some((e, _)) => Err(e),
        None if logged > 0 => Err(anyhow!("{logged} source(s) failed; see above")),
        None => Ok(()),
    }
}

fn task_error(joined: Result<Result<()>, JoinError>) -> Option<anyhow::Error> {
    match joined {
        Ok(Ok(())) => None,
        Ok(Err(e)) => Some(e),
        Err(e) if e.is_cancelled() => None,
        Err(e) => Some(anyhow!("a source task panicked: {e}")),
    }
}

/// Resolves on Ctrl+C, SIGTERM or SIGHUP.
async fn shutdown_signal() -> Result<()> {
    let mut term = signal(SignalKind::terminate()).context("installing SIGTERM handler")?;
    let mut hup = signal(SignalKind::hangup()).context("installing SIGHUP handler")?;
    tokio::select! {
        r = tokio::signal::ctrl_c() => r.context("waiting for Ctrl+C"),
        _ = term.recv() => Ok(()),
        _ = hup.recv() => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn esp32_specs() {
        assert_eq!(
            parse_esp32_spec("/dev/ttyUSB0=kitchen"),
            Ok(Esp32Spec {
                port: "/dev/ttyUSB0".into(),
                name: "kitchen".into()
            })
        );
        assert!(parse_esp32_spec("/dev/ttyUSB0").is_err());
        assert!(parse_esp32_spec("/dev/ttyUSB0=").is_err());
        assert!(parse_esp32_spec("=kitchen").is_err());
        assert!(parse_esp32_spec("/dev/ttyUSB0=living room").is_err());
    }

    #[test]
    fn cli_is_consistent() {
        use clap::CommandFactory;
        Cli::command().debug_assert();
    }
}
