use crate::config::{CoordinatorConfig, WorkerAddress};
use anyhow::{bail, ensure, Context, Result};
use djbod_core::{
    cluster::{ClusterDocument, NodeId},
    record::{DeviceId, MetadataRecord},
    shardfile::shard_geometry,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{collections::BTreeSet, net::SocketAddr, path::PathBuf};
use uuid::Uuid;

pub const FORMAT: u32 = 1;
pub const SELECTION_ALGORITHM: &str = "sha256-v1";
pub const MAX_BLOCK: u64 = 256 * 1024 * 1024;

pub fn digest(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceInfo {
    pub device: DeviceId,
    pub path: PathBuf,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Description {
    pub format: u32,
    pub node: NodeId,
    pub cluster: Uuid,
    pub journal_id: Uuid,
    pub devices: Vec<DeviceInfo>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlannedObject {
    pub record: MetadataRecord,
    pub record_checksum: String,
    pub selected: Vec<u8>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Plan {
    pub format: u32,
    pub tool_version: String,
    pub selection_algorithm: String,
    pub id: String,
    pub seed: u64,
    pub n: u16,
    pub bootstrap: Vec<SocketAddr>,
    pub config: CoordinatorConfig,
    pub document: ClusterDocument,
    pub workers: Vec<Description>,
    pub objects: Vec<PlannedObject>,
}

impl Plan {
    pub fn seal(&mut self) -> Result<()> {
        self.id.clear();
        self.id = digest(&serde_json::to_vec(self)?);
        self.validate()
    }
    pub fn validate(&self) -> Result<()> {
        ensure!(self.format == FORMAT, "unsupported plan format");
        ensure!(
            self.selection_algorithm == SELECTION_ALGORITHM,
            "unsupported selection algorithm"
        );
        ensure!(
            !self.bootstrap.is_empty(),
            "plan has no product bootstrap address"
        );
        self.config.validate()?;
        self.document.validate()?;
        let mut unhashed = self.clone();
        unhashed.id.clear();
        ensure!(
            self.id == digest(&serde_json::to_vec(&unhashed)?),
            "plan ID/checksum mismatch"
        );
        ensure!(
            self.n > 0 && !self.objects.is_empty(),
            "empty plan or zero shard count"
        );
        let mut devices = BTreeSet::new();
        let mut nodes = BTreeSet::new();
        for worker in &self.workers {
            ensure!(
                worker.format == FORMAT && worker.cluster == self.document.cluster_id,
                "worker protocol or cluster mismatch"
            );
            ensure!(
                !worker.journal_id.is_nil(),
                "worker journal has no identity"
            );
            ensure!(nodes.insert(worker.node), "duplicate worker identity");
            ensure!(
                self.config.workers.iter().any(|w| w.node == worker.node),
                "worker has no endpoint"
            );
            for device in &worker.devices {
                ensure!(devices.insert(device.device), "duplicate device ownership");
                ensure!(
                    self.document
                        .device(device.device)
                        .is_some_and(|d| d.node == worker.node),
                    "worker device owner differs from cluster document"
                );
            }
        }
        let mut identities = BTreeSet::new();
        for object in &self.objects {
            check_record(&object.record)?;
            ensure!(
                object.record_checksum == object.record.checksum().to_hex(),
                "plan record checksum mismatch"
            );
            ensure!(
                identities.insert((object.record.key_hash, object.record.version)),
                "duplicate object version"
            );
            let total = u16::from(object.record.k) + u16::from(object.record.m);
            ensure!(
                self.n <= total && object.selected.len() == usize::from(self.n),
                "invalid n for {}",
                object.record.key
            );
            let selected: BTreeSet<_> = object.selected.iter().copied().collect();
            ensure!(
                selected.len() == object.selected.len()
                    && selected.iter().all(|i| u16::from(*i) < total),
                "invalid or duplicate selected shard indices"
            );
            ensure!(
                object.selected.windows(2).all(|pair| pair[0] < pair[1]),
                "selected indices must be sorted"
            );
            for shard in &object.record.shards {
                ensure!(
                    devices.contains(&shard.device),
                    "missing worker coverage for {}",
                    shard.device
                );
            }
        }
        Ok(())
    }
    pub fn destructive(&self) -> bool {
        self.objects.iter().any(|o| self.n > u16::from(o.record.m))
    }
    pub fn owner(&self, device: DeviceId) -> Result<NodeId> {
        Ok(self
            .document
            .device(device)
            .context("device absent from plan")?
            .node)
    }
    pub fn endpoint(&self, node: NodeId) -> Result<&WorkerAddress> {
        self.config
            .workers
            .iter()
            .find(|w| w.node == node)
            .context("worker endpoint absent")
    }
    pub fn event(&self, sequence: u64) -> Result<Event> {
        ensure!(!self.objects.is_empty(), "empty plan");
        let object = (sequence % self.objects.len() as u64) as usize;
        let record = &self.objects[object].record;
        let geometry = shard_geometry(record.scheme()?, record.block_size, record.size)
            .context("empty object")?;
        let hash = Sha256::digest(format!("{}:stripe:{sequence}", self.id).as_bytes());
        let stripe = u64::from_le_bytes(hash[..8].try_into()?) % geometry.block_count;
        Ok(Event {
            sequence,
            object,
            stripe,
        })
    }
    pub fn position(&self, event: &Event, index: u8) -> Result<(u64, u8)> {
        ensure!(
            *event == self.event(event.sequence)?,
            "event differs from deterministic plan"
        );
        let record = &self.objects[event.object].record;
        let geometry = shard_geometry(record.scheme()?, record.block_size, record.size)
            .context("empty object")?;
        let length = if event.stripe + 1 == geometry.block_count {
            geometry.last_block_length
        } else {
            record.block_size
        };
        let hash =
            Sha256::digest(format!("{}:byte:{}:{index}", self.id, event.sequence).as_bytes());
        Ok((
            u64::from_le_bytes(hash[..8].try_into()?) % length,
            1 << (hash[8] % 8),
        ))
    }
    pub fn local_indices(&self, event: &Event, node: NodeId) -> Result<Vec<u8>> {
        let object = self
            .objects
            .get(event.object)
            .context("invalid object index")?;
        object
            .selected
            .iter()
            .copied()
            .filter_map(|index| {
                let device = object
                    .record
                    .device_for(djbod_core::erasure::ShardIndex(index));
                match device.and_then(|d| self.document.device(d)) {
                    Some(entry) if entry.node == node => Some(Ok(index)),
                    Some(_) => None,
                    None => Some(Err(anyhow::anyhow!("missing shard device"))),
                }
            })
            .collect()
    }

    pub fn validate_operation(&self, node: NodeId, operation: &Operation) -> Result<()> {
        let mutation = &operation.mutation;
        let event = self.event(operation.event.sequence)?;
        ensure!(
            operation.event == event
                && operation.id == operation_id(operation.run, event.sequence, mutation.index),
            "journal operation identity differs from plan"
        );
        ensure!(
            self.local_indices(&event, node)?.contains(&mutation.index),
            "journal operation targets an unselected shard"
        );
        let record = &self.objects[event.object].record;
        ensure!(
            record.device_for(djbod_core::erasure::ShardIndex(mutation.index))
                == Some(mutation.device),
            "journal operation targets a different device"
        );
        let root = self
            .workers
            .iter()
            .find(|w| w.node == node)
            .and_then(|w| w.devices.iter().find(|d| d.device == mutation.device))
            .context("operation device absent from plan")?;
        let expected_path =
            djbod_core::layout::object_directory(&root.path, &record.bucket, &record.key_hash)
                .join(djbod_core::layout::shard_file_name(
                    &record.version,
                    djbod_core::erasure::ShardIndex(mutation.index),
                ));
        let (byte, mask) = self.position(&event, mutation.index)?;
        let offset = event
            .stripe
            .checked_mul(record.block_size)
            .and_then(|n| n.checked_add(djbod_core::shardfile::HEADER_LEN))
            .and_then(|n| n.checked_add(byte))
            .context("operation offset overflow")?;
        ensure!(
            mutation.path == expected_path
                && mutation.offset == offset
                && mutation.mask == mask
                && mutation.before ^ mask == mutation.after
                && mutation.before_checksum != mutation.after_checksum,
            "journal mutation differs from deterministic plan"
        );
        Ok(())
    }
}

pub fn check_record(record: &MetadataRecord) -> Result<()> {
    record.validate()?;
    ensure!(
        record.bucket == "default" && record.size > 0,
        "empty object or unsupported bucket"
    );
    ensure!(
        record.block_size <= MAX_BLOCK,
        "block size exceeds bitrotter's 256 MiB limit"
    );
    record
        .block_size
        .checked_mul(u64::from(record.k))
        .context("stripe size overflow")?;
    Ok(())
}

pub fn select(record: &MetadataRecord, n: u16, seed: u64, filter: &[u8]) -> Result<Vec<u8>> {
    check_record(record)?;
    let total = u16::from(record.k) + u16::from(record.m);
    ensure!(
        n > 0 && n <= total,
        "n must be between 1 and k+m for {}",
        record.key
    );
    let mut indices: Vec<u8> = if filter.is_empty() {
        (0..total).map(|i| i as u8).collect()
    } else {
        filter.to_vec()
    };
    let distinct: BTreeSet<_> = indices.iter().copied().collect();
    if distinct.len() != indices.len() || indices.iter().any(|i| u16::from(*i) >= total) {
        bail!("invalid shard-index filter for {}", record.key);
    }
    ensure!(
        indices.len() >= usize::from(n),
        "filter contains fewer than n shards"
    );
    indices.sort_by_key(|i| {
        digest(format!("{seed}:{}:{}:{i}", record.key_hash.to_hex(), record.version).as_bytes())
    });
    indices.truncate(usize::from(n));
    indices.sort();
    Ok(indices)
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Event {
    pub sequence: u64,
    pub object: usize,
    pub stripe: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Consent {
    pub plan_id: String,
    pub test_damage: bool,
    pub data_loss: bool,
}

impl Consent {
    pub fn validate(&self, plan: &Plan) -> Result<()> {
        ensure!(
            self.plan_id == plan.id && self.test_damage,
            "explicit test-damage confirmation for this plan is required"
        );
        ensure!(
            !plan.destructive() || self.data_loss,
            "explicit certain-data-loss confirmation for this plan is required"
        );
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Fingerprint {
    pub device: u64,
    pub inode: u64,
    pub length: u64,
    pub mtime: (i64, i64),
    pub ctime: (i64, i64),
}

impl Fingerprint {
    pub fn same_file(&self, other: &Self) -> bool {
        self.device == other.device && self.inode == other.inode && self.length == other.length
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Probe {
    pub index: u8,
    pub intact: bool,
    pub fingerprint: Fingerprint,
    pub stored_checksum: u64,
    pub actual_checksum: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Mutation {
    pub index: u8,
    pub device: DeviceId,
    pub path: PathBuf,
    pub fingerprint: Fingerprint,
    pub offset: u64,
    pub mask: u8,
    pub before: u8,
    pub after: u8,
    pub before_checksum: u64,
    pub after_checksum: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Prepared,
    Intent,
    Applied,
    Cancelled,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Operation {
    pub id: String,
    pub run: Uuid,
    pub event: Event,
    pub mutation: Mutation,
    pub phase: Phase,
}

pub fn operation_id(run: Uuid, sequence: u64, index: u8) -> String {
    format!("{run}:{sequence}:{index}")
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case")]
pub enum Request {
    Describe,
    Probe {
        record: Box<MetadataRecord>,
        index: u8,
        stripe: u64,
    },
    Authorize {
        plan: Box<Plan>,
        run: Uuid,
        consent: Consent,
    },
    Prepare {
        run: Uuid,
        token: Uuid,
        sequence: u64,
    },
    Apply {
        run: Uuid,
        token: Uuid,
        sequence: u64,
    },
    Status {
        run: Uuid,
        token: Uuid,
        sequence: u64,
    },
    Cancel {
        run: Uuid,
        token: Uuid,
        sequence: u64,
    },
    Finish {
        run: Uuid,
        token: Uuid,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "result", content = "value", rename_all = "snake_case")]
pub enum Reply {
    Description(Description),
    Probe(Probe),
    Authorized { token: Uuid },
    Operations(Vec<Operation>),
    Finished,
    Error(String),
}
