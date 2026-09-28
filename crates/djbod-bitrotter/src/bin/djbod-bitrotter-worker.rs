use anyhow::{Context, Result};
use clap::{Args, Parser};
use djbod_bitrotter::{
    config::{self, WorkerConfig},
    network, signals, Stop, TEST_WARNING,
};
use djbod_client::transport::TlsPaths;
use djbod_core::cluster::{NodeId, Transport};
use std::{net::SocketAddr, path::PathBuf, process::ExitCode};
use uuid::Uuid;

/// Same complete server identity and precedence as djbod-node.
#[derive(Args)]
struct TlsArgs {
    /// This worker's PEM certificate, naming its listening address.
    #[arg(long, env = "DJBOD_TLS_CERT", requires_all = ["tls_key", "tls_ca"])]
    tls_cert: Option<PathBuf>,
    /// This worker's PEM private key, readable only by its owner.
    #[arg(long, env = "DJBOD_TLS_KEY", requires_all = ["tls_cert", "tls_ca"])]
    tls_key: Option<PathBuf>,
    /// PEM certificate authority, or a bundle.
    #[arg(long, env = "DJBOD_TLS_CA", requires_all = ["tls_cert", "tls_key"])]
    tls_ca: Option<PathBuf>,
}

#[derive(Parser)]
#[command(
    version = djbod_client::BUILD,
    about = "Serve disposable test devices for coordinated bitrot testing (port 6666)"
)]
struct Cli {
    /// Worker configuration file (TOML).
    #[arg(long, env = "DJBOD_CONFIG")]
    config: Option<PathBuf>,
    /// The product node's UUID.
    #[arg(long, env = "DJBOD_NODE_ID")]
    node_id: Option<Uuid>,
    /// The cluster's UUID.
    #[arg(long, env = "DJBOD_CLUSTER")]
    cluster: Option<Uuid>,
    /// Listen address; default 0.0.0.0:6666.
    #[arg(long, env = "DJBOD_LISTEN")]
    listen: Option<SocketAddr>,
    /// Allowed device root; repeat or use a comma-separated list.
    #[arg(long = "device", env = "DJBOD_DEVICES", value_delimiter = ',')]
    devices: Vec<PathBuf>,
    /// Durable worker journal, outside every device root.
    #[arg(long, env = "DJBOD_BITROTTER_JOURNAL")]
    journal: Option<PathBuf>,
    /// Accepted transport: plain (default), tls-optional, or tls.
    #[arg(long, env = "DJBOD_TRANSPORT")]
    transport: Option<Transport>,
    /// Optional controller certificate fingerprint allowlist; repeat or use commas.
    #[arg(
        long = "allowed-controller",
        env = "DJBOD_BITROTTER_ALLOWED_CONTROLLERS",
        value_delimiter = ','
    )]
    allowed_controllers: Vec<String>,
    #[command(flatten)]
    tls: TlsArgs,
}

impl Cli {
    fn resolve(&self) -> Result<WorkerConfig> {
        let mut config = match &self.config {
            Some(path) => WorkerConfig::read(path)?,
            None => WorkerConfig {
                node: NodeId(
                    self.node_id
                        .context("no configuration file: --node-id or DJBOD_NODE_ID is required")?,
                ),
                cluster: self
                    .cluster
                    .context("no configuration file: --cluster or DJBOD_CLUSTER is required")?,
                listen: config::default_listen(),
                devices: Vec::new(),
                journal: self.journal.clone().context(
                    "no configuration file: --journal or DJBOD_BITROTTER_JOURNAL is required",
                )?,
                transport: Transport::Plain,
                tls: None,
                allowed_controllers: Vec::new(),
            },
        };
        if let Some(node) = self.node_id {
            config.node = NodeId(node);
        }
        if let Some(cluster) = self.cluster {
            config.cluster = cluster;
        }
        if let Some(listen) = self.listen {
            config.listen = listen;
        }
        if !self.devices.is_empty() {
            config.devices = self.devices.clone();
        }
        if let Some(journal) = &self.journal {
            config.journal = journal.clone();
        }
        if let Some(transport) = self.transport {
            config.transport = transport;
        }
        if !self.allowed_controllers.is_empty() {
            config.allowed_controllers = self.allowed_controllers.clone();
        }
        if let (Some(cert), Some(key), Some(ca)) =
            (&self.tls.tls_cert, &self.tls.tls_key, &self.tls.tls_ca)
        {
            config.tls = Some(TlsPaths {
                cert: cert.clone(),
                key: key.clone(),
                ca: ca.clone(),
            });
        }
        let cwd = std::env::current_dir()?;
        for device in &mut config.devices {
            config::resolve(&cwd, device);
        }
        config::resolve(&cwd, &mut config.journal);
        if let Some(tls) = &mut config.tls {
            config::resolve_tls(&cwd, tls);
        }
        config.validate().context("worker configuration")?;
        Ok(config)
    }
}

async fn execute(cli: Cli) -> Result<()> {
    eprintln!("WARNING: {TEST_WARNING}");
    let config = cli.resolve()?;
    let stop = Stop::default();
    tokio::spawn(signals(stop.clone()));
    let listener = tokio::net::TcpListener::bind(config.listen).await?;
    eprintln!(
        "bitrotter worker {} listening on {} (transport {})",
        config.node,
        listener.local_addr()?,
        config.transport,
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
