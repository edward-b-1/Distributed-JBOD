use anyhow::Result;
use clap::Parser;
use djbod_bitrotter::{config::WorkerConfig, network, signals, Stop, TEST_WARNING};
use std::{path::PathBuf, process::ExitCode};

#[derive(Parser)]
#[command(
    version,
    about = "Serve disposable test devices for coordinated bitrot testing (port 6666)"
)]
struct Cli {
    /// Worker configuration file (TOML).
    #[arg(long)]
    config: PathBuf,
}

async fn execute(cli: Cli) -> Result<()> {
    eprintln!("WARNING: {TEST_WARNING}");
    let config = WorkerConfig::load(&cli.config)?;
    let stop = Stop::default();
    tokio::spawn(signals(stop.clone()));
    let listener = tokio::net::TcpListener::bind(config.listen).await?;
    eprintln!(
        "bitrotter worker {} listening on {} (mutual TLS)",
        config.node,
        listener.local_addr()?
    );
    network::serve(config, listener, stop).await
}

#[tokio::main]
async fn main() -> ExitCode {
    match execute(Cli::parse()).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error:#}");
            ExitCode::FAILURE
        }
    }
}
