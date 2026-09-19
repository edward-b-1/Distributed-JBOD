//! `djbod-ui`: serve the administration web UI (SPEC 20.3) for one
//! cluster.
//!
//! ```text
//! djbod-ui --node 10.0.0.1:5263 --cluster <uuid> [--listen 127.0.0.1:5264]
//! ```
//!
//! `--node` and `--cluster` may also come from `DJBOD_NODE` and
//! `DJBOD_CLUSTER`, as for the `djbod` client. The server has no
//! authentication and listens on localhost unless told otherwise.

use std::net::SocketAddr;
use std::process::ExitCode;

use anyhow::Context;
use clap::Parser;
use uuid::Uuid;

use djbod_ui::{router, Target};

#[derive(Parser)]
#[command(
    name = "djbod-ui",
    about = "Distributed-JBOD administration web UI",
    version
)]
struct Cli {
    /// Address of any node in the cluster.
    #[arg(long, env = "DJBOD_NODE")]
    node: SocketAddr,
    /// The cluster id, as printed by `djbod-node init-cluster`.
    #[arg(long, env = "DJBOD_CLUSTER")]
    cluster: Uuid,
    /// Address to serve the UI on.
    #[arg(long, default_value = "127.0.0.1:5264")]
    listen: SocketAddr,
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::FAILURE
        }
    }
}

async fn run(cli: Cli) -> anyhow::Result<()> {
    let target = Target {
        node: cli.node,
        cluster: cli.cluster,
    };
    let listener = tokio::net::TcpListener::bind(cli.listen)
        .await
        .with_context(|| format!("listening on {}", cli.listen))?;
    let address = listener.local_addr()?;
    eprintln!(
        "djbod-ui serving http://{address}/ for cluster {} via node {}",
        target.cluster, target.node
    );
    axum::serve(listener, router(target))
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await
        .context("serving")?;
    Ok(())
}
