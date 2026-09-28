use anyhow::{bail, ensure, Context, Result};
use clap::{Args, Parser, Subcommand};
use djbod_bitrotter::{
    config::CoordinatorConfig,
    coordinator::{self, Limits, Selection},
    model::{Consent, Plan},
    network, signals, Stop, LOSS_WARNING, TEST_WARNING,
};
use std::{
    io::{IsTerminal, Write},
    net::SocketAddr,
    path::PathBuf,
    process::ExitCode,
    time::Duration,
};

#[derive(Parser)]
#[command(
    version,
    about = "Deliberate corruption of disposable test data; never a production service"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Read cluster placement and save a deterministic plan without damaging data.
    Plan(PlanArgs),
    /// Confirm and execute a saved plan, or reconcile an interrupted run.
    Run(RunArgs),
    /// Print the SHA-256 fingerprint used by the worker controller allowlist.
    Fingerprint { cert: PathBuf },
}

#[derive(Args)]
struct PlanArgs {
    #[arg(long, required = true)]
    bootstrap_node: Vec<String>,
    #[arg(long)]
    workers: PathBuf,
    #[arg(long)]
    key: Vec<String>,
    #[arg(long)]
    prefix: Option<String>,
    /// Distinct shard indices to damage per object version, across all nodes.
    #[arg(long)]
    shards: u16,
    #[arg(long, default_value_t = 0)]
    seed: u64,
    /// Restrict selection to these indices; repeat for each allowed index.
    #[arg(long)]
    shard_index: Vec<u8>,
    #[arg(long)]
    out: PathBuf,
    #[arg(long)]
    json: bool,
}

#[derive(Args)]
struct RunArgs {
    #[arg(long)]
    plan: PathBuf,
    #[arg(long)]
    journal: PathBuf,
    #[arg(long)]
    resume: bool,
    #[arg(long, conflicts_with_all = ["duration", "continuous"])]
    events: Option<u64>,
    #[arg(long, value_parser = parse_duration, conflicts_with = "continuous")]
    duration: Option<Duration>,
    #[arg(long)]
    continuous: bool,
    #[arg(long, value_parser = parse_duration)]
    interval: Option<Duration>,
    #[arg(long)]
    max_mutations: Option<u64>,
    /// Explicit unattended acknowledgement, bound to the complete plan ID.
    #[arg(long)]
    confirm_test_damage: Option<String>,
    /// Additional acknowledgement required when any version has n > m.
    #[arg(long)]
    confirm_data_loss: Option<String>,
    #[arg(long)]
    json: bool,
}

fn parse_duration(value: &str) -> std::result::Result<Duration, String> {
    let (number, multiplier) = if let Some(n) = value.strip_suffix("ms") {
        (n, 1)
    } else if let Some(n) = value.strip_suffix('s') {
        (n, 1000)
    } else if let Some(n) = value.strip_suffix('m') {
        (n, 60_000)
    } else if let Some(n) = value.strip_suffix('h') {
        (n, 3_600_000)
    } else {
        (value, 1000)
    };
    let amount: u64 = number
        .parse()
        .map_err(|_| "duration must be an integer followed by ms, s, m, or h")?;
    let millis = amount.checked_mul(multiplier).ok_or("duration overflow")?;
    if millis == 0 {
        return Err("duration must be positive".into());
    }
    Ok(Duration::from_millis(millis))
}

fn show_plan(plan: &Plan) {
    eprintln!(
        "Plan {}: cluster {}, {} workers, {} object versions, n={}",
        plan.id,
        plan.document.cluster_id,
        plan.workers.len(),
        plan.objects.len(),
        plan.n
    );
    for worker in &plan.workers {
        if let Ok(endpoint) = plan.endpoint(worker.node) {
            eprintln!(
                "  worker {} at {}, journal {}, {} allowed devices",
                worker.node,
                endpoint.address,
                worker.journal_id,
                worker.devices.len()
            );
        }
    }
    eprintln!("  Full k+m device coverage is required for every targeted version.");
    for object in &plan.objects {
        eprintln!(
            "  {} version {} revision {}: {}+{}, selected {:?}{}",
            object.record.key,
            object.record.version,
            object.record.revision,
            object.record.k,
            object.record.m,
            object.selected,
            if plan.n > u16::from(object.record.m) {
                " — CERTAIN DATA LOSS"
            } else {
                ""
            }
        );
        for index in &object.selected {
            let device = object
                .record
                .device_for(djbod_core::erasure::ShardIndex(*index));
            if let Some(device) = device {
                for worker in &plan.workers {
                    if let Some(info) = worker.devices.iter().find(|d| d.device == device) {
                        eprintln!(
                            "    shard {index}: {} on {} ({})",
                            device,
                            worker.node,
                            info.path.display()
                        );
                    }
                }
            }
        }
    }
    if plan.destructive() {
        eprintln!("WARNING: {LOSS_WARNING}");
    }
}

async fn prompt(expected: String, stop: &Stop) -> Result<()> {
    ensure!(
        std::io::stdin().is_terminal(),
        "no interactive terminal; provide explicit plan-bound confirmation flags"
    );
    eprint!("Type {expected} to continue (60 second timeout): ");
    std::io::stderr().flush()?;
    let (sender, receiver) = tokio::sync::oneshot::channel();
    // A detached OS thread avoids trapping Tokio's blocking pool in a
    // stdin read when the confirmation times out.
    std::thread::spawn(move || {
        let mut line = String::new();
        let result = std::io::stdin().read_line(&mut line).map(|_| line);
        let _ = sender.send(result);
    });
    let line = tokio::select! {
        _ = stop.cancelled() => bail!("confirmation cancelled; no data changed"),
        result = tokio::time::timeout(Duration::from_secs(60), receiver) =>
            result.context("confirmation timed out; no data changed")???,
    };
    ensure!(
        line.trim_end() == expected,
        "confirmation declined; no data changed"
    );
    Ok(())
}

async fn confirm(plan: &Plan, args: &RunArgs, stop: &Stop) -> Result<Consent> {
    show_plan(plan);
    if let Some(id) = &args.confirm_test_damage {
        ensure!(
            id == &plan.id,
            "test-damage acknowledgement names a different plan"
        );
    } else {
        prompt(format!("DAMAGE TEST DATA {}", plan.id), stop).await?;
    }
    if let Some(id) = &args.confirm_data_loss {
        ensure!(
            id == &plan.id,
            "data-loss acknowledgement names a different plan"
        );
    } else if plan.destructive() {
        prompt(format!("ACCEPT CERTAIN DATA LOSS {}", plan.id), stop).await?;
    }
    let consent = Consent {
        plan_id: plan.id.clone(),
        test_damage: true,
        data_loss: plan.destructive() || args.confirm_data_loss.is_some(),
    };
    consent.validate(plan)?;
    Ok(consent)
}

async fn execute(cli: Cli) -> Result<ExitCode> {
    if !matches!(&cli.command, Command::Fingerprint { .. }) {
        eprintln!("WARNING: {TEST_WARNING}");
    }
    let stop = Stop::default();
    tokio::spawn(signals(stop.clone()));
    match cli.command {
        Command::Plan(args) => {
            let config = CoordinatorConfig::load(&args.workers)?;
            let mut bootstrap: Vec<SocketAddr> = Vec::new();
            for address in args.bootstrap_node {
                bootstrap.extend(
                    tokio::net::lookup_host(&address)
                        .await
                        .with_context(|| format!("resolving {address}"))?,
                );
            }
            let plan = coordinator::create_plan(
                bootstrap,
                config,
                Selection {
                    n: args.shards,
                    seed: args.seed,
                    keys: args.key,
                    prefix: args.prefix,
                    shard_indices: args.shard_index,
                },
            )
            .await?;
            show_plan(&plan);
            coordinator::save_plan(&args.out, &plan)?;
            if args.json {
                println!("{}", serde_json::to_string(&plan)?);
            } else {
                println!("Saved plan {} to {}", plan.id, args.out.display());
            }
        }
        Command::Run(args) => {
            let plan = coordinator::load_plan(&args.plan)?;
            let limits = if args.resume {
                ensure!(
                    args.events.is_none()
                        && args.duration.is_none()
                        && !args.continuous
                        && args.interval.is_none()
                        && args.max_mutations.is_none(),
                    "resume uses the original run limits"
                );
                coordinator::load_limits(&args.journal)?
            } else {
                Limits {
                    events: args.events.or_else(|| {
                        if args.duration.is_none() && !args.continuous {
                            Some(1)
                        } else {
                            None
                        }
                    }),
                    duration_secs: args.duration.map(|d| d.as_secs()),
                    continuous: args.continuous,
                    interval_millis: args
                        .interval
                        .unwrap_or(Duration::from_secs(1))
                        .as_millis()
                        .try_into()?,
                    max_mutations: args.max_mutations,
                }
            };
            limits.validate()?;
            let consent = confirm(&plan, &args, &stop).await?;
            let summary = coordinator::run(
                &plan,
                consent,
                &args.journal,
                limits,
                args.resume,
                stop,
                |event| {
                    if args.json {
                        println!(
                            "{}",
                            serde_json::to_string(event).expect("event serialization")
                        );
                    } else {
                        println!(
                            "{}: event {}, stripe {}, {}, bad shards {:?}",
                            event.key,
                            event.event.sequence,
                            event.event.stripe,
                            event.status,
                            event.bad_shards
                        );
                    }
                },
            )
            .await?;
            if args.json {
                println!("{}", serde_json::to_string(&summary)?);
            } else {
                println!(
                    "Run {}: {} complete, {} skipped; journal {}",
                    summary.run,
                    summary.completed,
                    summary.skipped,
                    args.journal.display()
                );
            }
            if summary.stopped {
                return Ok(ExitCode::from(130));
            }
        }
        Command::Fingerprint { cert } => println!("{}", network::certificate_fingerprint(&cert)?),
    }
    Ok(ExitCode::SUCCESS)
}

#[tokio::main]
async fn main() -> ExitCode {
    match execute(Cli::parse()).await {
        Ok(code) => code,
        Err(error) => {
            eprintln!("error: {error:#}");
            ExitCode::FAILURE
        }
    }
}
