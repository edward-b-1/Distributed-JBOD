//! Every message of the native protocol (SPEC 19.1.3), and how each maps
//! onto a frame.
//!
//! A conversation is: one `Hello` each way, then any number of requests. A request is one `Request`
//! frame. Its answer is one `Response` frame. Streaming operations add
//! `Data` frames, each carrying a sequence number, a checksum, and raw
//! bytes, and end with an `EndOfStream` frame carrying a status. All
//! frames of one operation share the request id.
//!
//! Streams and who sends them:
//!
//! | operation   | after the request         | after the response        |
//! |-------------|---------------------------|---------------------------|
//! | PutObject   | client: body bytes        |                           |
//! | GetObject   |                           | coordinator: body bytes   |
//! | PutShard    | sender: blocks (on READY) |                           |
//! | GetShard    |                           | holder: blocks            |
//!
//! Body streams are chunked at the coordinator's discretion; each chunk is
//! checksummed like a block so the transport is checked end to end.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

use djbod_core::checksum::BlockChecksum;
use djbod_core::cluster::{ClusterDocument, DeviceState, NodeId, Transport};
use djbod_core::keyhash::KeyHash;
use djbod_core::record::{DeviceId, MetadataRecord};
use djbod_core::scrub::{Finding, ScrubSummary};
use djbod_core::version::VersionId;

use crate::codec::{decode_cbor, encode_cbor, CodecError};
use crate::frame::{Frame, FrameError, MessageType};
use crate::handshake::Hello;

// ---------------------------------------------------------------- errors

/// Machine-readable reason for a failure. The `ErrorDetail` around it
/// carries what SPEC 16.2 requires for an administrator to act.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// A node in the cluster document did not respond.
    NodeUnreachable,
    /// A device needed for the request could not be reached or opened.
    DeviceUnavailable,
    /// A shard block failed its checksum.
    BlockChecksumMismatch,
    /// The whole-object checksum of a completed read did not match.
    ObjectChecksumMismatch,
    /// Record copies disagree, or fewer than k+m were found.
    RecordsInconsistent,
    /// The key in a record does not match the requested key.
    KeyMismatch,
    /// No object under this key.
    NotFound,
    /// Fewer than k+m eligible devices for a write.
    InsufficientDevices,
    /// A shard or record write failed and no replacement was found.
    WriteFailed,
    /// Peers hold different cluster document versions.
    DocumentVersionMismatch,
    /// Key exceeds the sanity limit.
    KeyTooLong,
    /// Object exceeds the maximum size.
    ObjectTooLarge,
    /// Content type or user metadata exceeds the record's limits.
    MetadataTooLarge,
    /// The request was malformed or out of sequence.
    ProtocolViolation,
    /// Handshake failed.
    Unauthorised,
    /// The cluster's transport is `tls` and the connection was plain
    /// (SPEC 19.1.6.4).
    TlsRequired,
    /// Anything else; the message says what.
    Internal,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorDetail {
    pub code: ErrorCode,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node: Option<NodeId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device: Option<DeviceId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<VersionId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shard_index: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stripe: Option<u64>,
}

impl ErrorDetail {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> ErrorDetail {
        ErrorDetail {
            code,
            message: message.into(),
            node: None,
            device: None,
            key: None,
            version: None,
            shard_index: None,
            stripe: None,
        }
    }
}

// -------------------------------------------------------------- requests

/// Keys per listing page are limited by their total length, so that a
/// page always fits inside one protocol frame with room to spare,
/// however many keys the cluster holds or how long the document allows
/// them to be (SPEC 15.2.1). A record listing page is bounded the same
/// way by encoded size.
pub const MAX_LIST_PAGE_BYTES: usize = 8 * 1024 * 1024;

/// Where a paged record listing continues from: the last (key, version)
/// of the previous page.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecordCursor {
    pub key: String,
    pub version: VersionId,
}

/// Where a paged lookup continues from: the last (version, device) of the
/// previous page.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LookupCursor {
    pub version: VersionId,
    pub device: DeviceId,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListQuery {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prefix: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_after: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
}

// `PartialEq` only: the cluster document carries a float (headroom).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Request {
    // ---- client to coordinator
    Status,
    /// Followed by a body stream of exactly `size` bytes.
    PutObject {
        key: String,
        size: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        content_type: Option<String>,
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        user_metadata: BTreeMap<String, String>,
    },
    GetObject {
        key: String,
    },
    HeadObject {
        key: String,
    },
    DeleteObject {
        key: String,
    },
    ListKeys(ListQuery),
    /// Rebuild every damaged or missing shard of a key's newest version
    /// from the intact ones (SPEC 18.3, 18.4). Administrative.
    RepairObject {
        key: String,
    },
    /// Move one shard of a key's newest version to another device
    /// (SPEC 18.8.2): the re-placement primitive behind drain, repair to a
    /// different device, and rebalance. Administrative.
    MoveShard {
        key: String,
        shard_index: u8,
        /// The destination, or `None` to choose as a write would (10.4).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        target: Option<DeviceId>,
    },
    /// Scrub the whole cluster (SPEC 20.1.2): every node's local scrub
    /// plus the cross-node checks, optionally repairing. Answered with
    /// `ScrubStarted`, then a stream of CBOR `ScrubEvent` data frames,
    /// then end-of-stream.
    Scrub {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        max_bytes_per_second: Option<u64>,
        repair: bool,
    },
    /// Move every shard off a `draining` device (SPEC 18.2.1, 18.2.2) in
    /// one pass. Answered with `DrainStarted`, then a stream of CBOR
    /// `DrainEvent` data frames, then end-of-stream, which carries an
    /// error if any version could not be moved. Administrative.
    Drain {
        device: DeviceId,
        /// Start even if the estimate says not everything will fit.
        partial: bool,
    },

    // ---- node to node
    LocalStatus,
    /// Every record copy under a key hash on this node's devices, in pages
    /// of at most `MAX_LIST_PAGE_BYTES` of encoded records sorted by
    /// version then device (15.2.2).
    LocalLookup {
        key_hash: KeyHash,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        after: Option<LookupCursor>,
    },
    LocalList(ListQuery),
    /// Every record on one local device, for the drain (18.2.1) and the
    /// removal scan (18.5), in pages: records after `after`, sorted by
    /// key then version, up to `MAX_LIST_PAGE_BYTES` of encoded records.
    LocalRecords {
        device: DeviceId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        after: Option<RecordCursor>,
    },
    /// Answered with `PutShardReady`, then the sender streams blocks, then
    /// the holder answers `PutShardDone`.
    PutShard {
        device: DeviceId,
        key_hash: KeyHash,
        version: VersionId,
        shard_index: u8,
        k: u8,
        m: u8,
        block_length: u64,
        object_size: u64,
    },
    /// Scrub this node's own devices (SPEC 20.1.2). Answered with
    /// `LocalScrubStarted`, then a stream of CBOR `ScrubItem` data frames,
    /// then end-of-stream.
    LocalScrub {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        max_bytes_per_second: Option<u64>,
    },
    /// Answered with `GetShard`, then a block stream.
    GetShard {
        device: DeviceId,
        key_hash: KeyHash,
        version: VersionId,
        shard_index: u8,
        first_block: u64,
        block_count: u64,
    },
    PutMeta {
        device: DeviceId,
        record: MetadataRecord,
    },
    GetMeta {
        device: DeviceId,
        key_hash: KeyHash,
        version: VersionId,
        /// Also report whether the shard file is present.
        probe: bool,
    },
    DeleteVersion {
        device: DeviceId,
        key_hash: KeyHash,
        version: VersionId,
    },
    AbortShard {
        device: DeviceId,
        key_hash: KeyHash,
        version: VersionId,
        shard_index: u8,
    },
    GetClusterConfig,
    ApplyClusterConfig {
        document: ClusterDocument,
    },
}

// ------------------------------------------------------------- responses

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceStatus {
    pub device: DeviceId,
    pub node: NodeId,
    pub state: DeviceState,
    /// The device's label from the cluster document, if it has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// The owning node's label, if it has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node_label: Option<String>,
    pub total_bytes: u64,
    pub free_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyEntry {
    pub key: String,
    pub size: u64,
    pub version: VersionId,
}

/// What repair found for one shard, and what it did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ShardCondition {
    /// Every block verified.
    Intact,
    /// The shard file could not be opened: missing or structurally
    /// corrupt. Rewritten.
    Unreadable { reason: String },
    /// The file opened but some blocks failed their checksums. Rewritten.
    CorruptBlocks { stripes: Vec<u64> },
    /// The device the record names is no longer in the cluster document
    /// (a forced removal, 6.2.6.3). Rebuilt onto another device and the
    /// record moved on by one revision (18.3).
    Lost,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShardRepair {
    pub index: u8,
    /// The device the record named before the repair.
    pub device: DeviceId,
    pub condition: ShardCondition,
    pub rewritten: bool,
    /// The device the shard was rebuilt onto when `device` is no longer
    /// in the cluster document.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relocated_to: Option<DeviceId>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepairReport {
    pub key: String,
    pub version: VersionId,
    pub shards: Vec<ShardRepair>,
    /// Devices whose copy of the metadata record was missing and has
    /// been rewritten from the agreeing copies (SPEC 18.4.2).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub record_copies_rewritten: Vec<DeviceId>,
    /// Devices from which a stale lower-revision copy, left by an
    /// interrupted re-placement, was removed (SPEC 18.8.1).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub stale_copies_removed: Vec<DeviceId>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocatedRecord {
    pub device: DeviceId,
    pub record: MetadataRecord,
}

// `PartialEq` only: the cluster document carries a float (headroom).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Response {
    Error(ErrorDetail),

    // ---- client to coordinator
    Status {
        cluster_id: Uuid,
        document_version: u64,
        coordinator: NodeId,
        #[serde(default)]
        transport: Transport,
        devices: Vec<DeviceStatus>,
    },
    PutObject {
        version: VersionId,
    },
    /// Followed by a body stream.
    GetObject {
        record: MetadataRecord,
    },
    HeadObject {
        record: MetadataRecord,
    },
    DeleteObject,
    ListKeys {
        keys: Vec<KeyEntry>,
        truncated: bool,
    },
    RepairObject(RepairReport),
    MoveShard {
        /// The record at its new revision.
        record: MetadataRecord,
        /// The device the shard came from.
        source: DeviceId,
        /// Whether the source's copy was removed; if not, the scrub will
        /// report it as stale and repair will remove it.
        source_cleaned: bool,
        /// Whether the shard was copied from the source or rebuilt from
        /// the other shards because the source was unreachable or damaged.
        rebuilt: bool,
    },
    /// Followed by a stream of `ScrubEvent` frames.
    ScrubStarted,
    /// Followed by a stream of `DrainEvent` frames.
    DrainStarted,

    // ---- node to node
    LocalStatus {
        node: NodeId,
        document_version: u64,
        /// Whether this node has TLS material loaded (19.1.6.4).
        #[serde(default)]
        tls_ready: bool,
        devices: Vec<DeviceStatus>,
    },
    /// A page of record copies; `truncated` says whether more follow
    /// after the last one.
    LocalLookup {
        records: Vec<LocatedRecord>,
        truncated: bool,
    },
    /// A page of at most `MAX_LIST_PAGE_BYTES` of keys; `truncated` says
    /// whether more follow after the last entry.
    LocalList {
        entries: Vec<KeyEntry>,
        truncated: bool,
    },
    /// A page of records; `truncated` says whether more follow after the
    /// last one.
    LocalRecords {
        records: Vec<MetadataRecord>,
        truncated: bool,
    },
    /// The holder has created and reserved the file; send blocks.
    PutShardReady,
    /// The holder has fsynced and renamed the file.
    PutShardDone,
    /// Followed by a block stream.
    GetShard {
        block_count: u64,
    },
    /// Followed by a stream of `ScrubItem` frames.
    LocalScrubStarted,
    PutMeta,
    GetMeta {
        record: Option<MetadataRecord>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        shard_present: Option<bool>,
    },
    DeleteVersion,
    AbortShard,
    GetClusterConfig {
        document: ClusterDocument,
    },
    ApplyClusterConfig,
}

// ------------------------------------------------------------ scrubbing

/// One frame of a `LocalScrub` stream: a finding as it arises, then one
/// summary when the node's devices are done.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "item", rename_all = "snake_case")]
pub enum ScrubItem {
    Finding {
        device: DeviceId,
        finding: Finding,
    },
    Summary {
        device: DeviceId,
        summary: ScrubSummary,
    },
}

/// A cross-node check that no single node can make (SPEC 20.1.2).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ClusterFinding {
    /// Fewer record copies than the record itself says there are holders,
    /// or copies that disagree, or a copy on an unlisted device.
    RecordsInconsistent {
        key: String,
        version: Option<VersionId>,
        detail: String,
    },
    /// A holder listed in the record does not have the shard file.
    ShardMissingOnHolder {
        key: String,
        version: VersionId,
        device: DeviceId,
        shard_index: u8,
    },
    /// A copy of the record at a lower placement revision than the
    /// current one, on a device the current revision no longer lists:
    /// left behind by an interrupted re-placement (SPEC 18.8.1).
    StaleCopy {
        key: String,
        version: VersionId,
        device: DeviceId,
        revision: u64,
        current_revision: u64,
    },
    /// A holder listed in the record could not be asked.
    HolderUnavailable {
        key: String,
        version: VersionId,
        device: DeviceId,
        detail: String,
    },
}

/// One frame of a `Scrub` stream, in the order things happen: local
/// findings and summaries from each node as they arrive, then cross-node
/// findings, then repairs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum ScrubEvent {
    NodeFinding {
        node: NodeId,
        device: DeviceId,
        finding: Finding,
    },
    NodeSummary {
        node: NodeId,
        device: DeviceId,
        summary: ScrubSummary,
    },
    /// A node could not be scrubbed; the scrub continues with the others
    /// but reports failure at the end.
    NodeFailed {
        node: NodeId,
        detail: ErrorDetail,
    },
    ClusterFinding(ClusterFinding),
    Repaired {
        key: String,
        report: RepairReport,
    },
    RepairFailed {
        key: String,
        detail: ErrorDetail,
    },
}

/// One event of a drain (SPEC 18.2.1, 18.2.2): the estimate, then one
/// `Moved` or `Skipped` per version on the device.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum DrainEvent {
    /// What the pass will attempt, measured before anything moves.
    Estimate {
        device: DeviceId,
        node: NodeId,
        /// Versions with a shard on the device.
        versions: u64,
        /// Bytes of shard files to move.
        shard_bytes: u64,
        /// Free bytes, within headroom, on `active` devices.
        target_free_bytes: u64,
        /// `active` devices in the cluster, and the k+m any version needs.
        active_devices: u64,
        required_devices: u64,
    },
    Moved {
        key: String,
        version: VersionId,
        shard_index: u8,
        destination: DeviceId,
        /// Rebuilt from the other shards rather than copied from the
        /// draining device.
        rebuilt: bool,
    },
    /// The version stays where it is; the detail says why.
    Skipped {
        key: String,
        version: VersionId,
        detail: ErrorDetail,
    },
    /// The version was deleted after the pass listed it: no copy of its
    /// record remains anywhere, so there is nothing to move. Not a
    /// failure.
    Deleted { key: String, version: VersionId },
}

// --------------------------------------------------------------- streams

/// One chunk of a stream. On the wire: sequence (u64), checksum (u64),
/// then the bytes. For shard streams the sequence is the stripe number
/// (SPEC 10.8); for body streams it counts chunks from zero.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DataFrame {
    pub sequence: u64,
    pub checksum: BlockChecksum,
    pub bytes: Vec<u8>,
}

pub const DATA_PREFIX_LEN: usize = 16;

impl DataFrame {
    pub fn encode_payload(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(DATA_PREFIX_LEN + self.bytes.len());
        out.extend_from_slice(&self.sequence.to_le_bytes());
        out.extend_from_slice(&self.checksum.0.to_le_bytes());
        out.extend_from_slice(&self.bytes);
        out
    }

    pub fn decode_payload(payload: &[u8]) -> Result<DataFrame, MessageError> {
        if payload.len() < DATA_PREFIX_LEN {
            return Err(MessageError::DataFrameTooShort(payload.len()));
        }
        let mut sequence = [0u8; 8];
        sequence.copy_from_slice(&payload[0..8]);
        let mut checksum = [0u8; 8];
        checksum.copy_from_slice(&payload[8..16]);
        Ok(DataFrame {
            sequence: u64::from_le_bytes(sequence),
            checksum: BlockChecksum(u64::from_le_bytes(checksum)),
            bytes: payload[DATA_PREFIX_LEN..].to_vec(),
        })
    }
}

/// Terminates a stream. `error` is `None` on success. For `PutShard` the
/// sender supplies the object size and checksum here so the holder can
/// check geometry and write the footer (SPEC 19.1.3). For `GetObject` the
/// coordinator reports the whole-object verification here (11.7), which
/// is why a client must read this frame before trusting the body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StreamEnd {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<ErrorDetail>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub object_size: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub object_checksum: Option<BlockChecksum>,
}

impl StreamEnd {
    pub fn ok() -> StreamEnd {
        StreamEnd {
            error: None,
            object_size: None,
            object_checksum: None,
        }
    }

    pub fn failed(error: ErrorDetail) -> StreamEnd {
        StreamEnd {
            error: Some(error),
            object_size: None,
            object_checksum: None,
        }
    }
}

// --------------------------------------------------------------- message

/// A decoded frame.
#[derive(Debug, Clone, PartialEq)]
pub enum Message {
    Hello(Hello),
    Request { id: u32, request: Request },
    Response { id: u32, response: Response },
    Data { id: u32, data: DataFrame },
    EndOfStream { id: u32, end: StreamEnd },
}

#[derive(Debug, Error)]
pub enum MessageError {
    #[error(transparent)]
    Frame(#[from] FrameError),
    #[error(transparent)]
    Codec(#[from] CodecError),
    #[error("data frame payload of {0} bytes is shorter than its 16-byte prefix")]
    DataFrameTooShort(usize),
}

impl Message {
    pub fn request_id(&self) -> Option<u32> {
        match self {
            Message::Hello(_) => None,
            Message::Request { id, .. }
            | Message::Response { id, .. }
            | Message::Data { id, .. }
            | Message::EndOfStream { id, .. } => Some(*id),
        }
    }

    pub fn to_frame(&self) -> Result<Frame, MessageError> {
        let frame = match self {
            Message::Hello(hello) => Frame::new(MessageType::Hello, 0, encode_cbor(hello)?),
            Message::Request { id, request } => {
                Frame::new(MessageType::Request, *id, encode_cbor(request)?)
            }
            Message::Response { id, response } => {
                Frame::new(MessageType::Response, *id, encode_cbor(response)?)
            }
            Message::Data { id, data } => Frame::new(MessageType::Data, *id, data.encode_payload()),
            Message::EndOfStream { id, end } => {
                Frame::new(MessageType::EndOfStream, *id, encode_cbor(end)?)
            }
        };
        Ok(frame)
    }

    pub fn from_frame(frame: &Frame) -> Result<Message, MessageError> {
        let id = frame.header.request_id;
        let message = match frame.header.message_type {
            MessageType::Hello => Message::Hello(decode_cbor(&frame.payload)?),
            MessageType::Request => Message::Request {
                id,
                request: decode_cbor(&frame.payload)?,
            },
            MessageType::Response => Message::Response {
                id,
                response: decode_cbor(&frame.payload)?,
            },
            MessageType::Data => Message::Data {
                id,
                data: DataFrame::decode_payload(&frame.payload)?,
            },
            MessageType::EndOfStream => Message::EndOfStream {
                id,
                end: decode_cbor(&frame.payload)?,
            },
        };
        Ok(message)
    }

    /// Encode to wire bytes.
    pub fn encode(&self) -> Result<Vec<u8>, MessageError> {
        Ok(self.to_frame()?.encode())
    }

    /// Decode one message from the front of `bytes`, returning it and the
    /// number of bytes consumed.
    pub fn decode(bytes: &[u8]) -> Result<(Message, usize), MessageError> {
        let (frame, consumed) = Frame::decode(bytes)?;
        Ok((Message::from_frame(&frame)?, consumed))
    }
}
