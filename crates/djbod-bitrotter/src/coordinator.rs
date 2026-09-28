use crate::{
    config::CoordinatorConfig,
    journal::{Journal, OpenMode},
    model::{
        select, Consent, Description, Event, Phase, Plan, PlannedObject, Probe, Reply, Request,
        FORMAT,
    },
    network::RpcClient,
    Stop,
};
use anyhow::{bail, ensure, Context, Result};
use djbod_client::{transport::Connector, Client, ClientOptions};
use djbod_core::{
    cluster::{ClusterDocument, DeviceState, NodeId},
    record::MetadataRecord,
};
use futures_util::{stream, StreamExt};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    net::SocketAddr,
    path::Path,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use uuid::Uuid;

pub use crate::journal::{load_plan, save_plan};

async fn product(
    config: &CoordinatorConfig,
    bootstrap: &[SocketAddr],
    cluster: Option<Uuid>,
) -> Result<Client> {
    let connector = match &config.product_tls {
        Some(tls) => tls.connector()?,
        None => Connector::plain(),
    };
    let mut options = ClientOptions::new(bootstrap.to_vec()).connector(connector);
    if let Some(cluster) = cluster {
        options = options.cluster(cluster);
    }
    Ok(tokio::time::timeout(Duration::from_secs(30), Client::connect(options)).await??)
}

#[derive(Default)]
pub struct Selection {
    pub n: u16,
    pub seed: u64,
    pub keys: Vec<String>,
    pub prefix: Option<String>,
    pub shard_indices: Vec<u8>,
}

pub async fn create_plan(
    bootstrap: Vec<SocketAddr>,
    config: CoordinatorConfig,
    selection: Selection,
) -> Result<Plan> {
    config.validate()?;
    ensure!(selection.n > 0, "n must be positive");
    let mut client = product(&config, &bootstrap, None).await?;
    let document = client.cluster_document().await?;
    let mut keys = selection.keys.clone();
    if keys.is_empty() {
        let listing = client.list_all(selection.prefix.as_deref()).await?;
        ensure!(
            listing.complete && listing.unread.is_empty(),
            "cluster listing is incomplete or has unread devices"
        );
        keys = listing.keys.into_iter().map(|entry| entry.key).collect();
    }
    keys.sort();
    keys.dedup();
    let mut records = Vec::new();
    for key in keys {
        if let Some(prefix) = &selection.prefix {
            ensure!(key.starts_with(prefix), "key does not match prefix");
        }
        let read = client.head(&key).await?;
        ensure!(
            read.missing_records.is_empty(),
            "incomplete record copies for {key}"
        );
        if read.record.size == 0 {
            eprintln!("skipped empty object: {key}");
        } else {
            records.push(read.record);
        }
    }
    assemble_plan(
        bootstrap,
        config,
        document,
        records,
        selection.n,
        selection.seed,
        &selection.shard_indices,
    )
    .await
}

/// Also useful for tests with files created through the public core writer.
pub async fn assemble_plan(
    bootstrap: Vec<SocketAddr>,
    config: CoordinatorConfig,
    document: ClusterDocument,
    mut records: Vec<MetadataRecord>,
    n: u16,
    seed: u64,
    filter: &[u8],
) -> Result<Plan> {
    config.validate()?;
    ensure!(!records.is_empty(), "no nonempty object versions selected");
    records.sort_by(|a, b| a.key.cmp(&b.key).then(a.version.cmp(&b.version)));
    let mut objects = Vec::new();
    let mut needed = BTreeSet::new();
    for record in records {
        let selected = select(&record, n, seed, filter)?;
        for shard in &record.shards {
            let device = document
                .device(shard.device)
                .context("record device absent from cluster document")?;
            ensure!(
                device.state != DeviceState::Removed,
                "record references a removed device"
            );
            needed.insert(device.node);
        }
        let record_checksum = record.checksum().to_hex();
        objects.push(PlannedObject {
            record,
            record_checksum,
            selected,
        });
    }
    let rpc = RpcClient::new(config.tls.as_ref())?;
    let mut workers: Vec<Description> = Vec::new();
    for node in needed {
        let endpoint = config
            .workers
            .iter()
            .find(|w| w.node == node)
            .with_context(|| format!("no worker for {node}"))?;
        match rpc.request(endpoint, Request::Describe).await? {
            Reply::Description(description) => {
                ensure!(
                    description.node == node && description.cluster == document.cluster_id,
                    "worker identity differs from configured node/cluster"
                );
                workers.push(description);
            }
            _ => bail!("unexpected worker description"),
        }
    }
    let mut plan = Plan {
        format: FORMAT,
        tool_version: env!("CARGO_PKG_VERSION").to_owned(),
        selection_algorithm: crate::model::SELECTION_ALGORITHM.to_owned(),
        id: String::new(),
        seed,
        n,
        bootstrap,
        config,
        document,
        workers,
        objects,
    };
    plan.seal()?;
    let coordinator = Coordinator::new(&plan)?;
    for object in &plan.objects {
        // Opening each shard validates its complete framing and the exact record
        // copy. Payload checks for the chosen stripe happen again before an event.
        coordinator.probe(&object.record, 0).await?;
    }
    Ok(plan)
}

pub struct Coordinator<'a> {
    plan: &'a Plan,
    rpc: RpcClient,
}

impl<'a> Coordinator<'a> {
    pub fn new(plan: &'a Plan) -> Result<Self> {
        plan.validate()?;
        Ok(Self {
            plan,
            rpc: RpcClient::new(plan.config.tls.as_ref())?,
        })
    }
    fn validate_operations(
        &self,
        node: NodeId,
        run: Uuid,
        event: &Event,
        operations: &[crate::model::Operation],
        applied: bool,
    ) -> Result<usize> {
        let expected = self.plan.local_indices(event, node)?;
        let mut seen = BTreeSet::new();
        ensure!(
            operations.len() == expected.len(),
            "worker {node} returned an incorrect operation count"
        );
        for operation in operations {
            self.plan.validate_operation(node, operation)?;
            ensure!(
                operation.run == run
                    && operation.event == *event
                    && seen.insert(operation.mutation.index)
                    && if applied {
                        operation.phase == Phase::Applied
                    } else {
                        operation.phase != Phase::Cancelled
                    },
                "worker {node} returned unexpected operation identities or phases"
            );
        }
        Ok(operations.len())
    }
    async fn requests(&self, requests: Vec<(NodeId, Request)>) -> Vec<(NodeId, Result<Reply>)> {
        stream::iter(requests)
            .map(|(node, request)| async move {
                let result = match self.plan.endpoint(node) {
                    Ok(endpoint) => self.rpc.request(endpoint, request).await,
                    Err(error) => Err(error),
                };
                (node, result)
            })
            .buffer_unordered(16)
            .collect()
            .await
    }
    pub async fn probe(&self, record: &MetadataRecord, stripe: u64) -> Result<Vec<Probe>> {
        let mut requests = Vec::new();
        for shard in &record.shards {
            requests.push((
                self.plan.owner(shard.device)?,
                Request::Probe {
                    record: Box::new(record.clone()),
                    index: shard.index,
                    stripe,
                },
            ));
        }
        let mut probes = Vec::new();
        for (_, response) in self.requests(requests).await {
            match response? {
                Reply::Probe(probe) => probes.push(probe),
                _ => bail!("unexpected probe response"),
            }
        }
        probes.sort_by_key(|p| p.index);
        ensure!(
            probes.len() == record.shards.len()
                && probes
                    .iter()
                    .enumerate()
                    .all(|(index, p)| usize::from(p.index) == index),
            "invalid probe shard identities"
        );
        Ok(probes)
    }
    pub async fn authorize(&self, run: Uuid, consent: &Consent) -> Result<BTreeMap<NodeId, Uuid>> {
        consent.validate(self.plan)?;
        let requests = self
            .plan
            .workers
            .iter()
            .map(|worker| {
                (
                    worker.node,
                    Request::Authorize {
                        plan: Box::new(self.plan.clone()),
                        run,
                        consent: consent.clone(),
                    },
                )
            })
            .collect();
        let mut tokens = BTreeMap::new();
        let mut errors = Vec::new();
        for (node, result) in self.requests(requests).await {
            match result {
                Ok(Reply::Authorized { token }) => {
                    tokens.insert(node, token);
                }
                Ok(_) => errors.push(format!("{node}: unexpected authorization reply")),
                Err(error) => errors.push(format!("{node}: {error:#}")),
            }
        }
        ensure!(
            errors.is_empty(),
            "authorization failed: {}",
            errors.join("; ")
        );
        Ok(tokens)
    }
    async fn event_requests(
        &self,
        run: Uuid,
        tokens: &BTreeMap<NodeId, Uuid>,
        event: &Event,
        make: impl Fn(Uuid, Uuid, u64) -> Request,
    ) -> Result<Vec<(NodeId, Result<Reply>)>> {
        let object = &self.plan.objects[event.object];
        let mut nodes = BTreeSet::new();
        for index in &object.selected {
            let device = object
                .record
                .device_for(djbod_core::erasure::ShardIndex(*index))
                .context("selected shard absent")?;
            nodes.insert(self.plan.owner(device)?);
        }
        let mut requests = Vec::new();
        for node in nodes {
            requests.push((
                node,
                make(
                    run,
                    *tokens.get(&node).context("missing session token")?,
                    event.sequence,
                ),
            ));
        }
        Ok(self.requests(requests).await)
    }
    pub async fn cancel(
        &self,
        run: Uuid,
        tokens: &BTreeMap<NodeId, Uuid>,
        event: &Event,
    ) -> Result<()> {
        for (_, reply) in self
            .event_requests(run, tokens, event, |run, token, sequence| Request::Cancel {
                run,
                token,
                sequence,
            })
            .await?
        {
            reply?;
        }
        Ok(())
    }
    pub async fn prepare(
        &self,
        run: Uuid,
        tokens: &BTreeMap<NodeId, Uuid>,
        event: &Event,
    ) -> Result<()> {
        let replies = self
            .event_requests(run, tokens, event, |run, token, sequence| {
                Request::Prepare {
                    run,
                    token,
                    sequence,
                }
            })
            .await?;
        let mut count = 0;
        let mut errors = Vec::new();
        for (node, reply) in replies {
            match reply {
                Ok(Reply::Operations(operations)) => {
                    match self.validate_operations(node, run, event, &operations, false) {
                        Ok(n) => count += n,
                        Err(error) => errors.push(format!("{error:#}")),
                    }
                }
                Ok(_) => errors.push("unexpected prepare reply".to_string()),
                Err(error) => errors.push(format!("{error:#}")),
            }
        }
        if !errors.is_empty() || count != usize::from(self.plan.n) {
            let cancellation = self.cancel(run, tokens, event).await;
            bail!(
                "prepare failed (no apply authorized): {}; cancellation: {cancellation:?}",
                errors.join("; ")
            );
        }
        Ok(())
    }
    pub async fn apply(
        &self,
        run: Uuid,
        tokens: &BTreeMap<NodeId, Uuid>,
        event: &Event,
    ) -> Result<()> {
        let replies = self
            .event_requests(run, tokens, event, |run, token, sequence| Request::Apply {
                run,
                token,
                sequence,
            })
            .await?;
        let mut count = 0;
        let mut errors = Vec::new();
        for (node, result) in replies {
            let response = match result {
                Ok(reply) => Ok(reply),
                Err(original) => {
                    // A lost acknowledgement is not a reason to pick another
                    // shard. Ask the same worker about the same durable IDs.
                    let token = *tokens.get(&node).context("missing token")?;
                    match self
                        .rpc
                        .request(
                            self.plan.endpoint(node)?,
                            Request::Status {
                                run,
                                token,
                                sequence: event.sequence,
                            },
                        )
                        .await
                    {
                        Ok(Reply::Operations(operations))
                            if operations.iter().all(|o| o.phase == Phase::Applied) =>
                        {
                            Ok(Reply::Operations(operations))
                        }
                        Ok(_) => Err(original),
                        Err(status) => {
                            Err(anyhow::anyhow!("{original:#}; status unknown: {status:#}"))
                        }
                    }
                }
            };
            match response {
                Ok(Reply::Operations(operations)) => {
                    match self.validate_operations(node, run, event, &operations, true) {
                        Ok(n) => count += n,
                        Err(error) => errors.push(format!("{error:#}")),
                    }
                }
                Ok(_) => errors.push("worker did not confirm applied operations".to_string()),
                Err(error) => errors.push(format!("{error:#}")),
            }
        }
        ensure!(
            errors.is_empty() && count == usize::from(self.plan.n),
            "partial or uncertain event ({count}/{} mutations confirmed); resume with the same journal: {}",
            self.plan.n,
            errors.join("; ")
        );
        Ok(())
    }
    async fn finish(&self, run: Uuid, tokens: &BTreeMap<NodeId, Uuid>) -> Result<()> {
        for (_, reply) in self
            .requests(
                tokens
                    .iter()
                    .map(|(&node, &token)| (node, Request::Finish { run, token }))
                    .collect(),
            )
            .await
        {
            reply?;
        }
        Ok(())
    }
    async fn revalidate(&self, object: usize) -> Result<()> {
        let mut client = product(
            &self.plan.config,
            &self.plan.bootstrap,
            Some(self.plan.document.cluster_id),
        )
        .await?;
        let document = client.cluster_document().await?;
        let expected = &self.plan.objects[object].record;
        for shard in &expected.shards {
            let device = document
                .device(shard.device)
                .context("device removed since planning")?;
            ensure!(
                device.node == self.plan.owner(shard.device)?
                    && device.state != DeviceState::Removed,
                "device ownership/state changed"
            );
        }
        let read = client.head(&expected.key).await?;
        ensure!(
            read.record == *expected && read.missing_records.is_empty(),
            "object version, placement, or record coverage changed"
        );
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Limits {
    pub events: Option<u64>,
    pub duration_secs: Option<u64>,
    pub continuous: bool,
    pub interval_millis: u64,
    pub max_mutations: Option<u64>,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            events: Some(1),
            duration_secs: None,
            continuous: false,
            interval_millis: 1000,
            max_mutations: None,
        }
    }
}

impl Limits {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            usize::from(self.events.is_some())
                + usize::from(self.duration_secs.is_some())
                + usize::from(self.continuous)
                == 1,
            "choose one of events, duration, or continuous"
        );
        ensure!(
            self.events != Some(0) && self.duration_secs != Some(0) && self.interval_millis > 0,
            "limits and interval must be positive"
        );
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EventReport {
    pub event: Event,
    pub key: String,
    pub status: String,
    pub bad_shards: Vec<u8>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct RunSummary {
    pub run: Uuid,
    pub completed: u64,
    pub skipped: u64,
    pub mutations_reserved: u64,
    pub stopped: bool,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum Entry {
    Started {
        run: Uuid,
        plan_id: String,
        limits: Limits,
        started_ms: u64,
        consent: Consent,
    },
    Attempt {
        event: Event,
    },
    Probed {
        event: Event,
        after_apply: bool,
        blocks: Vec<Probe>,
    },
    ApplyDecided {
        sequence: u64,
    },
    Outcome {
        report: EventReport,
    },
    Uncertain {
        sequence: u64,
        reason: String,
        observed: Option<Vec<Probe>>,
    },
    Finished,
}

fn now() -> Result<u64> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)?
        .as_millis()
        .try_into()?)
}

pub fn load_limits(path: &Path) -> Result<Limits> {
    let journal = Journal::open(path, OpenMode::Existing)?;
    match journal.read::<Entry>()?.first() {
        Some(Entry::Started { limits, .. }) => {
            limits.validate()?;
            Ok(limits.clone())
        }
        _ => bail!("run journal has no header"),
    }
}

pub async fn run(
    plan: &Plan,
    consent: Consent,
    path: &Path,
    limits: Limits,
    resume: bool,
    stop: Stop,
    mut report: impl FnMut(&EventReport),
) -> Result<RunSummary> {
    plan.validate()?;
    consent.validate(plan)?;
    limits.validate()?;
    let mut journal = Journal::open(
        path,
        if resume {
            OpenMode::Existing
        } else {
            OpenMode::Create
        },
    )?;
    let entries = journal.read::<Entry>()?;
    let (run, limits, started_ms) = if resume {
        match entries.first() {
            Some(Entry::Started {
                run,
                plan_id,
                limits,
                started_ms,
                consent: original_consent,
            }) => {
                ensure!(plan_id == &plan.id, "journal belongs to a different plan");
                limits.validate()?;
                original_consent.validate(plan)?;
                (*run, limits.clone(), *started_ms)
            }
            _ => bail!("run journal has no header"),
        }
    } else {
        let run = Uuid::new_v4();
        let started_ms = now()?;
        journal.append(&Entry::Started {
            run,
            plan_id: plan.id.clone(),
            limits: limits.clone(),
            started_ms,
            consent: consent.clone(),
        })?;
        (run, limits, started_ms)
    };
    let mut summary = RunSummary {
        run,
        ..RunSummary::default()
    };
    let mut pending = None;
    let mut decided = false;
    let mut sequence = 0;
    for entry in entries.into_iter().skip(1) {
        match entry {
            Entry::Attempt { event } => {
                ensure!(
                    pending.is_none() && event == plan.event(sequence)?,
                    "invalid journal attempt sequence"
                );
                sequence = event.sequence;
                pending = Some(event);
                decided = false;
            }
            Entry::ApplyDecided {
                sequence: apply_sequence,
            } => {
                ensure!(
                    pending.is_some() && !decided && apply_sequence == sequence,
                    "invalid journal apply decision"
                );
                decided = true;
                summary.mutations_reserved += u64::from(plan.n);
            }
            Entry::Outcome { report } => {
                ensure!(
                    report.event == plan.event(sequence)?,
                    "invalid journal outcome sequence"
                );
                ensure!(
                    if report.status == "complete" {
                        decided
                    } else {
                        !decided
                    },
                    "journal outcome conflicts with apply decision"
                );
                if report.status == "complete" {
                    summary.completed += 1;
                } else {
                    summary.skipped += 1;
                }
                sequence = report.event.sequence + 1;
                pending = None;
                decided = false;
            }
            Entry::Finished => {
                ensure!(pending.is_none(), "journal ended with an unresolved event");
                return Ok(summary);
            }
            Entry::Started { .. } => bail!("duplicate journal header"),
            _ => {}
        }
    }
    let coordinator = Coordinator::new(plan)?;
    loop {
        if pending.is_none() {
            if stop.is_stopped() {
                summary.stopped = true;
                break;
            }
            let expired = limits.duration_secs.is_some_and(|s| {
                now().unwrap_or(u64::MAX).saturating_sub(started_ms) >= s.saturating_mul(1000)
            });
            let exhausted = limits.events.is_some_and(|n| sequence >= n)
                || limits.max_mutations.is_some_and(|n| {
                    summary.mutations_reserved.saturating_add(u64::from(plan.n)) > n
                });
            if expired || exhausted {
                break;
            }
        }
        let tokens = coordinator.authorize(run, &consent).await?;
        let event = pending.clone().unwrap_or(plan.event(sequence)?);
        let key = plan.objects[event.object].record.key.clone();
        if pending.is_some() && !decided {
            coordinator.cancel(run, &tokens, &event).await?;
            let outcome = EventReport {
                event: event.clone(),
                key,
                status: "cancelled_before_apply".into(),
                bad_shards: Vec::new(),
            };
            journal.append(&Entry::Outcome {
                report: outcome.clone(),
            })?;
            report(&outcome);
            summary.skipped += 1;
        } else {
            let execution = async {
                coordinator.revalidate(event.object).await?;
                if !decided {
                    let probes = coordinator
                        .probe(&plan.objects[event.object].record, event.stripe)
                        .await?;
                    journal.append(&Entry::Probed {
                        event: event.clone(),
                        after_apply: false,
                        blocks: probes.clone(),
                    })?;
                    let bad: Vec<u8> = probes
                        .iter()
                        .filter(|p| !p.intact)
                        .map(|p| p.index)
                        .collect();
                    if !bad.is_empty() {
                        return Ok::<_, anyhow::Error>(EventReport {
                            event: event.clone(),
                            key: key.clone(),
                            status: "skipped_existing_damage".into(),
                            bad_shards: bad,
                        });
                    }
                    journal.append(&Entry::Attempt {
                        event: event.clone(),
                    })?;
                    coordinator.prepare(run, &tokens, &event).await?;
                    if stop.is_stopped() {
                        coordinator.cancel(run, &tokens, &event).await?;
                        return Ok(EventReport {
                            event: event.clone(),
                            key: key.clone(),
                            status: "cancelled_before_apply".into(),
                            bad_shards: Vec::new(),
                        });
                    }
                    journal.append(&Entry::ApplyDecided {
                        sequence: event.sequence,
                    })?;
                    summary.mutations_reserved += u64::from(plan.n);
                }
                coordinator.apply(run, &tokens, &event).await?;
                coordinator.revalidate(event.object).await?;
                let probes = coordinator
                    .probe(&plan.objects[event.object].record, event.stripe)
                    .await?;
                journal.append(&Entry::Probed {
                    event: event.clone(),
                    after_apply: true,
                    blocks: probes.clone(),
                })?;
                let bad: Vec<u8> = probes
                    .iter()
                    .filter(|p| !p.intact)
                    .map(|p| p.index)
                    .collect();
                ensure!(
                    bad == plan.objects[event.object].selected,
                    "partial/changed event: observed bad shards {bad:?}, expected {:?}",
                    plan.objects[event.object].selected
                );
                Ok(EventReport {
                    event: event.clone(),
                    key: key.clone(),
                    status: "complete".into(),
                    bad_shards: bad,
                })
            }
            .await;
            let outcome = match execution {
                Ok(outcome) => outcome,
                Err(error) => {
                    let observed = coordinator
                        .probe(&plan.objects[event.object].record, event.stripe)
                        .await
                        .ok();
                    journal.append(&Entry::Uncertain {
                        sequence: event.sequence,
                        reason: format!("{error:#}"),
                        observed: observed.clone(),
                    })?;
                    let bad_shards = observed
                        .unwrap_or_default()
                        .into_iter()
                        .filter(|p| !p.intact)
                        .map(|p| p.index)
                        .collect();
                    report(&EventReport {
                        event: event.clone(),
                        key,
                        status: "partial_or_uncertain".into(),
                        bad_shards,
                    });
                    return Err(error.context(format!(
                        "run {run}, event {}; reconcile with --resume and the same journal",
                        event.sequence
                    )));
                }
            };
            journal.append(&Entry::Outcome {
                report: outcome.clone(),
            })?;
            report(&outcome);
            if outcome.status == "complete" {
                summary.completed += 1;
            } else {
                summary.skipped += 1;
            }
        }
        coordinator.finish(run, &tokens).await?;
        pending = None;
        decided = false;
        sequence = event.sequence + 1;
        if stop.is_stopped() {
            summary.stopped = true;
            break;
        }
        tokio::select! {
            _ = stop.cancelled() => { summary.stopped = true; break; },
            _ = tokio::time::sleep(Duration::from_millis(limits.interval_millis)) => {},
        }
    }
    if !summary.stopped {
        journal.append(&Entry::Finished)?;
    }
    Ok(summary)
}
