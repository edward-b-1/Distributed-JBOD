# Distributed Object Store: Design Specification

Draft 1, 16 September 2026. Written up from a design discussion; awaiting
review and approval.

Every numbered item carries one of these markers:

- **[D]** Decided in discussion.
- **[P]** Proposed. Follows from a decision but has not been explicitly
  approved. Approve, amend, or reject.
- **[O]** Open. Needs a decision before implementation of the affected part.
- **[X]** Deferred. Agreed to be out of scope for the first version.

---

## 1. Purpose and scope

1.1 [D] The system aggregates the disks of several commodity machines into
one object store with a flat key namespace, S3-like semantics, protection
against device loss, and protection against silent data corruption (bitrot).

1.2 [D] Target hardware is heterogeneous: machines of differing compute and
memory, each holding an arbitrary collection of disks of differing sizes.
Deployment target is Linux (Ubuntu Server or similar), one ext4 filesystem
per disk, each mounted separately.

1.3 [D] The first version supports whole-object PUT and GET, and lookup by
exact key. Listing is supported but is permitted to be slow. Range reads,
multipart upload, and other partial-object operations are out of scope.

1.4 [D] The system is not designed for high availability under frequent
intermittent failures. It is designed for correctness and durability on
small clusters, with an administrator in the loop when something breaks.

## 2. Terminology

| Term | Meaning |
|------|---------|
| **Node** | One machine running one instance of the storage process. |
| **Device** | One filesystem, mounted at one path, managed by one node. The smallest unit of independent failure. |
| **Coordinator** | The node a client connects to for a given request. Any node can be a coordinator. |
| **Bucket** | A namespace for keys. |
| **Key** | A UTF-8 string of up to 1024 bytes naming an object within a bucket. |
| **Object** | The data stored under a key, plus its metadata. |
| **Version** | One immutable body written under a key. A key may have several versions. |
| **Stripe** | A fixed-size window of an object's bytes, the unit of erasure coding. |
| **Shard block** | One device's piece of one stripe. The unit that carries a checksum. |
| **Shard file** | All of one device's shard blocks for one version, concatenated, plus a header. |
| **Shard index** | The position 0 .. k+m-1 of a shard within its stripe. Indices 0 .. k-1 are data, k .. k+m-1 are parity. |
| **Metadata record** | The small document describing one version: where its shards are and how it was encoded. |
| **Failure domain** | A grouping of devices that are expected to fail together (a machine, a rack, a site). |

## 3. Goals and non-goals

### Goals

3.1 [D] **Symmetry.** Every node runs the same process with the same code
paths. There is no master role, no metadata role, and no role assigned by
election. Any node can serve any request.

3.2 [D] **No single metadata location.** No piece of information required to
read an object is stored on only one device.

3.3 [D] **Heterogeneous devices.** Devices of any size participate fully.
Capacity is used in proportion to what each device has free.

3.4 [D] **Bitrot protection.** Every stored block is checksummed. Corruption
is detected on read and can be found by a scrubber before it is read.

3.5 [D] **Device loss protection.** Loss of up to m devices (see section 8)
loses no data.

3.6 [D] **Bounded memory.** Serving a request of any object size requires
memory proportional to the stripe size, not the object size.

3.7 [D] **Simple membership changes.** Adding a device or node requires no
data movement. Removing one is an explicit administrative operation.

### Non-goals for the first version

3.8 [D] High availability. Any unreachable node fails the request.

3.9 [D] Automatic healing. Corruption and loss are reported, not repaired
inline.

3.10 [D] Fast listing. Listing is a cluster-wide scan.

3.11 [D] Multi-tenancy, access control, and encryption at rest in the native
layer. These belong to the translation layer (section 19) or to later work.

## 4. Architecture overview

```
                 +-----------------+           +-----------------+
   S3 clients -->| S3 translation  |  native   |                 |
                 | process         |---------->|  Node A         |<---> devices
                 +-----------------+  protocol |  (coordinator   |
                                                |   for this req) |
   native      ------------------------------>  |                 |
   clients                                      +--------+--------+
                                                         | native protocol
                                                         | (broadcast lookup,
                                                         |  shard transfer)
                                        +----------------+----------------+
                                        |                |                |
                                   +----+----+      +----+----+      +----+----+
                                   | Node B  |      | Node C  |      | Node D  |
                                   +----+----+      +----+----+      +----+----+
                                        |                |                |
                                     devices          devices          devices
```

4.1 [D] A client connects to any node. That node is the coordinator for the
request. The coordinator finds the object by broadcasting to all nodes,
transfers shard blocks to or from the devices that hold them, and performs
or delegates erasure coding.

4.2 [D] Placement is **recorded**, not computed. At write time the
coordinator chooses which devices hold each shard using current free space
and the failure domain rules, and records the choice in the metadata
record. Nothing about an object's location can be derived from its key
alone. The record is what is found by broadcast.

4.3 [D] The metadata record for a version is stored in full on every device
that holds a shard of that version. There are therefore k+m identical copies.

## 5. Nodes and devices

5.1 [D] A node's local configuration lists the devices it manages as
filesystem paths, for example `/mnt/disk0/data`, `/mnt/disk1/data`.

5.2 [D] On first use, the node writes a device identity file at the root of
the device path containing a freshly generated UUID, the cluster id, the
on-disk format version, and a creation timestamp. The UUID is the device's
name in every record. Paths are never recorded.

5.3 [D] At startup the node checks the filesystem id (`st_dev`) of every
configured path. If two paths resolve to the same filesystem, the node
refuses to start. This catches the misconfiguration where two "devices" are
one disk.

5.4 [P] At startup the node also refuses to start if a device identity file
names a different cluster id, or if the same device UUID is claimed by two
paths.

5.5 [D] The failure domain of a device is described by labels inherited
from its node (section 7). A device's leaf label is its own UUID.

5.6 [D] A device reports free space as the filesystem's free bytes as
returned by `statvfs`, minus a configured headroom, minus the sum of
reservations currently in flight on that device.

## 6. Configuration

Configuration has three layers.

### 6.1 Per-node (local file)

6.1.1 [D] Node UUID, listen addresses, the list of device paths, the node's
failure domain labels, one or more bootstrap peer addresses, and the
cluster secret.

6.1.2 [D] Coordinator policy (section 17): the maximum number of concurrent
server-side encode or assemble streams this node will accept. Zero means
the node serves lookups and shard transfers but never codes on behalf of a
client.

### 6.2 Cluster-wide (versioned document held by every node)

6.2.1 [D] The document carries a monotonically increasing version number
and a cluster id. Every node holds a copy. A joining node fetches it from a
bootstrap peer after authenticating with the cluster secret.

6.2.2 [D] Contents: `k`, `m`, shard block size `B`, checksum algorithm, key
hash algorithm, the failure domain level at which shards must be
independent (section 7), the headroom fraction, the node list (UUID,
addresses, labels), and the device list (UUID, owning node, state).

6.2.3 [D] `k`, `m`, and `B` are global. Every object in the cluster is
encoded with the same parameters. There is no per-bucket or per-object
redundancy policy.

6.2.4 [D] Device states are `active`, `draining`, and `removed`. Only
`active` devices receive new shards.

6.2.5 [O] The mechanism by which the cluster document is changed and
propagated without a master. Proposed for v1: an administrative command,
issued through any node, that requires every node in the current document
to be reachable and to acknowledge the new version before the change is
considered applied. This is consistent with the fail-stop rule.

6.2.6 [D] All nodes must hold the same document version to serve requests.
A node that finds itself holding a different version from a peer during a
request returns an error.

### 6.3 Per-object (in the metadata record)

6.3.1 [D] Each version's metadata record stores the `k`, `m`, `B`, checksum
algorithm, and key hash algorithm it was written with. This is a record,
not a policy. It exists so that a future change to the global values does
not make existing objects unreadable, and so that a recovery tool needs no
cluster configuration.

## 7. Failure domains

7.1 [D] Devices are organised in a hierarchy. From the leaf upward:
device, host, rack, datacenter, region. Labels above `device` are set in
node configuration. Any level may be omitted.

7.2 [D] The cluster document names one level as the **independence level**.
The k+m shards of one stripe must be placed on k+m devices that are
pairwise distinct at that level. With independence level `device`, two
shards may share a host but not a disk. With `host`, no two shards share a
machine.

7.3 [D] The independence level is a hard constraint. If fewer than k+m
distinct domains at that level have room for the shard, the write fails.

7.4 [P] Within the hard constraint, placement prefers to spread across
higher levels as well when it can. With independence level `device` and two
hosts, a 3+1 write should avoid putting three shards on one host if a
spread of two and two is available.

## 8. Data encoding

### 8.1 Code

8.1.1 [D] Objects are encoded with a systematic Reed-Solomon code over
GF(2^8) with k data shards and m parity shards. Any k of the k+m shards
recover the stripe.

8.1.2 [D] `k = 1` yields plain replication with `m+1` copies. `m = 0`
yields an unprotected distributed JBOD with checksums. Both are valid
configurations of the same code path.

8.1.3 [D] The encoding matrix, field polynomial, and generator are those of
the chosen library and are fixed for the life of the cluster.

8.1.4 [D] An existing, well-tested library is used. The code is not
implemented from scratch.

8.1.5 [X] A non-systematic (information dispersal) variant, in which no
shard contains plaintext, may be offered later as an option. It would
change only the encoding matrix.

### 8.2 Stripes and shard blocks

8.2.1 [D] The shard block size `B` is a global constant. A stripe is
`k × B` bytes of the object. `B` must be a multiple of 4096.

8.2.2 [D] A stripe is split contiguously: bytes `0 .. B-1` of the stripe
form shard block 0, bytes `B .. 2B-1` form shard block 1, and so on. Every
data shard block is therefore a verbatim run of the original object.

8.2.3 [D] Parity shard blocks `k .. k+m-1` are computed from the k data
blocks byte-wise: byte j of each parity block depends only on byte j of
each data block.

8.2.4 [P] The final stripe of an object is usually short. Its k data
blocks are each `ceil(remaining / k)` bytes, zero-padded to that length,
and parity is computed over the padded blocks. The object's true length is
stored in the metadata record and padding is discarded on read. Every
block within one stripe therefore has equal length.

8.2.5 [P] An object of length zero has zero stripes and consists of a
metadata record alone.

### 8.3 Checksums

8.3.1 [D] Every shard block carries a 64-bit checksum computed over the
stored (padded) block.

8.3.2 [P] Algorithm: XXH3-64. Alternative: CRC-64. Either is many times
faster than disk. A cryptographic hash is not required because the native
protocol does not admit untrusted writers in v1.

8.3.3 [D] Checksums are stored contiguously in a table in the shard file
header (section 9.3), not interleaved with the blocks, so that every block
begins on a 4096-byte boundary.

8.3.4 [D] A checksum mismatch on read is treated as an erasure of that
block. The read is not served from the corrupt block.

8.3.5 [D] The parity shards are used for erasures only. They are not used
to detect or locate corruption. Detection and location are the checksum's
job.

## 9. On-disk layout

### 9.1 Device root

```
<device path>/
    device.json               identity file (section 5.2)
    objects/
        ab/
            cd/
                abcd...<64 hex>/          one directory per (bucket, key)
                    <version>.meta.json   one metadata record per version
                    <version>.<idx>.shard one shard file per version held here
```

9.1.1 [D] The object directory is named by the key hash, not by the key.
Keys are not filesystem-safe: they may exceed 255 bytes, contain any byte
but NUL, and an object `a` may coexist with an object `a/b`.

9.1.2 [P] Key hash: `BLAKE3(bucket ‖ 0x00 ‖ key)`, rendered as 64 lowercase
hex characters. The first two and next two characters form two levels of
fan-out directories.

9.1.3 [D] The original bucket and key are stored in plain text inside the
metadata record. A disk can be searched for an object by name with
ordinary tools.

9.1.4 [D] A device holds exactly one shard of any given version, so there
is at most one shard file per version per object directory. Which shard
index it holds varies from object to object.

### 9.2 Version identifiers

9.2.1 [P] Version identifiers are ULIDs: 128 bits, time-ordered, generated
by the coordinator at write time. Sorting version file names
lexicographically yields creation order.

9.2.2 [O] Resolution of two coordinators writing the same key concurrently.
Not yet designed. See section 21.

### 9.3 Shard file format

```
offset 0        header, padded to 4096 bytes (or a multiple, if the
                checksum table needs more)
    magic                      8 bytes
    format version             4 bytes
    checksum algorithm id      4 bytes
    block count                8 bytes
    block length B             8 bytes
    last block length          8 bytes
    header checksum            8 bytes   (over the header, table included)
    checksum table             8 bytes × block count

offset H        shard block 0
offset H + B    shard block 1
...
```

9.3.1 [D] Shard files are immutable once written. They are written to a
temporary name, fsynced, and renamed into place. They are never modified.

9.3.2 [D] Shard blocks start at offsets that are multiples of 4096.

9.3.3 [X] Fields identifying the object (key hash, version, shard index,
`k`, `m`, and the UUIDs of the sibling devices) could be added to the
header to make a shard self-describing without its metadata record. Parked
pending further discussion.

### 9.4 Metadata record

9.4.1 [D] One record per version, in a human-readable format (JSON),
stored on every device holding a shard of that version.

9.4.2 [P] Fields:

```
format_version      integer
bucket              string
key                 string
key_hash            hex string
version             ULID
created             RFC 3339 timestamp
size                integer, true object length in bytes
k, m                integers
block_size          integer, B
checksum_algorithm  string
key_hash_algorithm  string
shards              array of { index, device_uuid, node_uuid }
user_metadata       opaque map, reserved for the translation layer
content_type        string, optional
delete_marker       boolean (see section 14)
```

9.4.3 [D] The record is written after all shard files of the version are
durable, using the same temporary-name, fsync, rename procedure, followed
by an fsync of the directory.

9.4.4 [D] Because the record is on every holder, and every read must reach
every node, the read path checks that all copies agree. Disagreement is an
error (section 16).

## 10. Write path (PUT)

10.1 [D] The client sends bucket, key, content length, and the body to a
coordinator. Content length is required in v1.

10.2 [D] The coordinator generates a version id and computes the key hash,
stripe count, and shard file size.

10.3 [D] The coordinator obtains current free space for every device by
querying every node. Any node failing to respond fails the write.

10.4 [D] The coordinator chooses k+m devices that are `active`, have free
space of at least one shard file, and satisfy the independence level.

10.5 [P] Among eligible devices, selection is random weighted by free
space. Deterministic "most free first" would send every concurrent write in
the cluster to the same device.

10.6 [D] For each chosen device, the coordinator opens a shard transfer.
The receiving node creates the temporary shard file and reserves its full
size with `fallocate(2)`, mode 0. If `fallocate` returns `ENOSPC` the node
refuses the shard, and the coordinator chooses a replacement device. If no
replacement exists the write fails and everything written so far is
removed.

10.7 [D] The reservation is a metadata operation on the filesystem. It
writes no data and causes no wear. `posix_fallocate` must not be used,
because it silently falls back to writing zeros on filesystems that lack
native support.

10.8 [D] The coordinator (or the client, section 17) reads the body one
stripe at a time, splits it into k data blocks, computes m parity blocks,
computes k+m checksums, and streams block and checksum to each holder.
Memory in use per request is bounded by one stripe plus parity.

10.9 [D] Each receiving node writes blocks into the reserved file, writes
the header last, fsyncs, and renames. It then writes the metadata record
the coordinator sends, fsyncs, renames, and fsyncs the directory.

10.10 [D] The coordinator acknowledges the write to the client only when
all k+m shard files and all k+m metadata records are durable. Any failure
at any step fails the whole write. The coordinator makes a best effort to
delete temporary files on failure.

10.11 [O] Cleanup of temporary files left by a coordinator that crashed
mid-write. Proposed: a device deletes temporary files older than a
configurable age at startup and during scrub.

## 11. Read path (GET)

11.1 [D] The client sends bucket, key, and optionally a version id to a
coordinator.

11.2 [D] The coordinator broadcasts a lookup (section 13) and receives the
metadata records from every holder. It selects the requested version, or
the newest version if none was requested.

11.3 [D] For each stripe in order, the coordinator requests shard blocks
`0 .. k-1` (the data blocks) from their holders, verifies each block against
its checksum, and delivers the stripe to the client. Parity blocks are not
read.

11.4 [D] If any block fails its checksum, or any holder is unreachable, the
request fails with an error identifying the device UUID, object, version,
shard index, and stripe number. No reconstruction is attempted in v1.

11.5 [D] Reads stream. The coordinator holds a bounded number of stripes in
memory at once.

11.6 [D] The last stripe is truncated to the object's true length before
delivery.

## 12. Placement summary

For reference, a stripe of an object written with 3+1 on a cluster of two
hosts (A: three devices, B: two devices) at independence level `device`:

```
shard 0 (data)   -> host A, device a1
shard 1 (data)   -> host B, device b1
shard 2 (data)   -> host A, device a3
shard 3 (parity) -> host B, device b2
```

Every stripe of the version uses the same four devices in the same order.
Every one of the four devices holds a copy of the metadata record.

## 13. Lookup by broadcast

13.1 [D] To find an object, the coordinator sends a lookup for (bucket,
key hash) to every node in the cluster document and waits for every node to
respond.

13.2 [D] Each node checks each of its devices for a directory at the key
hash and returns every metadata record found there.

13.3 [D] If any node fails to respond within the timeout, the lookup fails.
Because every node must answer, a lookup that finds nothing is an
authoritative "not found".

13.4 [D] No node stores any index of what other nodes hold.

13.5 [D] Fan-out is one request per node, not per device.

## 14. Delete

14.1 [P] Deleting a version removes its shard files and metadata records
from every holder. The coordinator looks the version up, instructs every
holder to delete, and reports success only when all have confirmed. Any
unreachable holder fails the delete.

14.2 [O] Whether deleting a key without a version id removes all versions,
removes the newest, or writes a delete marker. S3 semantics require delete
markers on versioned buckets; the native protocol need not. The
`delete_marker` field in the metadata record is reserved for this.

## 15. Listing

15.1 [D] Listing a bucket, optionally by prefix, broadcasts to every node.
Each node scans the metadata records on each of its devices and returns
matching keys. The coordinator merges, deduplicates (since each record
exists on k+m devices), sorts, and returns.

15.2 [D] This is a full scan of every device and is accepted as slow.

15.3 [X] A sorted index to make listing fast belongs in the translation
layer or a later version.

## 16. Failure semantics

16.1 [D] The system is fail-stop. The following conditions return an error
to the client:

- Any node in the cluster document does not respond to a broadcast.
- Any device holding a shard needed for a read is unreachable.
- Any shard block fails its checksum.
- Metadata record copies for a version disagree.
- Fewer than k+m eligible devices exist for a write.
- A reservation or write fails on any device and no replacement is found.
- Nodes disagree on the cluster document version.

16.2 [D] Errors carry enough detail for an administrator to act: the
condition, the node and device UUIDs involved, and for data errors the
object, version, shard index, and stripe.

16.3 [D] Clients may retry. The system does not retry internally except for
choosing a replacement device on `ENOSPC` during a write.

16.4 [D] Corruption and loss are repaired by administrative action
(section 18), not by the read path.

16.5 [X] Inline reconstruction on checksum failure, with the client
receiving correct data while the fault is logged and queued for repair, is
a planned extension.

## 17. Coordinator role and reconstruction policy

17.1 [D] Erasure coding work (encoding on write, and assembling or
reconstructing on read) can be done by the coordinator or by the client.
Which one is negotiated per request.

17.2 [D] A client indicates whether it wants the coordinator to perform
coding or wants to do it itself. A native client that codes for itself
receives (on read) the metadata record and fetches shard blocks directly
from the holding nodes, and (on write) encodes stripes and sends shard
streams directly to the chosen devices.

17.3 [D] A node may refuse to perform coding. Refusal is governed by the
node's local limit on concurrent coding streams. A limit of zero makes the
node lookup-and-transfer only.

17.4 [P] A refusing node returns the metadata record (for reads) or the
chosen placement (for writes) so the client may proceed itself, and may
also name another node willing to accept the work.

17.5 [D] This mechanism is how deployments spread coding CPU across
machines of unequal capability, or steer it to a strong machine.

17.6 [D] Regardless of where coding happens, the receiving node computes
each shard block's checksum itself and does not trust a checksum supplied
by the client.

## 18. Membership: add, drain, remove, repair, rebalance

18.1 [D] **Add a device or node.** Update the cluster document. The new
device becomes eligible for new shards immediately. No data moves.

18.2 [D] **Drain a device.** Set its state to `draining`. It receives no
new shards. A drain job finds every version with a shard on that device,
places that shard on another eligible device, and updates the metadata
record on all holders. When no record references the device, it is set to
`removed` and may be detached. Draining a node is draining all its devices.

18.3 [D] **Repair after loss.** Identical to drain except that the shard is
reconstructed from k surviving shards rather than copied.

18.4 [D] **Repair after corruption.** The corrupt shard block is identified
by the checksum. The shard file is reconstructed from k valid shards and
rewritten on the same or another device.

18.5 [P] Finding the versions that reference a device is a scan of all
metadata records on all devices, run as a background job.

18.6 [D] Every metadata record has k+m copies, so a single device loss
never loses a record. With `m = 1` this leaves k copies after a loss and
the record survives. With `m = 0` a device loss loses both the shard and
one record copy; the remaining copies still describe the object and report
it as damaged.

18.7 [X] **Rebalance.** Moving existing shards from full devices to empty
ones to even out utilisation. Administrator-triggered when implemented.
Not in v1.

18.8 [D] Drain, repair, and rebalance are all instances of one operation:
produce shard `i` of version `v` on device `d`, then update the record.

## 19. Protocols

### 19.1 Native protocol

19.1.1 [D] The native protocol is binary, framed, and streaming, over TCP.
Frames carry one shard block and its checksum so that the wire unit matches
the storage unit.

19.1.2 [P] Operations: `Status` (node and per-device free space),
`Lookup`, `List`, `PutObject`, `GetObject`, `PutShard`, `GetShard`,
`PutMeta`, `GetMeta`, `DeleteVersion`, `GetClusterConfig`,
`ApplyClusterConfig`.

19.1.3 [O] Authentication and transport security between nodes and for
native clients. The cluster secret authenticates joining nodes; whether it
also authenticates every connection, and whether TLS is required, is
undecided.

### 19.2 S3 translation layer

19.2.1 [D] S3 compatibility is provided by a separate process that speaks
S3 to clients and the native protocol to the cluster. It may run on one
node, on every node, or elsewhere.

19.2.2 [D] The translation layer aims for faithful S3 semantics. The value
of S3 compatibility is existing tooling; improvements belong in the native
protocol.

19.2.3 [P] The translation layer owns everything the native layer does not:
authentication, policies, bucket configuration, sorted listing, multipart
assembly, and S3 versioning semantics including delete markers. It stores
what it needs in `user_metadata` or in its own state.

## 20. Operations and tooling

### 20.1 Scrubber

20.1.1 [X] Not in v1, but required before the system is trusted with data.
Because every shard block has a checksum stored beside it, a device can be
scrubbed locally at disk speed with no network traffic and no coordination.

20.1.2 [O] Two designs were identified. A separate process per machine that
asks the node, over a local socket, to verify shard files at a configured
rate. Or a separate process that reads the on-disk format directly. The
first is simpler and reuses the node's code; the second is independent of
the node's health.

20.1.3 [D] Since reads report rather than heal, scrubbing is the mechanism
by which corruption is found before a client encounters it.

### 20.2 Recovery tool

20.2.1 [P] A single static binary that, given one or more device paths and
no running cluster, lists the versions present and reassembles any version
for which k shards can be found among the given paths. Requires only the
metadata records and shard files. This is the answer to the loss of
human-readable on-disk layout.

### 20.3 ext4 deployment notes

20.3.1 [D] One ext4 filesystem per disk, mounted separately.

20.3.2 [P] Format and mount recommendations for data devices:

- Set reserved blocks to zero: `tune2fs -m 0 <dev>`. The default reserves
  five percent for root.
- Consider a higher inode ratio at format time if many small objects are
  expected. Each version costs one directory (shared per key), one metadata
  file, and one shard file per holding device.
- Mount with `noatime`.
- `fallocate` is natively supported on ext4 and XFS. ZFS does not support
  it and Btrfs cannot guarantee it. The system requires native `fallocate`.

20.3.3 [D] Filename limit is 255 bytes and path limit is 4096 bytes. The
layout in section 9 uses fixed-length names and stays well within both.

## 21. Open questions

| # | Question | Notes |
|---|----------|-------|
| 21.1 | Concurrent writes to the same key from two coordinators. | ULIDs give an order. Whether both versions are kept, or the later one wins, and how holders are kept consistent, is undecided. |
| 21.2 | How the cluster document is changed without a master. | Proposal in 6.2.5. |
| 21.3 | Delete semantics in the native protocol. | Section 14. |
| 21.4 | Cleanup of orphaned temporary files. | 10.11. |
| 21.5 | Authentication and TLS on the native protocol. | 19.1.3. |
| 21.6 | Whether buckets are a native concept or a prefix convention. | The key hash includes the bucket, so the layout assumes native buckets. |
| 21.7 | Scrubber architecture. | 20.1.2. |
| 21.8 | Free-space query on every write versus a cached heartbeat. | Query per write is simplest and consistent with fail-stop. |
| 21.9 | Placement weighting. | 10.5 proposes weighted random. |
| 21.10 | Handling of unknown content length. | 10.1 requires a length. |

## 22. Deferred items

- Inline reconstruction on read (16.5).
- Scrubber (20.1).
- Rebalance (18.7).
- Self-describing shard headers (9.3.3).
- Non-systematic encoding option (8.1.5).
- Optional parity verification on read, for deployments that want it.
- Range reads and multipart upload.
- Inline storage of very small objects in the metadata record, avoiding a
  shard file per device for objects far smaller than one block.
- Packing shard files into large volume files to reduce inode and fsync
  cost. The logical layout in section 9 is designed to survive this change
  unaltered.
- Local reconstruction codes or other repair-efficient codes.
- A sorted listing index.

## 23. Decision log

Brief rationale for each major decision, so that it can be revisited with
the reasoning in view.

**Symmetric nodes, no master.** Requirement of the project. Rules out
Ceph, SeaweedFS, and MooseFS as bases. Systems that meet it (Garage,
Swift, MinIO) do so by making placement either computable from shared
configuration or recorded in replicated metadata. This design takes the
second route.

**Recorded placement over deterministic placement.** Deterministic
placement (hash to a partition table) forces data movement on every layout
change and only approximates free space. Recorded placement uses devices
of any size exactly, makes adding a device free, and defers rebalancing to
an explicit action. Its cost, finding the record, is paid by broadcast.

**Broadcast lookup.** Simplest thing that works and stores no index
anywhere. Cost is one message per node per lookup. Acceptable for the
target cluster sizes. Fail-stop makes negative answers authoritative.

**Fail-stop.** The administrator wants to know about failures immediately
and failures on old hardware are expected to be noticed and fixed by hand.
Removes quorum logic, hinted handoff, and read repair from the design.

**Systematic Reed-Solomon.** One library covers replication (`k = 1`),
RAID 5 and 6 equivalents, and wide schemes, so the choice of redundancy is
configuration rather than code. Systematic form means healthy reads need
no decoding and data shard blocks are verbatim runs of the object.
RAID-Z2 and RAID 6 are the same code.

**Global k and m.** Simplicity. The parameters are still recorded per
object so the global value can change without breaking old data.

**Checksums per shard block, not parity-based detection.** Parity can
detect and correct errors without checksums but at half the repair
capability, with every read costing the full stripe, and with scrubbing
requiring the whole cluster over the network. A 64-bit checksum costs eight
bytes per block, converts every corruption into a locatable erasure, and
lets each device scrub itself. ZFS reached the same conclusion.

**Metadata replicated, not erasure-coded.** Metadata is tiny, is read
before any shard can be fetched, and is the only thing that knows where
shards are. Coding it would add k round trips to every read and buy
nothing. It is copied to every shard holder, as MinIO does.

**Key hash directories.** Keys are not filesystem-safe. Hashing gives fixed
length, path-safe, uniformly distributed names. The plain key is kept
inside the record for humans.

**fallocate reservation.** Guarantees space for a shard without writing,
with the filesystem as the arbiter, so concurrent coordinators cannot
overcommit and other processes cannot steal reserved space.

**Separate S3 process, native binary protocol.** S3's value is
compatibility, so implement it faithfully in a translation layer. The
native protocol is where streaming and per-block framing pay off.

---

## Appendix A: Erasure coding primer

A Reed-Solomon code takes k data symbols and produces k+m symbols such
that any k of them recover the original. Symbols are bytes. The encoding
is applied independently at every byte offset across the k+m shard blocks,
so a 1 MiB block is a million independent codewords.

Arithmetic is in GF(2^8): addition is XOR, multiplication is polynomial
multiplication modulo a fixed degree-8 polynomial (conventionally
`x^8 + x^4 + x^3 + x^2 + 1`). Encoding multiplies the k data bytes by a
fixed (k+m) × k matrix whose top k rows are the identity (so data shards
are stored as-is) and whose bottom m rows are chosen (Vandermonde or
Cauchy construction) so that every k × k submatrix is invertible.
Single-parity XOR is the case m = 1 with a parity row of all ones.

Decoding after up to m erasures: take any k surviving rows, invert that
k × k matrix, and multiply. If the erasure is a parity shard, nothing needs
decoding. If it is a data shard and the XOR parity survives, recovery is a
plain XOR of the other data shards and the parity.

The code corrects m *erasures* (missing shards at known positions) but only
floor(m / 2) *errors* (wrong shards at unknown positions). Checksums turn
errors into erasures, which is why this design uses both.

## Appendix B: Worked example

Cluster: two hosts, five devices, `k = 3`, `m = 1`, `B = 1 MiB`,
independence level `device`.

Object: 10 MiB.

- Stripe size is 3 MiB. Stripes: 3 MiB, 3 MiB, 3 MiB, 1 MiB.
- Stripes 1 to 3: three data blocks of 1 MiB and one parity block of 1 MiB.
- Stripe 4: remaining 1 MiB split into three blocks of 349,526 bytes
  (padded), plus one parity block of the same size.
- Four shard files, each holding four blocks: three of 1 MiB and one of
  349,526 bytes, plus a 4 KiB header. Each shard file is about 3.34 MiB.
- Stored total: about 13.4 MiB for 10 MiB of data, a ratio of 1.33.
- Four metadata records of a few hundred bytes each, one per holder.
- Read: fetch shard blocks 0, 1, 2 for each stripe from three devices,
  verify four × three checksums, deliver 10 MiB. The parity device is not
  touched.
- Loss of any one device: every object remains recoverable from the other
  three shards. Writes continue only if four eligible devices remain, so
  with five devices one loss leaves exactly four and writes still succeed;
  a second loss stops writes.
