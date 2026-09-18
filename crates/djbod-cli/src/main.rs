//! `djbod`: the command-line client (SPEC C.2).
//!
//! ```text
//! djbod --node 10.0.0.1:5263 --cluster <uuid> status
//! djbod ... put photos/cat.jpg ./cat.jpg
//! djbod ... get photos/cat.jpg ./copy.jpg
//! djbod ... head photos/cat.jpg
//! djbod ... list --prefix photos/
//! djbod ... delete photos/cat.jpg
//! djbod ... cluster-config
//! ```
//!
//! `--node` and `--cluster` may also come from `DJBOD_NODE` and
//! `DJBOD_CLUSTER`. `init-cluster` prints the cluster id to use.
//!
//! Object bodies stream: `put` reads the file a chunk at a time, `get`
//! writes as chunks arrive. A failed `get` leaves a partial output file
//! and says so; it is removed when the output is a named file.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{bail, Context};
use clap::{Parser, Subcommand};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use uuid::Uuid;

use djbod_node::client::{ClientError, Connection, DEFAULT_BODY_CHUNK};
use djbod_proto::message::{ErrorDetail, ListQuery, Request, Response};

#[derive(Parser)]
#[command(name = "djbod", about = "Distributed-JBOD client", version)]
struct Cli {
    /// Address of any node in the cluster.
    #[arg(long, env = "DJBOD_NODE", global = true)]
    node: Option<SocketAddr>,
    /// The cluster id, as printed by `djbod-node init-cluster`.
    #[arg(long, env = "DJBOD_CLUSTER", global = true)]
    cluster: Option<Uuid>,
    /// Print results as JSON.
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Show every device's state and free space.
    Status,
    /// Store a file (or standard input with `-`) under a key.
    Put {
        key: String,
        /// File to upload, or `-` for standard input (which is read fully
        /// first, since the size must be known).
        file: PathBuf,
        #[arg(long)]
        content_type: Option<String>,
    },
    /// Fetch an object to a file (or standard output with `-`, the default).
    Get {
        key: String,
        #[arg(default_value = "-")]
        file: PathBuf,
    },
    /// Show an object's metadata record.
    Head { key: String },
    /// Delete an object.
    Delete { key: String },
    /// List keys.
    List {
        #[arg(long)]
        prefix: Option<String>,
        #[arg(long)]
        start_after: Option<String>,
        #[arg(long)]
        limit: Option<u32>,
    },
    /// Print the cluster document.
    ClusterConfig,
    /// Cluster membership commands.
    Cluster {
        #[command(subcommand)]
        command: ClusterCommand,
    },
    /// Rebuild damaged or missing shards of an object from the intact ones.
    Repair { key: String },
    /// Scrub the whole cluster: every node checks its own disks, then the
    /// cross-node checks run; with --repair, damaged objects are rebuilt.
    Scrub {
        /// Cap on each node's read rate in MiB per second.
        #[arg(long)]
        rate_mib: Option<u64>,
        #[arg(long)]
        repair: bool,
    },
}

#[derive(Subcommand)]
enum ClusterCommand {
    /// Ask every node for its document and show who holds which version.
    Show,
    /// Bring every node up to the highest document version any holds.
    Sync,
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

async fn connect(cli: &Cli) -> anyhow::Result<Connection> {
    let node = cli
        .node
        .context("no node address: pass --node or set DJBOD_NODE")?;
    let cluster = cli
        .cluster
        .context("no cluster id: pass --cluster or set DJBOD_CLUSTER")?;
    Connection::connect(node, Connection::client_hello(cluster))
        .await
        .map_err(|e| anyhow::anyhow!("{}", describe_error(&e)))
        .with_context(|| format!("connecting to {node}"))
}

/// Render a client error the way an administrator wants to read it: the
/// code, the message, then every identifying field the node supplied
/// (SPEC 16.2).
fn describe_error(e: &ClientError) -> String {
    match e {
        ClientError::Remote(detail) | ClientError::StreamFailed(detail) => describe_detail(detail),
        other => other.to_string(),
    }
}

fn describe_detail(detail: &ErrorDetail) -> String {
    let mut out = format!("{:?}: {}", detail.code, detail.message);
    if let Some(node) = detail.node {
        out.push_str(&format!("\n  node:    {}", node.0));
    }
    if let Some(device) = detail.device {
        out.push_str(&format!("\n  device:  {}", device.0));
    }
    if let Some(key) = &detail.key {
        out.push_str(&format!("\n  key:     {key}"));
    }
    if let Some(version) = detail.version {
        out.push_str(&format!("\n  version: {version}"));
    }
    if let Some(index) = detail.shard_index {
        out.push_str(&format!("\n  shard:   {index}"));
    }
    if let Some(stripe) = detail.stripe {
        out.push_str(&format!("\n  stripe:  {stripe}"));
    }
    out
}

fn remote(e: ClientError) -> anyhow::Error {
    anyhow::anyhow!("{}", describe_error(&e))
}

async fn run(cli: Cli) -> anyhow::Result<()> {
    match &cli.command {
        Command::Status => {
            let mut conn = connect(&cli).await?;
            match conn.request(Request::Status).await.map_err(remote)? {
                Response::Status {
                    cluster_id,
                    document_version,
                    coordinator,
                    devices,
                } => {
                    if cli.json {
                        println!(
                            "{}",
                            serde_json::to_string_pretty(&serde_json::json!({
                                "cluster_id": cluster_id,
                                "document_version": document_version,
                                "coordinator": coordinator,
                                "devices": devices,
                            }))?
                        );
                    } else {
                        println!("cluster   {cluster_id}");
                        println!("document  version {document_version}");
                        println!("answered  by node {}", coordinator.0);
                        println!();
                        println!(
                            "{:<36}  {:<36}  {:<9}  {:>12}  {:>12}",
                            "DEVICE", "NODE", "STATE", "TOTAL", "FREE"
                        );
                        for d in devices {
                            println!(
                                "{:<36}  {:<36}  {:<9}  {:>12}  {:>12}",
                                d.device.0,
                                d.node.0,
                                format!("{:?}", d.state).to_lowercase(),
                                human_bytes(d.total_bytes),
                                human_bytes(d.free_bytes)
                            );
                        }
                    }
                }
                other => bail!("unexpected response {other:?}"),
            }
        }
        Command::Put {
            key,
            file,
            content_type,
        } => {
            let mut conn = connect(&cli).await?;
            let version = if file.as_os_str() == "-" {
                let mut body = Vec::new();
                tokio::io::stdin()
                    .read_to_end(&mut body)
                    .await
                    .context("reading standard input")?;
                conn.put_object(key, &body, DEFAULT_BODY_CHUNK, content_type.clone())
                    .await
                    .map_err(remote)?
            } else {
                let mut source = tokio::fs::File::open(file)
                    .await
                    .with_context(|| format!("opening {}", file.display()))?;
                let size = source.metadata().await?.len();
                conn.put_object_from_reader(
                    key,
                    size,
                    &mut source,
                    DEFAULT_BODY_CHUNK,
                    content_type.clone(),
                )
                .await
                .map_err(remote)?
            };
            if cli.json {
                println!(
                    "{}",
                    serde_json::json!({ "key": key, "version": version.to_text() })
                );
            } else {
                println!("stored {key} as version {version}");
            }
        }
        Command::Get { key, file } => {
            let mut conn = connect(&cli).await?;
            if file.as_os_str() == "-" {
                let mut stdout = tokio::io::stdout();
                conn.get_object_to_writer(key, &mut stdout)
                    .await
                    .map_err(remote)?;
                stdout.flush().await?;
            } else {
                let mut sink = tokio::fs::File::create(file)
                    .await
                    .with_context(|| format!("creating {}", file.display()))?;
                let result = conn.get_object_to_writer(key, &mut sink).await;
                match result {
                    Ok(record) => {
                        sink.sync_all().await?;
                        if cli.json {
                            println!("{}", serde_json::to_string_pretty(&record)?);
                        } else {
                            eprintln!(
                                "fetched {key} ({} bytes) to {}",
                                record.size,
                                file.display()
                            );
                        }
                    }
                    Err(e) => {
                        drop(sink);
                        remove_partial(file);
                        return Err(remote(e)).with_context(|| {
                            format!("fetching {key}; partial output {} removed", file.display())
                        });
                    }
                }
            }
        }
        Command::Head { key } => {
            let mut conn = connect(&cli).await?;
            match conn
                .request(Request::HeadObject { key: key.clone() })
                .await
                .map_err(remote)?
            {
                Response::HeadObject { record } => {
                    if cli.json {
                        println!("{}", serde_json::to_string_pretty(&record)?);
                    } else {
                        println!("key           {}", record.key);
                        println!("version       {}", record.version);
                        println!("size          {} bytes", record.size);
                        println!("created       {}", record.created);
                        println!("checksum      {}", record.object_checksum.to_hex());
                        println!(
                            "scheme        {}+{}, block {} bytes",
                            record.k, record.m, record.block_size
                        );
                        if let Some(ct) = &record.content_type {
                            println!("content-type  {ct}");
                        }
                        for shard in &record.shards {
                            println!("shard {:<3}     device {}", shard.index, shard.device.0);
                        }
                    }
                }
                other => bail!("unexpected response {other:?}"),
            }
        }
        Command::Delete { key } => {
            let mut conn = connect(&cli).await?;
            conn.request(Request::DeleteObject { key: key.clone() })
                .await
                .map_err(remote)?;
            if !cli.json {
                println!("deleted {key}");
            }
        }
        Command::List {
            prefix,
            start_after,
            limit,
        } => {
            let mut conn = connect(&cli).await?;
            match conn
                .request(Request::ListKeys(ListQuery {
                    prefix: prefix.clone(),
                    start_after: start_after.clone(),
                    limit: *limit,
                }))
                .await
                .map_err(remote)?
            {
                Response::ListKeys { keys, truncated } => {
                    if cli.json {
                        println!(
                            "{}",
                            serde_json::to_string_pretty(&serde_json::json!({
                                "keys": keys,
                                "truncated": truncated,
                            }))?
                        );
                    } else {
                        for entry in &keys {
                            println!("{:>14}  {}  {}", entry.size, entry.version, entry.key);
                        }
                        if truncated {
                            eprintln!(
                                "(more keys follow; use --start-after {:?})",
                                keys.last().map(|k| k.key.as_str()).unwrap_or("")
                            );
                        }
                    }
                }
                other => bail!("unexpected response {other:?}"),
            }
        }
        Command::Scrub { rate_mib, repair } => {
            use djbod_proto::message::ScrubEvent;
            let mut conn = connect(&cli).await?;
            let id = conn
                .start_scrub(rate_mib.map(|m| m * 1024 * 1024), *repair)
                .await
                .map_err(remote)?;
            let mut findings = 0usize;
            let mut repairs = 0usize;
            let end = loop {
                match conn.next_scrub_event(id).await.map_err(remote)? {
                    Ok(event) => {
                        if cli.json {
                            println!("{}", serde_json::to_string(&event)?);
                            continue;
                        }
                        match &event {
                            ScrubEvent::NodeFinding {
                                node,
                                device,
                                finding,
                            } => {
                                findings += 1;
                                println!(
                                    "node {}  device {}\n  {}",
                                    short(&node.0),
                                    short(&device.0),
                                    describe_scrub_finding(finding)
                                );
                            }
                            ScrubEvent::NodeSummary {
                                node,
                                device,
                                summary,
                            } => {
                                eprintln!(
                                    "node {}  device {}: {} records, {} shards, {} blocks, {} read, {} finding(s)",
                                    short(&node.0),
                                    short(&device.0),
                                    summary.records_checked,
                                    summary.shards_checked,
                                    summary.blocks_checked,
                                    human_bytes(summary.bytes_read),
                                    summary.findings.len()
                                );
                            }
                            ScrubEvent::NodeFailed { node, detail } => {
                                println!(
                                    "node {} could not be scrubbed: {}",
                                    short(&node.0),
                                    detail.message
                                );
                            }
                            ScrubEvent::ClusterFinding(finding) => {
                                findings += 1;
                                println!("cluster check: {finding:?}");
                            }
                            ScrubEvent::Repaired { key, report } => {
                                repairs += 1;
                                let rewritten =
                                    report.shards.iter().filter(|s| s.rewritten).count();
                                println!("repaired {key}: {rewritten} shard(s) rewritten");
                            }
                            ScrubEvent::RepairFailed { key, detail } => {
                                println!("repair of {key} failed: {}", describe_detail(detail));
                            }
                        }
                    }
                    Err(end) => break end,
                }
            };
            if !cli.json {
                eprintln!("{findings} finding(s), {repairs} repair(s)");
            }
            if let Some(error) = end.error {
                eprintln!("scrub incomplete: {}", describe_detail(&error));
                std::process::exit(2);
            }
            if findings > 0 && !*repair {
                std::process::exit(2);
            }
        }
        Command::Repair { key } => {
            let mut conn = connect(&cli).await?;
            match conn
                .request(Request::RepairObject { key: key.clone() })
                .await
                .map_err(remote)?
            {
                Response::RepairObject(report) => {
                    if cli.json {
                        println!("{}", serde_json::to_string_pretty(&report)?);
                    } else {
                        println!("key      {}", report.key);
                        println!("version  {}", report.version);
                        for shard in &report.shards {
                            let condition = match &shard.condition {
                                djbod_proto::message::ShardCondition::Intact => {
                                    "intact".to_string()
                                }
                                djbod_proto::message::ShardCondition::Unreadable { reason } => {
                                    format!("unreadable ({reason})")
                                }
                                djbod_proto::message::ShardCondition::CorruptBlocks { stripes } => {
                                    format!("corrupt blocks in stripes {stripes:?}")
                                }
                            };
                            println!(
                                "shard {:<3}  device {}  {}{}",
                                shard.index,
                                shard.device.0,
                                condition,
                                if shard.rewritten {
                                    "  -> rewritten"
                                } else {
                                    ""
                                }
                            );
                        }
                        let count = report.shards.iter().filter(|s| s.rewritten).count();
                        println!("{count} shard(s) rewritten");
                    }
                }
                other => bail!("unexpected response {other:?}"),
            }
        }
        Command::Cluster { command } => {
            let node = cli
                .node
                .context("no node address: pass --node or set DJBOD_NODE")?;
            let cluster = cli
                .cluster
                .context("no cluster id: pass --cluster or set DJBOD_CLUSTER")?;
            match command {
                ClusterCommand::Show => {
                    let document = djbod_node::membership::fetch_document(node, cluster)
                        .await
                        .map_err(|e| anyhow::anyhow!("{e}"))?;
                    let reports = djbod_node::membership::fetch_all(&document).await;
                    if cli.json {
                        let rows: Vec<serde_json::Value> = reports
                            .iter()
                            .map(|r| {
                                serde_json::json!({
                                    "node": r.node,
                                    "address": r.address,
                                    "version": r.result.as_ref().ok().map(|d| d.version),
                                    "error": r.result.as_ref().err(),
                                })
                            })
                            .collect();
                        println!("{}", serde_json::to_string_pretty(&rows)?);
                    } else {
                        println!("cluster   {}", document.cluster_id);
                        println!("document  version {} as held by {node}", document.version);
                        println!();
                        println!("{:<36}  {:<21}  VERSION", "NODE", "ADDRESS");
                        for r in &reports {
                            let version = match &r.result {
                                Ok(d) => d.version.to_string(),
                                Err(e) => format!("unreachable: {e}"),
                            };
                            println!("{:<36}  {:<21}  {version}", r.node.0, r.address);
                        }
                    }
                }
                ClusterCommand::Sync => {
                    let report = djbod_node::membership::sync(node, cluster)
                        .await
                        .map_err(|e| anyhow::anyhow!("{e}"))?;
                    if cli.json {
                        println!(
                            "{}",
                            serde_json::to_string_pretty(&serde_json::json!({
                                "highest_version": report.highest_version,
                                "updated": report.updated,
                                "already_current": report.already_current,
                                "unreachable": report.unreachable,
                            }))?
                        );
                    } else {
                        println!("highest version  {}", report.highest_version);
                        for n in &report.updated {
                            println!("updated          {}", n.0);
                        }
                        for n in &report.already_current {
                            println!("already current  {}", n.0);
                        }
                        for (n, reason) in &report.unreachable {
                            println!("unreachable      {}  {reason}", n.0);
                        }
                        if !report.unreachable.is_empty() {
                            std::process::exit(2);
                        }
                    }
                }
            }
        }
        Command::ClusterConfig => {
            let mut conn = connect(&cli).await?;
            match conn
                .request(Request::GetClusterConfig)
                .await
                .map_err(remote)?
            {
                Response::GetClusterConfig { document } => {
                    println!("{}", serde_json::to_string_pretty(&document)?);
                }
                other => bail!("unexpected response {other:?}"),
            }
        }
    }
    Ok(())
}

fn short(id: &Uuid) -> String {
    id.to_string()[..8].to_string()
}

fn describe_scrub_finding(finding: &djbod_core::scrub::Finding) -> String {
    use djbod_core::scrub::Finding::*;
    match finding {
        RecordCorrupt { path, reason } => format!("record corrupt: {}  {reason}", path.display()),
        ShardUnreadable { path, reason, .. } => {
            format!("shard unreadable: {}  {reason}", path.display())
        }
        ShardMisplaced { path, reason, .. } => {
            format!("shard misplaced: {}  {reason}", path.display())
        }
        ShardBlocksCorrupt { path, stripes, .. } => {
            format!("corrupt blocks: {}  stripes {stripes:?}", path.display())
        }
        ShardWithoutRecord { path, .. } => format!("shard without record: {}", path.display()),
        RecordWithoutShard {
            path, shard_index, ..
        } => format!("record without shard {shard_index}: {}", path.display()),
        RecordNotForThisDevice { path, .. } => {
            format!("record not for this device: {}", path.display())
        }
        StaleTemporary { path, age_secs } => {
            format!("stale temporary: {}  {age_secs}s old", path.display())
        }
    }
}

fn remove_partial(path: &Path) {
    let _ = std::fs::remove_file(path);
}

fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}
