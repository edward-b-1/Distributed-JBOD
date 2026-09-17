# Distributed Object Store: Design Specification

Draft 3, 16 September 2026. Revised after review of drafts 1 and 2.
Approved for implementation; remaining [O] and [P] items are settled when
the part they affect is built.

Every numbered item carries one of these markers:

- **[D]** Decided in discussion.
- **[P]** Proposed. Follows from a decision but has not been explicitly
  approved. Approve, amend, or reject.
- **[O]** Open. Needs a decision before implementation of the affected part.
- **[X]** Deferred. Agreed to be out of scope for the first version.

### Changes since draft 2

- Cluster secret included in v1 (6.1.2).
- Buckets: the `default` bucket is a top-level directory under `objects/`
  and the key hash covers the key alone (9.1). Buckets stay independent on
  the filesystem and need no migration when more are added.
- PUT to an existing key replaces it; decided (9.2.4).

### Changes since draft 1

- Buckets deferred. Keys form one flat namespace within the single bucket
  `default` (2, 9.1).
- No key length limit beyond a configurable sanity limit (9.1.5).
- Versioning deferred. At most one version per key in v1 (9.2).
- Failure domains deferred. Independence level fixed to `device` (7).
- Checksum and key hash algorithms are fixed by on-disk format version, not
  configurable (6.2.2, 8.3.2, 9.1.2).
- Placement is deterministic most-free-first (10.5).
- Coordinator coding concurrency limit deferred (6.1.2).
- Added: re-encode operation for changing `k` or `m` (18.9); per-operation
  specification of the native protocol (19.1); deferred items for an
  in-memory cache, an administration web UI, access keys, and TLS (22).
- S3 translation layer marked as a separate, deferred stream of work (19.2).
- Removed inline small-object storage from the deferred list. The format
  does not vary with object size.
- Reopened for discussion with recommendations: content length requirement
  (10.1), space reservation (10.6), one file per shard versus one file per
  block (9.3), key hash algorithm (9.1.2).

---

## 1. Purpose and scope

1.1 [D] The system aggregates the disks of several commodity machines into
one object store with a flat key namespace, whole-object semantics,
protection against device loss, and protection against silent data
corruption (bitrot).

1.2 [D] Target hardware is heterogeneous: machines of differing compute and
memory, each holding an arbitrary collection of disks of differing sizes.
Deployment target is Linux (Ubuntu Server or similar), one ext4 filesystem
per disk, each mounted separately.

1.3 [D] The first version supports whole-object PUT, GET, and DELETE by
exact key. Listing is supported but is permitted to be slow. Range reads,
multipart upload, other partial-object operations, and object versioning
are out of scope.

1.4 [D] The system is not designed for high availability under frequent
intermittent failures. It is designed for correctness and durability on
small clusters, with an administrator in the loop when something breaks.

## 2. Terminology

| Term | Meaning |
|------|---------|
| **Node** | One machine running one instance of the storage process. |
| **Device** | One filesystem, mounted at one path, managed by one node. The smallest unit of independent failure. |
| **Coordinator** | The node a client connects to for a given request. Any node can be a coordinator. |
| **Key** | A UTF-8 string naming an object. No fixed length limit (9.1.5). |
| **Bucket** | A namespace for keys, and a top-level directory on every device. v1 has exactly one, named `default`, and the API exposes no bucket concept. |
| **Object** | The data stored under a key, plus its metadata. |
| **Version** | One immutable body written under a key. Versioning is deferred; v1 keeps at most one version per key, but every body still has a version id (9.2). |
| **Stripe** | A fixed-size window of an object's bytes, the unit of erasure coding. |
| **Shard block** | One device's piece of one stripe. The unit that carries a checksum. |
| **Shard file** | All of one device's shard blocks for one version, concatenated, plus a header. See 9.3 for the open alternative. |
| **Shard index** | The position 0 .. k+m-1 of a shard within its stripe. Indices 0 .. k-1 are data, k .. k+m-1 are parity. |
| **Metadata record** | The small document describing one version: where its shards are and how it was encoded. |
| **Failure domain** | A grouping of devices that are expected to fail together. Deferred; v1 knows only devices. |

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

3.8 [D] **Uniform format.** The on-disk format does not depend on object
size. Small objects and large objects are stored the same way.

### Non-goals for the first version

3.9 [D] High availability. Any unreachable node fails the request.

3.10 [D] Automatic healing. Corruption and loss are reported, not repaired
inline.

3.11 [D] Fast listing. Listing is a cluster-wide scan.

3.12 [D] Users, permissions, client access keys, and TLS. Some minimal form
may be added later and must remain optional for LAN deployments (22). The
cluster secret between nodes (6.1.2) is in v1.

3.13 [D] S3 compatibility. Planned as a separate stream of work (19.2).

## 4. Architecture overview

```
   native clients ------------------------------>  +-----------------+
                                                   |  Node A         |<---> devices
   S3 clients --> [ S3 translation process ] ----> |  (coordinator   |
                  (deferred, separate work)        |   for this req) |
                                                   +--------+--------+
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
coordinator chooses which devices hold each shard using current free space,
and records the choice in the metadata record. Nothing about an object's
location can be derived from its key alone. The record is what is found by
broadcast.

4.3 [D] The metadata record for a version is stored in full on every device
that holds a shard of that version. There are therefore k+m identical copies.

4.4 [P] Each node process is single-threaded in the sense of one thread of
control, but it must serve several connections concurrently, for example
with an event loop. A node that handled one connection at a time could
deadlock: node A coordinating a write needs node B to accept a shard while
node B, coordinating its own write, needs the same of node A.

## 5. Nodes and devices

5.1 [D] A node's local configuration lists the devices it manages as
filesystem paths, for example `/mnt/disk0/data`, `/mnt/disk1/data`.

5.2 [D] On first use, the node writes a device identity file `device.json`
at the root of the device path. Fields:

```
system           "distributed-jbod"   identifies the file as belonging to this system
format_version   integer              on-disk format version (fixes algorithms, 6.2.2)
device_uuid      UUID                 freshly generated; the device's name in every record
cluster_id       UUID                 the cluster this device was initialised into
created          RFC 3339 timestamp
```

The UUID is the device's name in every record. Paths are never recorded.

5.3 [D] At startup the node checks the filesystem id (`st_dev`) of every
configured path. If two paths resolve to the same filesystem, the node
refuses to start. This catches the misconfiguration where two "devices" are
one disk.

5.4 [P] At startup the node also refuses to start if a device identity file
has a `system` field other than `distributed-jbod`, names a different
cluster id, has an unsupported format version, or if the same device UUID
is claimed by two paths.

5.5 [D] A device reports free space as the filesystem's free bytes as
returned by `statvfs`, minus a configured headroom, minus the sum of
reservations currently in flight on that device (if reservation is used,
10.6).

## 6. Configuration

Configuration has three layers.

### 6.1 Per-node (local file)

6.1.1 [D] Node UUID, listen addresses, the list of device paths, one or
more bootstrap peer addresses, and the cluster secret.

6.1.2 [D] **Cluster secret.** A shared token, the same on every node, that
a node must present when joining and on every node-to-node connection. Its
purpose is to stop an unrelated machine on the same network from joining
the cluster or impersonating a node. It is not encryption and does not
protect data on the wire. Included in v1. The presentation mechanism is
part of the connection handshake (19.1.2); a challenge-response so the
secret never crosses the wire in clear is preferred over sending it.

6.1.3 [X] Coordinator coding limit: the maximum number of concurrent
server-side encode or assemble streams a node will accept, with zero
meaning the node serves lookups and shard transfers but never codes on
behalf of a client (section 17). Deferred. In v1 a coordinator accepts all
coding work.

### 6.2 Cluster-wide (versioned document held by every node)

6.2.1 [D] The document carries a monotonically increasing version number
and a cluster id. Every node holds a copy. A joining node fetches it from a
bootstrap peer after presenting the cluster secret.

6.2.2 [D] Contents: `k`, `m`, shard block size `B`, the independence level
(section 7; the only valid value in v1 is `device`), the headroom fraction,
the node list (UUID, addresses), and the device list (UUID, owning node,
state).

The checksum algorithm and key hash algorithm are **not** configuration.
They are fixed by the on-disk format version. Changing either is a format
version change.

6.2.3 [D] `k`, `m`, and `B` are global. Every object in the cluster is
encoded with the same parameters at the time it is written. There is no
per-object redundancy policy. Changing them later is an administrative
operation that re-encodes existing objects (18.9).

6.2.4 [P] Sanity limits, enforced when the document is applied:
`1 <= k <= 32`, `0 <= m <= 8`, `k + m <= 64`, `B` a multiple of 4096 with
`64 KiB <= B <= 64 MiB`. The bounds are generous and exist only to reject
typos.

6.2.5 [D] Device states are `active`, `draining`, and `removed`. Only
`active` devices receive new shards.

6.2.6 [O] The mechanism by which the cluster document is changed and
propagated without a master. Proposed for v1: an administrative command,
issued through any node, that requires every node in the current document
to be reachable and to acknowledge the new version before the change is
considered applied. This is consistent with the fail-stop rule.

6.2.7 [D] All nodes must hold the same document version to serve requests.
A node that finds itself holding a different version from a peer during a
request returns an error.

### 6.3 Per-object (in the metadata record)

6.3.1 [D] Each version's metadata record stores the `k`, `m`, and `B` it
was written with, and the format version, which fixes the checksum and key
hash algorithms. This is a **record of the global values at write time**,
not a per-object policy. It exists for two reasons: a recovery tool can
read a disk with no cluster configuration, and the global values can be
changed (18.9) without making existing objects unreadable during or after
the migration.

## 7. Failure domains

7.1 [X] A hierarchy of failure domains (device, host, rack, datacenter,
region) with a configurable independence level was discussed and is
deferred. The design should not preclude it: placement is already a
function of a device list, and adding labels to devices and a filter to
placement later changes nothing else.

7.2 [D] In v1 the independence level is `device` and it is the only value
the cluster document accepts. The k+m shards of one stripe are placed on
k+m distinct devices. Two shards may share a host.

7.3 [D] If fewer than k+m active devices have room for the shard, the
write fails.

## 8. Data encoding

### 8.1 Code

8.1.1 [D] Objects are encoded with a systematic Reed-Solomon code over
GF(2^8) with k data shards and m parity shards. Any k of the k+m shards
recover the stripe.

8.1.2 [D] `k = 1` yields plain replication with `m+1` copies. `m = 0`
yields an unprotected distributed JBOD with checksums. Both are valid
configurations of the same code path.

8.1.3 [D] The encoding matrix, field polynomial, and generator are those of
the chosen library and are fixed by the on-disk format version.

8.1.4 [D] An existing, well-tested library is used. The code is not
implemented from scratch.

8.1.5 [P] The library's matrix construction should have the property that
the first `m'` parity rows for scheme `k+m'` equal the first `m'` parity
rows for scheme `k+m` whenever `m' < m`. Cauchy-style constructions with an
identity block on top have this property. It makes reducing `m` a matter of
deleting parity shards rather than recomputing them (18.9). Verified by
test for `reed-solomon-erasure` 6.0.0, which builds its matrix as
Vandermonde(k+m, k) times the inverse of its top k rows, so parity row j
depends only on j and k
(`crates/djbod-core/tests/erasure_library.rs`).

8.1.6 [X] A non-systematic (information dispersal) variant, in which no
shard contains plaintext, may be offered later as an option. It would
change only the encoding matrix.

### 8.2 Stripes and shard blocks

8.2.1 [D] The shard block size `B` is a global constant within the sanity
limits of 6.2.4. A stripe is `k × B` bytes of the object.

8.2.2 [D] `B` must be a multiple of 4096. Reason: 4096 is the page size on
every target CPU and the block size ext4 uses by default on any disk larger
than a few hundred megabytes. Blocks that start and end on 4096-byte
boundaries can be read and written without the kernel reading or rewriting
partial pages, and are compatible with direct I/O should it ever be wanted.
The restriction costs nothing and may be revisited.

8.2.3 [D] A stripe is split contiguously: bytes `0 .. B-1` of the stripe
form shard block 0, bytes `B .. 2B-1` form shard block 1, and so on. Every
data shard block is therefore a verbatim run of the original object.

8.2.4 [D] Parity shard blocks `k .. k+m-1` are computed from the k data
blocks byte-wise: byte j of each parity block depends only on byte j of
each data block.

8.2.5 [P] The final stripe of an object is usually short. Its k data
blocks are each `ceil(remaining / k)` bytes, zero-padded to that length,
and parity is computed over the padded blocks. The object's true length is
stored in the metadata record and padding is discarded on read. Every
block within one stripe therefore has equal length.

8.2.6 [P] An object of length zero has zero stripes and consists of a
metadata record alone.

### 8.3 Checksums

8.3.1 [D] Every shard block carries a 64-bit checksum computed over the
stored (padded) block.

8.3.2 [D] Algorithm: XXH3-64, fixed by format version 1. A cryptographic
hash is not required because the native protocol does not admit untrusted
writers in v1. Implemented in `crates/djbod-core/src/checksum.rs`, pinned
by a test against the published XXH3 value for empty input.

8.3.3 [D] Checksums are stored contiguously in a table, not interleaved
with the blocks, so that every block begins on a 4096-byte boundary. Where
the table lives depends on the resolution of 9.3.

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
        default/              one directory per bucket; v1 has only this one
            ab/               first two hex characters of the key hash
                cd/           next two hex characters
                    abcd...<64 hex>/          one directory per key
                        <version>.meta.json   the metadata record
                        <version>.<idx>.shard the shard file held on this device
```

9.1.1 [D] The object directory is named by the key hash, not by the key.
Keys are not filesystem-safe: they may exceed 255 bytes, contain any byte
but NUL, and an object `a` may coexist with an object `a/b`.

9.1.2 [D] **Key hash algorithm: SHA-256** over the key's raw bytes,
untruncated, rendered as 64 lowercase hex characters. The bucket is a
directory, not part of the hash. No normalisation is applied to the key:
two keys that differ in any byte are different keys, as in S3. Fixed by
on-disk format version 1. Alternatives considered are listed in 9.1.7.

9.1.3 [D] The two fan-out levels `ab/cd/` exist only to keep directory
sizes bounded. Without them, `objects/` would hold one entry per key on the
device. With two levels of 256 there are 65,536 leaf directories and a
device with ten million keys has about 150 entries per directory. The hash
is uniformly distributed, so fan-out is even.

9.1.4 [D] The original key is stored in plain text inside the metadata
record. A disk can be searched for an object by name with ordinary tools.

9.1.5 [D] There is no fixed maximum key length. A configurable sanity limit
rejects absurd keys by accident; proposed default 16 KiB. The key is stored
only inside the metadata record and in the wire protocol, so no filesystem
limit applies to it.

9.1.6 [D] **Collisions.** With a 256-bit hash, two distinct keys share a
directory only if SHA-256 collides, which has never been observed and
would need on the order of 2^128 objects to expect by chance. The design
nonetheless detects it rather than assuming it away, because the check is
free: every metadata record contains the plain key.

- **On read** (11.2), the coordinator compares the requested key with the
  key in every record found under the hash. A mismatch is an error naming
  both keys (16.1). The stored object is never returned for the wrong key.
- **On write** (10.2), before placing anything, the coordinator looks the
  hash up. If a record exists whose key differs from the key being
  written, the write is refused with the same error. The existing object is
  untouched and the new key cannot be stored in this cluster.
- **On delete and list**, the same comparison applies wherever a record is
  matched to a key.

No attempt is made to store two keys in one directory. The second key of a
colliding pair is simply unstorable, which is accepted given the odds. The
same check also catches two failures that are far more likely than a
collision: a record whose key field was corrupted on disk, and a record
written under the wrong directory by a software bug.

9.1.7 Key hash alternatives, for reference:

| Algorithm | Output | Notes |
|-----------|--------|-------|
| SHA-256 | 256 bit | Standard everywhere, hardware support on recent x86 and ARM. Recommended. |
| BLAKE3 | 256 bit | Published 2020, descendant of BLAKE2 and ChaCha. Very fast in software. Less widely available in standard libraries. |
| BLAKE2b | up to 512 bit | Standardised (RFC 7693), in many standard libraries. Fine. |
| XXH3-128 | 128 bit | Not cryptographic. Fast. Adequate only if all clients are trusted. |
| MD5 | 128 bit | Cryptographically broken. Swift uses it for placement. Not recommended for new work. |

9.1.8 [D] A device holds exactly one shard of any given version, so there
is at most one shard file per version per object directory. Which shard
index it holds varies from object to object.

9.1.9 [D] Buckets are top-level directories under `objects/`. v1 creates
only `default` and every operation acts within it. Adding buckets later
means creating a directory and exposing a bucket parameter in the API;
nothing on disk moves and the hash function is unchanged. Keeping each
bucket in its own directory also keeps them independent on the
filesystem: a bucket can be listed, scrubbed, or removed by path.

### 9.2 Version identifiers

9.2.1 [X] Object versioning (several bodies retained under one key) is
deferred. It is planned, may be implemented as a client-side wrapper, and
may ultimately be dropped.

9.2.2 [D] Every body written nonetheless receives a version identifier, so
that the layout and record format need not change if versioning is added.

9.2.3 [P] Version identifiers are ULIDs: 128 bits, time-ordered, generated
by the coordinator at write time. Sorting version file names
lexicographically yields creation order.

9.2.4 [D] **PUT to an existing key replaces it.** The coordinator writes
the new version fully (all shards and records durable), then deletes the
old version from its holders, then acknowledges. If the
coordinator dies between the two steps the key briefly has two versions;
reads return the newest by ULID and the scrubber or a later PUT removes the
older. This also resolves concurrent writes to the same key (21.1) as
last-writer-wins by ULID.

### 9.3 Shard file format

9.3.1 [O] **One file per shard, or one file per shard block.** Draft 1
proposed one file per shard: all of a device's blocks for a version in one
file with a header. The reviewer's assumption was one file per block. The
trade:

*One file per shard block.* Simplest possible file: block bytes, checksum
in a small header or in the file name. Every file has a known maximum
size of `B` plus a header. Blocks can be verified in isolation with no
shared structure. Cost: a 10 GiB object at `B = 1 MiB` and 3+1 produces
10,240 files per device and 40,960 files in total, each needing a create,
a write, an fsync, and an inode, in a directory of 10,240 entries. Small
files also defeat the disk's sequential read path.

*One file per shard.* One create, one fsync, one inode per device per
version. Blocks are contiguous so sequential reads run at disk speed. The
file size is bounded by the object size divided by k, which on ext4 is
16 TiB. Cost: a header with a checksum table that must be read before any
block can be verified, and a maximum object size that is a configuration
choice rather than a property of the format.

Recommendation: one file per shard, as below, with a configurable maximum
object size (default 1 TiB) enforced at PUT. If the reviewer prefers one
file per block, the change is confined to 9.3 and the shard transfer
operations in 19.1.

9.3.2 [P] Proposed shard file format:

```
offset 0        header, padded to a multiple of 4096 bytes
    magic                      8 bytes   identifies a distributed-jbod shard file
    format version             4 bytes
    reserved                   4 bytes
    block count                8 bytes
    block length B             8 bytes
    last block length          8 bytes
    header checksum            8 bytes   (over the header, table included)
    checksum table             8 bytes × block count

offset H        shard block 0
offset H + B    shard block 1
...
offset H + (n-1)B   shard block n-1 (last block length bytes, padded per 8.2.5)
```

The header size H is the smallest multiple of 4096 that holds the fixed
fields and the table. For `B = 1 MiB` one 4 KiB header covers objects up
to about 500 MiB per shard; larger objects use a larger header.

9.3.3 [D] Shard files are immutable once written. They are written to a
temporary name, fsynced, and renamed into place. They are never modified.

9.3.4 [D] Shard blocks start at offsets that are multiples of 4096.

9.3.5 [X] Fields identifying the object (key hash, version, shard index,
`k`, `m`, and the UUIDs of the sibling devices) could be added to the
header to make a shard self-describing without its metadata record. Parked
pending further discussion.

### 9.4 Metadata record

9.4.1 [D] One record per version, in a human-readable format (JSON),
stored on every device holding a shard of that version.

9.4.2 [P] Fields:

```
format_version      integer, fixes checksum and key hash algorithms
system              "distributed-jbod"
bucket              string, the bucket directory; always "default" in v1
key                 string
key_hash            hex string
version             ULID
created             RFC 3339 timestamp
size                integer, true object length in bytes
k, m                integers, the global values when written (6.3)
block_size          integer, B when written
shards              array of { index, device_uuid, node_uuid }
content_type        string, optional
user_metadata       opaque map, optional, reserved for clients and the
                    future translation layer
```

9.4.3 [D] The record is written after all shard files of the version are
durable, using the same temporary-name, fsync, rename procedure, followed
by an fsync of the directory.

9.4.4 [D] Because the record is on every holder, and every read must reach
every node, the read path checks that all copies agree. Disagreement is an
error (section 16).

## 10. Write path (PUT)

10.1 [O] **Whether content length is required.** Draft 1 required it.
Three things use it: choosing devices with enough room, reserving space
(10.6), and writing a header whose checksum table is sized by block count.
Without it the coordinator must pick devices blind, a write can fail
partway when a device fills, and the checksum table must move to a trailer
at the end of the file, written after the last block. All of that is
workable. Recommendation: require content length in v1. Every native
client knows the size of what it is sending, and S3 requires it on PUT as
well. Unknown-length streaming can be added later by switching the table
to a trailer.

10.2 [D] The coordinator generates a version id and computes the key hash,
stripe count, and shard file size.

10.3 [D] The coordinator obtains current free space for every device by
querying every node. Any node failing to respond fails the write.

10.4 [D] The coordinator chooses k+m distinct `active` devices each with
free space of at least one shard file.

10.5 [D] Selection is deterministic: the k+m devices with the most free
space in absolute bytes, ties broken by device UUID. Consequences worth
knowing: with 1 TB and 2 TB disks the 2 TB disks receive all writes until
their free space matches the 1 TB disks, after which writes alternate,
so all disks fill at the same absolute rate and reach full together. Two
coordinators writing at once will pick the same devices; if reservation
(10.6) is used this is safe, merely uneven, and the second write moves on
if the first fills the device. Randomised or round-robin selection for load
spreading is deferred.

10.6 [O] **Space reservation.** Three options were discussed.

*Reserve the entire device up front.* Either one preallocated file per
device with an allocator inside it, which is a volume format and a large
piece of work, or a pool of preallocated fixed-size block files claimed by
rename. The pool variant is simple but every object's final block occupies
a whole `B`-sized file, wasting up to `(k+m) × B` per object, which at
`B = 1 MiB` is several megabytes per small object. It also fixes the file
count at format time and means the per-key directory holds no data, only
pointers, changing section 9 substantially.

*Reserve per write with `fallocate(2)`.* When a shard transfer begins the
receiving node preallocates the shard file to its final size. This is a
filesystem metadata operation, writes nothing, causes no wear, and makes
the filesystem the arbiter: two coordinators cannot overcommit, another
process cannot take reserved space, and a write that starts will not run
out of room. Requires the size up front (10.1). `posix_fallocate` must not
be used; it silently writes zeros on filesystems without native support.

*No reservation.* Check free space, write, and if the device fills
mid-stream fail the write and clean up. Consistent with fail-stop. The
only cost is that near-full devices produce failures the coordinator could
have avoided.

Recommendation: `fallocate` per write. It is a single system call in the
existing write path and removes a class of failures.

10.7 [D] For each chosen device, the coordinator opens a shard transfer.
If the receiving node cannot create or reserve the shard file (for
example `ENOSPC`), it refuses the shard and the coordinator chooses the
next-most-free eligible device. If none exists the write fails and
everything written so far is removed.

10.8 [D] The coordinator (or the client, section 17) reads the body one
stripe at a time, splits it into k data blocks, computes m parity blocks,
computes k+m checksums, and streams block and checksum to each holder.
Memory in use per request is bounded by one stripe plus parity.

10.9 [D] Each receiving node writes blocks into the shard file, writes the
header last, fsyncs, and renames. It then writes the metadata record the
coordinator sends, fsyncs, renames, and fsyncs the directory.

10.10 [D] The coordinator acknowledges the write to the client only when
all k+m shard files and all k+m metadata records are durable. Any failure
at any step fails the whole write. The coordinator makes a best effort to
delete temporary files on failure.

10.11 [O] Cleanup of temporary files left by a coordinator that crashed
mid-write. Proposed: a device deletes temporary files older than a
configurable age at startup and during scrub.

## 11. Read path (GET)

11.1 [D] The client sends a key to a coordinator.

11.2 [D] The coordinator broadcasts a lookup (section 13) and receives the
metadata records from every holder. It checks that the key in the record
matches the requested key (9.1.6) and that all copies agree. If more than
one version is present (9.2.4) it selects the newest.

11.3 [D] For each stripe in order, the coordinator requests shard blocks
`0 .. k-1` (the data blocks) from their holders, verifies each block against
its checksum, and delivers the stripe to the client. Parity blocks are not
read.

11.4 [D] If any block fails its checksum, or any holder is unreachable, the
request fails with an error identifying the device UUID, key, version,
shard index, and stripe number. No reconstruction is attempted in v1.

11.5 [D] Reads stream. The coordinator holds a bounded number of stripes in
memory at once.

11.6 [D] The last stripe is truncated to the object's true length before
delivery.

## 12. Placement summary

For reference, a stripe of an object written with 3+1 on a cluster of two
hosts (A: three devices, B: two devices):

```
shard 0 (data)   -> host A, device a1
shard 1 (data)   -> host B, device b1
shard 2 (data)   -> host A, device a3
shard 3 (parity) -> host B, device b2
```

Every stripe of the version uses the same four devices in the same order.
Every one of the four devices holds a copy of the metadata record. Since
v1 places at device level, the split across hosts is incidental.

## 13. Lookup by broadcast

13.1 [D] To find an object, the coordinator sends a lookup for the key
hash to every node in the cluster document and waits for every node to
respond.

13.2 [D] Each node checks each of its devices for a directory at the key
hash and returns every metadata record found there.

13.3 [D] If any node fails to respond within the timeout, the lookup fails.
Because every node must answer, a lookup that finds nothing is an
authoritative "not found".

13.4 [D] No node stores any index of what other nodes hold.

13.5 [D] Fan-out is one request per node, not per device.

## 14. Delete

14.1 [D] Deleting a key looks up its version(s), instructs every holder to
delete the shard file and metadata record, and reports success only when
all have confirmed. Any unreachable holder fails the delete.

14.2 [P] Holders delete the metadata record first, then the shard file,
then remove the key directory if empty. A partially deleted version, one
where some holders still have a record, is reported by lookup as
inconsistent (16.1) and the delete can be retried.

14.3 [X] Delete markers and versioned-delete semantics belong to
versioning and are deferred with it.

## 15. Listing

15.1 [D] Listing keys, optionally by prefix, broadcasts to every node.
Each node scans the metadata records on each of its devices and returns
matching keys. The coordinator merges, deduplicates (since each record
exists on k+m devices), sorts, and returns.

15.2 [D] This is a full scan of every device and is accepted as slow.

15.3 [X] A sorted index to make listing fast is deferred.

## 16. Failure semantics

16.1 [D] The system is fail-stop. The following conditions return an error
to the client:

- Any node in the cluster document does not respond to a broadcast.
- Any device holding a shard needed for a read is unreachable.
- Any shard block fails its checksum.
- Metadata record copies for a version disagree, or fewer than k+m are
  found.
- The key in a record does not match the requested key (hash collision or
  corruption).
- Fewer than k+m eligible devices exist for a write.
- A write fails on any device and no replacement is found.
- Nodes disagree on the cluster document version.
- A key exceeds the sanity limit, or an object exceeds the maximum size.

16.2 [D] Errors carry enough detail for an administrator to act: the
condition, the node and device UUIDs involved, and for data errors the
key, version, shard index, and stripe.

16.3 [D] Clients may retry. The system does not retry internally except for
choosing a replacement device during a write (10.7).

16.4 [D] Corruption and loss are repaired by administrative action
(section 18), not by the read path.

16.5 [X] Inline reconstruction on checksum failure, with the client
receiving correct data while the fault is logged and queued for repair, is
a planned extension.

## 17. Coordinator role and reconstruction policy

17.1 [D] Erasure coding work (encoding on write, and assembling or
reconstructing on read) can be done by the coordinator or by the client.
Which one is chosen per request.

17.2 [D] A client indicates whether it wants the coordinator to perform
coding or wants to do it itself. A native client that codes for itself
receives (on read) the metadata record and fetches shard blocks directly
from the holding nodes, and (on write) asks the coordinator for a
placement, encodes stripes, sends shard streams directly to the chosen
devices, and asks the coordinator to commit the record (19.1).

17.3 [X] A node may refuse to perform coding, governed by the local limit
of 6.1.3. Deferred with it. In v1 every node accepts coding work.

17.4 [X] A refusing node returns the metadata record (for reads) or the
chosen placement (for writes) so the client may proceed itself, and may
also name another node willing to accept the work. Deferred with 17.3.

17.5 [D] This mechanism is how deployments spread coding CPU across
machines of unequal capability, or steer it to a strong machine.

17.6 [D] Regardless of where coding happens, the receiving node computes
each shard block's checksum itself and does not trust a checksum supplied
by the client.

## 18. Membership: add, drain, remove, repair, rebalance, re-encode

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

18.9 [P] **Re-encode (change `k` or `m`).** A drain needs a spare eligible
device to receive each shard. A cluster with exactly k+m devices therefore
cannot decommission one without either adding a device first or lowering
`m`. Procedure for changing the global parameters:

1. The administrator applies a new cluster document with the new `k`, `m`,
   or `B`. From that moment new writes use the new values.
2. Existing objects, identifiable by the values recorded in their metadata
   (6.3), continue to be readable with their own values.
3. A background re-encode job visits every version whose recorded values
   differ from the current ones and rewrites it. Reducing `m` with a
   library satisfying 8.1.5 is deletion of the surplus parity shards and a
   record update. Increasing `m` is computing the new parity shards from
   k data shards and placing them. Changing `k` or `B` is a full read and
   rewrite of the version as a new version, followed by deletion of the old.
4. When no version with old values remains, the migration is complete.

This is the same job as 18.8 with a different rule for which shards to
produce. Its existence is the reason 6.3 records the parameters per
version.

## 19. Protocols

### 19.1 Native protocol

19.1.1 [D] The native protocol is binary, framed, and streaming, over TCP.
Frames carry one shard block and its checksum so that the wire unit matches
the storage unit.

19.1.2 [P] **Framing.** Every message is a fixed header followed by a
payload:

```
message type        2 bytes
flags               2 bytes
request id          4 bytes    correlates responses and streams with requests
payload length      4 bytes
payload             payload length bytes
```

Control payloads (requests, responses, records) are encoded in a compact
self-describing format; CBOR is proposed. Data payloads (shard blocks) are
raw bytes preceded by their 8-byte checksum. A streaming operation is one
request message followed by a sequence of data messages sharing its
request id, terminated by an end-of-stream message carrying a status.

19.1.3 [P] **Operations.** Two groups: those a client sends to a
coordinator, and those nodes send to each other. Every response is either
`OK` with the fields shown or `ERROR` with the detail required by 16.2.

**Client to coordinator**

`Status`
: Request: none. Response: cluster id, document version, coordinator node
  UUID, and for every device in the cluster: UUID, owning node, state,
  total bytes, free bytes. Implemented by broadcasting `LocalStatus`.

`PutObject`
: Request: key, size, optional content type, optional user metadata.
  Followed by a stream of `size` body bytes in frames of any length.
  Coordinator performs placement, encoding, and shard transfer. Response:
  version id. Fails per section 16.

`GetObject`
: Request: key. Response: the metadata record, then a stream of body
  bytes in stripe-sized frames, then end-of-stream. Coordinator performs
  lookup, block fetch, and checksum verification.

`HeadObject`
: Request: key. Response: the metadata record, no body. Implemented by
  broadcast lookup.

`DeleteObject`
: Request: key. Response: none. Section 14.

`ListKeys`
: Request: optional prefix, optional start-after key, optional limit.
  Response: sorted list of keys and, for each, size and version id; plus a
  flag saying whether more remain. Section 15.

`PlaceObject` (client-side coding, 17.2)
: Request: key, size. Response: version id, key hash, and the ordered list
  of k+m (device UUID, node address) chosen by 10.4 and 10.5. The
  coordinator opens no transfers; the client sends `PutShard` to each
  holder itself.

`CommitObject` (client-side coding)
: Request: the complete metadata record for a version whose shards the
  client has finished sending. The coordinator verifies that every listed
  holder reports the shard file present and complete (`GetMeta` with a
  probe flag, or a dedicated check), then sends `PutMeta` to every holder,
  then, if a previous version of the key exists, deletes it (9.2.4).
  Response: none.

`LocateObject` (client-side coding)
: Request: key. Response: the metadata record and, for each shard, the
  node address to fetch it from. The client then issues `GetShard`
  directly. Identical to `HeadObject` plus addresses.

**Node to node**

`LocalStatus`
: Request: none. Response: node UUID, document version, and for each
  local device: UUID, state, total bytes, free bytes (5.5).

`LocalLookup`
: Request: key hash. Response: every metadata record found under that
  hash on any local device, each tagged with the device UUID it was read
  from. Empty list if none.

`LocalList`
: Request: optional prefix, optional start-after, optional limit.
  Response: for each matching record on any local device, the key, size,
  and version id. Duplicates across devices are the coordinator's problem.

`PutShard`
: Request: device UUID, key hash, version id, shard index, block count,
  block length, last block length. The node creates the temporary shard
  file, reserves it (10.6), and responds `READY`. Then a stream of exactly
  `block count` data frames, each a checksum and a block; the node
  recomputes the checksum and rejects the stream on mismatch (17.6). On
  end-of-stream the node writes the header, fsyncs, renames, and responds
  `OK`. Any failure deletes the temporary file and responds `ERROR`.

`GetShard`
: Request: device UUID, key hash, version id, shard index, first block,
  block count. Response: a stream of data frames, each the stored checksum
  and block, then end-of-stream. The sending node does not verify
  checksums; the receiver does. A missing file is an `ERROR`.

`PutMeta`
: Request: device UUID, key hash, version id, metadata record. The node
  writes it with the procedure of 9.4.3. Response: none.

`GetMeta`
: Request: device UUID, key hash, version id. Response: the record, or
  `ERROR` if absent. With a `probe` flag the response also states whether
  the shard file for this version is present on that device.

`DeleteVersion`
: Request: device UUID, key hash, version id. The node deletes the record,
  then the shard file, then the directory if empty (14.2). Response: none.
  Deleting something already absent is `OK`.

`AbortShard`
: Request: device UUID, key hash, version id, shard index. Deletes a
  temporary or complete shard file and any record for that version on
  that device. Used by a coordinator cleaning up a failed write.

`GetClusterConfig`
: Request: none. Response: the full cluster document.

`ApplyClusterConfig`
: Request: a cluster document with version one greater than the current.
  Response: `OK` once the node has durably stored it and switched to it, or
  `ERROR` if the version is not exactly current plus one. The coordinating
  administrative command sends this to every node and reports failure if
  any node fails (6.2.6). Whether a two-phase prepare and commit is needed
  is part of open question 21.2.

19.1.4 [D] Client-facing operations never require the client to know
about devices, nodes, or the cluster document, except the three
client-side coding operations, which expose device UUIDs and node
addresses by design.

19.1.5 [D] Every node-to-node connection begins with a handshake that
proves knowledge of the cluster secret (6.1.2) and exchanges cluster id
and document version. A connection failing the handshake is closed.

19.1.6 [X] Client authentication (an access key) and TLS are deferred and
must remain optional for LAN deployments. The `flags` field in the frame
header is reserved so a later version can negotiate them in the handshake.

### 19.2 S3 translation layer

19.2.1 [X] S3 compatibility is provided by a separate process that speaks
S3 to clients and the native protocol to the cluster. It may run on one
node, on every node, or elsewhere. It is a separate stream of work,
deferred, and likely to be included in a first release.

19.2.2 [D] The translation layer aims for faithful S3 semantics. The value
of S3 compatibility is existing tooling; improvements belong in the native
protocol.

19.2.3 [P] The translation layer owns everything the native layer does not:
authentication, policies, bucket configuration, sorted listing, multipart
assembly, and S3 versioning semantics including delete markers. It stores
what it needs in `user_metadata` or in its own state. Until more than one
native bucket exists it serves a single S3 bucket mapped to `default`.

## 20. Operations and tooling

### 20.1 Scrubber

20.1.1 [X] Not in v1, but required before the system is trusted with data.
Because every shard block has a checksum stored beside it, a device can be
scrubbed locally at disk speed with no network traffic and no coordination.

20.1.2 [O] Two designs were identified. A separate process per machine that
asks the node, over a local socket, to verify shard files at a configured
rate. Or a separate process that reads the on-disk format directly. The
first is simpler and reuses the node's code; the second is independent of
the node's health and probably more efficient.

20.1.3 [D] Since reads report rather than heal, scrubbing is the mechanism
by which corruption is found before a client encounters it.

### 20.2 Recovery tool

20.2.1 [P] A single static binary that, given one or more device paths and
no running cluster, lists the versions present and reassembles any version
for which k shards can be found among the given paths. Requires only the
metadata records and shard files. This is the answer to the loss of
human-readable on-disk layout, and the reason 6.3 records encoding
parameters per version.

### 20.3 Administration

20.3.1 [X] A web UI for administration (cluster status, device states,
drain and repair, errors) is required eventually and deferred. Every
administrative action it performs must also be available as a command-line
operation over the native protocol.

### 20.4 ext4 deployment notes

20.4.1 [D] One ext4 filesystem per disk, mounted separately.

20.4.2 [P] Format and mount recommendations for data devices:

- Set reserved blocks to zero: `tune2fs -m 0 <dev>`. The default reserves
  five percent for root.
- Consider a higher inode ratio at format time if many small objects are
  expected. Each version costs one directory (shared per key), one metadata
  file, and one shard file per holding device.
- Mount with `noatime`.
- `fallocate` is natively supported on ext4 and XFS. ZFS does not support
  it and Btrfs cannot guarantee it. If 10.6 adopts `fallocate`, the system
  requires native support.

20.4.3 [D] Filename limit is 255 bytes and path limit is 4096 bytes. The
layout in section 9 uses fixed-length names and stays well within both.

## 21. Open questions

| # | Question | Where | Recommendation |
|---|----------|-------|----------------|
| 21.1 | How the cluster document is changed without a master. | 6.2.6 | All-nodes-acknowledge command. |
| 21.2 | Cleanup of orphaned temporary files. | 10.11 | Age-based deletion at startup and scrub. |
| 21.3 | One file per shard or per shard block. | 9.3.1 | Per shard. |
| 21.4 | Content length required on PUT. | 10.1 | Required. |
| 21.5 | Space reservation. | 10.6 | `fallocate` per write. |
| 21.6 | Scrubber architecture. | 20.1.2 | Direct on-disk reader. |
| 21.7 | Free-space query on every write versus a cached heartbeat. | 10.3 | Query per write. |
| 21.8 | Control payload encoding. | 19.1.2 | CBOR. |

## 22. Deferred items

- Object versioning (9.2). Planned; may be a client-side wrapper; may be
  dropped.
- Buckets beyond `default` (2, 9.1.9).
- Failure domain hierarchy and configurable independence level (7).
- Coordinator coding limit and refusal (6.1.3, 17.3, 17.4).
- Randomised or round-robin placement for load spreading (10.5).
- Inline reconstruction on read (16.5).
- Scrubber (20.1).
- Rebalance (18.7).
- Self-describing shard headers (9.3.5).
- Non-systematic encoding option (8.1.6).
- Optional parity verification on read, for deployments that want it.
- Range reads and multipart upload.
- Unknown content length on PUT, via a trailer checksum table (10.1).
- In-memory cache of metadata records and object data.
- Administration web UI (20.3).
- Users, permissions, client access keys, TLS. All optional for LAN
  deployments (19.1.6).
- S3 translation layer (19.2). Separate stream of work.
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

**Most-free-first placement.** Deterministic and simple. Fills devices at
equal absolute rates so mixed sizes reach full together. Uneven load under
concurrent writes is accepted for v1.

**Systematic Reed-Solomon.** One library covers replication (`k = 1`),
RAID 5 and 6 equivalents, and wide schemes, so the choice of redundancy is
configuration rather than code. Systematic form means healthy reads need
no decoding and data shard blocks are verbatim runs of the object.
RAID-Z2 and RAID 6 are the same code.

**Global k, m, and B, recorded per version.** One scheme for the whole
cluster, for simplicity. The values are still written into each record so
that a recovery tool needs no configuration and so that the global values
can be changed with a background re-encode instead of a flag day.

**Checksums per shard block, not parity-based detection.** Parity can
detect and correct errors without checksums but at half the repair
capability, with every read costing the full stripe, and with scrubbing
requiring the whole cluster over the network. A 64-bit checksum costs eight
bytes per block, converts every corruption into a locatable erasure, and
lets each device scrub itself. ZFS reached the same conclusion.

**Algorithms fixed by format version.** Checksum, key hash, and code
matrix are properties of the on-disk format, not configuration. Changing
one is a format upgrade with a migration, not a setting.

**Metadata replicated, not erasure-coded.** Metadata is tiny, is read
before any shard can be fetched, and is the only thing that knows where
shards are. Coding it would add k round trips to every read and buy
nothing. It is copied to every shard holder, as MinIO does.

**Key hash directories.** Keys are not filesystem-safe. Hashing gives fixed
length, path-safe, uniformly distributed names. The plain key is kept
inside the record for humans and for collision detection.

**One bucket, no versioning, no failure domains in v1.** Each is a
feature the format can accommodate later (bucket as a directory, version
id in every file name, device list ready for labels) without a migration,
so none needs to be built now.

**PUT replaces.** Without versioning a key has one body. Writing the new
body fully before deleting the old one means a crash never leaves the key
empty, and gives concurrent writers a last-writer-wins outcome by ULID.

**Uniform format regardless of object size.** One code path for storage,
scrub, repair, and recovery. The cost of a small object is one small shard
file per holder, accepted.

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
plain XOR of the other data shards and the parity. Only k valid shards are
ever needed; with one erasure and m = 2, the second parity shard is
unused. Its purpose is a second simultaneous loss.

The code corrects m *erasures* (missing shards at known positions) but only
floor(m / 2) *errors* (wrong shards at unknown positions). Checksums turn
errors into erasures, which is why this design uses both.

## Appendix B: Worked example

Cluster: two hosts, five devices, `k = 3`, `m = 1`, `B = 1 MiB`.

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
- Decommissioning a device with five devices and 3+1: drain needs a fifth
  device to receive each moved shard, and after the drain four remain, so
  this works once. Decommissioning a second device requires adding one
  first or re-encoding to `m = 0` (18.9).

## Appendix C: Implementation plan

C.1 [D] **Language: Rust.** Chosen because the reviewer reads Rust today.
Static binaries, no garbage collector, direct system call access, and
mature libraries for everything the design needs. Garage is the reference
point for a Rust system of this shape.

C.2 [P] **Crate layout.** One Cargo workspace:

| Crate | Contents |
|-------|----------|
| `djbod-core` | On-disk format (device identity, shard file, metadata record), key hash, block checksums, Reed-Solomon wrapper, stripe encode and decode. No networking. Fully unit-tested, including round-trips through the code with every erasure pattern up to `m`. |
| `djbod-proto` | Native protocol: frame header, handshake, CBOR message types for every operation in 19.1.3. Shared by node, client, and tools. |
| `djbod-node` | The node process. Device management, local operations, coordinator logic (placement, broadcast, streaming PUT and GET), cluster document. |
| `djbod-cli` | Command-line client and administrative commands over the native protocol. |
| `djbod-recover` | The offline recovery tool of 20.2, built on `djbod-core` only. |

C.3 [P] **Candidate dependencies**, to be confirmed at each milestone.
Findings so far on `reed-solomon-erasure` 6.0.0 from the library tests:
it is systematic, recovers every erasure pattern up to `m`, refuses `m+1`
erasures, trusts unmarked corrupt shards (confirming the need for
checksums, 8.3), is byte-wise independent (streaming is valid), satisfies
8.1.5, treats `k = 1` as replication, rejects `m = 0` (our wrapper must
special-case it, 8.1.2), and allows `k + m <= 256`. Without SIMD it
encodes 3+1 at about 6 GiB/s and 10+4 at about 1.5 GiB/s of data on one
core, and reconstructs one shard at about 6 GiB/s. Dependencies:
`tokio` (async runtime and networking), `reed-solomon-erasure` (classic
GF(2^8) systematic Reed-Solomon, a port of the Go library MinIO uses) with
`reed-solomon-simd` as the alternative if throughput demands it,
`xxhash-rust` (XXH3-64), `sha2` (SHA-256), `ciborium` or `minicbor`
(CBOR), `ulid`, `uuid`, `serde` and `serde_json` (metadata records),
`rustix` or `nix` (`fallocate`, `statvfs`, `st_dev`), `clap`, `tracing`,
`thiserror`.

C.4 [P] **Milestones.** Each ends with something that runs and is tested.

1. **Core format.** `djbod-core` complete. A test writes an object into
   shard files in temporary directories, corrupts or deletes up to `m` of
   them, and reads it back. Settles 21.3 (hash) and 21.4 (file per shard)
   in code. Verifies whether the library satisfies 8.1.5.
2. **Single node.** A node process managing several device directories on
   one machine, the native protocol, and a CLI. PUT, GET, DELETE, and LIST
   work end to end against a cluster of one node. Settles 21.5 (content
   length), 21.6 (reservation), and 21.9 (CBOR).
3. **Cluster.** Cluster document, handshake with the cluster secret,
   broadcast lookup, placement across nodes, fail-stop error propagation.
   Integration tests start several node processes on localhost with
   directories as devices, kill one, and check every error in 16.1 is
   produced with the detail in 16.2. Settles 21.1 and 21.8.
4. **Administration.** Drain, repair, re-encode, the recovery tool, and
   the scrubber. Settles 21.2 and 21.7.

C.5 [P] **Testing stance.** Devices in tests are ordinary directories.
Multi-node tests run real node processes on one machine. Every failure
condition in 16.1 has a test that provokes it. Corruption tests flip bytes
in shard files directly.
