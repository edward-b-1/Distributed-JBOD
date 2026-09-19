//! `djbod-ui`: serve the administration web UI (SPEC 20.3) for one
//! cluster.
//!
//! ```text
//! djbod-ui --node 10.0.0.1:5263 --cluster <uuid> [--listen 127.0.0.1:5264]
//! ```
//!
//! `--node`, `--cluster`, and the `--tls-*` settings may also come from
//! the same environment variables as for the `djbod` client, and mean
//! the same. The HTTP side has no authentication and listens on
//! localhost unless told otherwise.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::Context;
use clap::Parser;
use uuid::Uuid;

use djbod_node::transport::Connector;
use djbod_ui::{router_for_hosts, Target};

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
    /// The cluster's PEM certificate authority (or bundle). Given alone,
    /// connections to the node are encrypted but present no certificate.
    #[arg(long, env = "DJBOD_TLS_CA")]
    tls_ca: Option<PathBuf>,
    /// This client's PEM certificate; needs --tls-key and --tls-ca.
    #[arg(long, env = "DJBOD_TLS_CERT", requires_all = ["tls_key", "tls_ca"])]
    tls_cert: Option<PathBuf>,
    /// This client's PEM private key, readable only by its owner.
    #[arg(long, env = "DJBOD_TLS_KEY", requires_all = ["tls_cert", "tls_ca"])]
    tls_key: Option<PathBuf>,
    /// A host name this server answers to, besides IP addresses and
    /// localhost, for example the machine's DNS name. Repeatable. A
    /// request for any other name is refused, since a name pointed at
    /// this address by someone else would let their page act as this one.
    #[arg(long = "host", value_name = "NAME")]
    hosts: Vec<String>,
}

/// How the UI connects to the node (SPEC 19.1.6.2), decided by the
/// transport module exactly as the `djbod` client decides it.
fn connector(cli: &Cli) -> anyhow::Result<Connector> {
    Connector::from_client_options(
        cli.tls_ca.as_deref(),
        cli.tls_cert.as_deref(),
        cli.tls_key.as_deref(),
    )
    .map_err(|e| anyhow::anyhow!("{e}"))
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
        connector: connector(&cli)?,
    };
    let listener = tokio::net::TcpListener::bind(cli.listen)
        .await
        .with_context(|| format!("listening on {}", cli.listen))?;
    let address = listener.local_addr()?;
    eprintln!(
        "djbod-ui serving http://{address}/ for cluster {} via node {}{}",
        target.cluster,
        target.node,
        if cli.tls_ca.is_some() {
            " over TLS"
        } else {
            ""
        }
    );
    axum::serve(listener, router_for_hosts(target, cli.hosts.clone()))
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await
        .context("serving")?;
    Ok(())
}
