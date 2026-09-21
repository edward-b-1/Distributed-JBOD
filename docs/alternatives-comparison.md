# Distributed-JBOD compared with MinIO, Garage, Ceph, SeaweedFS, and RustFS

This comparison addresses architecture, compatibility, hardware fit,
failure handling, and operations. It covers Distributed-JBOD at commit
`d9601b7` and upstream documentation reviewed on **21 September 2026**.
The systems compared are MinIO, Garage, Ceph, SeaweedFS, and RustFS; their
inclusion is not a measured popularity or performance ranking.

Upstream references describe community/open-source implementations unless
explicitly stated otherwise. Ceph references use its versioned Squid
documentation; other upstream pages may evolve after the review date.
Feature availability should be checked against the exact release chosen
for deployment. The assessments below are deductions from the documented
designs, not results from running these systems side by side.

**Performance comparisons are reserved for later experiments.** No
throughput, latency, resource-use, repair-speed, or scale measurements are
reported or inferred. The [performance section](#performance-comparisons-placeholder)
is a placeholder for that separate work.

## Contents

- [What Distributed-JBOD offers today](#what-distributed-jbod-offers-today)
- [Architectural overview](#architectural-overview)
- [MinIO](#minio)
- [Garage](#garage)
- [Ceph](#ceph)
- [SeaweedFS](#seaweedfs)
- [RustFS](#rustfs)
- [Capacity and failure domains](#capacity-and-failure-domains)
- [Recovery and operational responsibility](#recovery-and-operational-responsibility)
- [Choosing by requirements](#choosing-by-requirements)
- [Performance comparisons placeholder](#performance-comparisons-placeholder)

## What Distributed-JBOD offers today

Distributed-JBOD combines unequal device directories across machines,
choosing `k+m` active devices for each new object by available free bytes.
It stores systematic Reed-Solomon shards and a checksummed JSON metadata
record on every holder. Devices can be added individually, and records
preserve each object's actual placement and encoding scheme. An offline
tool can extract objects from surviving device trees without a running
cluster. These behaviours are implemented in the
[coordinator](../crates/djbod-node/src/coordinator.rs),
[device layer](../crates/djbod-core/src/device.rs), and
[recovery tool](../crates/djbod-recover/src/main.rs).

Its narrower interface and stricter availability rules matter as much as
those features. At this revision:

- Applications use a native protocol, CLI, Rust library, or Python
  binding. There is no S3 endpoint, mounted filesystem, range-read API,
  multipart upload, or user-visible object version history.
- Ordinary metadata lookups require all listed nodes to respond and all
  `k+m` current record copies to agree. A stopped node can block access to
  objects it does not hold.
- A normal GET checks data blocks and the complete body but does not
  reconstruct damaged data or check unused parity. Repair and full scrub
  are explicit administration operations.
- Shards are separated by device, not by host or rack. A machine failure
  can remove more than `m` shards of an object.
- TLS can authenticate nodes and clients, but clients have no per-user
  authorisation boundaries. The UI has no login; its browser connection
  needs separate protection when exposed beyond loopback.

These are current implementation limits, not promised future capabilities.
See [SPEC.md](../SPEC.md), particularly sections 7, 11, 13, 16, 19, and
22, and the project's [status](../README.md#status).

## Architectural overview

The entries summarise the documented data model, not an equivalent
resilience or performance configuration. Sources and qualifications follow
in the individual comparisons.

| System | Access and storage model | Placement and control |
| --- | --- | --- |
| Distributed-JBOD | Native object API; per-object erasure coding on write | Per-object device list; all-node lookup; equal node roles and ordered configuration updates |
| [MinIO](https://github.com/minio/minio/blob/master/docs/distributed/DESIGN.md) | S3 object storage; object erasure coding | Objects mapped to erasure sets within server pools; quorum within the relevant set |
| [Garage](https://garagehq.deuxfleurs.fr/documentation/reference-manual/features/) | S3 subset; replicated blocks | Capacity/zone-aware layout; no central request-ordering leader |
| [Ceph](https://docs.ceph.com/en/squid/architecture/) | RADOS with replicated or erasure-coded pools; object, block, and file services | CRUSH placement; OSDs, monitor quorum, managers, and interface-specific services |
| [SeaweedFS](https://github.com/seaweedfs/seaweedfs/wiki/Components) | Volume-based blobs; filer and S3/file interfaces | Masters track volumes; volume servers store data; filer maintains namespace metadata |
| [RustFS](https://github.com/rustfs/rustfs/blob/main/ARCHITECTURE.md) | S3 object storage implemented in Rust; erasure coding | Storage engine organised around pools and erasure sets, with separate API and administration layers |

## MinIO

MinIO erasure-codes objects within sets of drives. Its distributed design
uses object-to-set mapping, read/write quorum within the object's set,
healing, and expansion through additional server pools. It therefore
differs from Distributed-JBOD's choice of individual devices for each
object and its all-node metadata broadcast. A failed drive does not imply
every request must stop if the relevant MinIO quorum remains available.
That is still a topology- and quorum-dependent property, not unlimited
outage tolerance. [Distributed server design](https://github.com/minio/minio/blob/master/docs/distributed/DESIGN.md).

MinIO's erasure-code documentation recommends drives of approximately
equal size and documents HighwayHash protection against bitrot. It would
be misleading to describe it as unable to use commodity hardware or as
having no corruption protection. The relevant distinction for a pile of
unequal disks is the erasure-set/pool layout and its sizing constraints,
compared with Distributed-JBOD's per-object placement across eligible
devices. [Erasure-code overview](https://github.com/minio/minio/blob/master/docs/erasure/README.md).

There is also a concrete maintenance distinction at the review date:
the upstream `minio/minio` repository is archived and its README says it
is no longer maintained. It describes the community edition as source-only
under AGPLv3 and points to AIStor offerings separately. Those offerings
must not be treated as the same maintained, open-source distributed
edition. A deployment evaluation needs an explicit answer for who supplies
future fixes. [MinIO repository and maintenance notice](https://github.com/minio/minio).

**Assessment:** MinIO's S3 interface and quorum/healing model address
requirements that the current Distributed-JBOD implementation does not.
Distributed-JBOD offers a different expansion model and inspectable recovery
format, at the cost of its native-only interface and stronger dependence
on all members answering. For a new MinIO deployment, the upstream
maintenance status is a separate decision from the storage architecture.

## Garage

Garage is a close alternative for heterogeneous machines and
geographically distributed storage. Its layout accounts for capacity and
zones, and it can rebalance after membership changes. It replicates blocks
rather than erasure-coding them; three replicas are recommended, but the
replica count is configurable. It also supports deduplication and optional
compression, so a three-copy policy is not an exact prediction of actual
filesystem usage. [Garage features](https://garagehq.deuxfleurs.fr/documentation/reference-manual/features/),
[layout management](https://garagehq.deuxfleurs.fr/documentation/operations/layout/).

Garage's default `consistent` mode with three replicas uses read and write
quorums of two. Other modes deliberately change those guarantees. Reads
consult responsible nodes for metadata and obtain the necessary blocks,
rather than requiring every cluster member to answer. This provides a
different availability trade-off from Distributed-JBOD even though neither
uses a permanent request-ordering master. [Replication and consistency configuration](https://garagehq.deuxfleurs.fr/documentation/reference-manual/configuration/),
[request routing](https://garagehq.deuxfleurs.fr/documentation/design/internals/).

Garage has documented block-hash scrubbing and restoration of corrupt
blocks from another replica, including scheduled scrubs. A claim that it
lacks bitrot detection or repair is incorrect. This comparison does not
assume that every system performs identical checks on every read.
[Durability and repairs](https://garagehq.deuxfleurs.fr/documentation/operations/durability-repairs/).

**Assessment:** Garage fits applications needing an implemented S3 API,
zone-aware replication, and operation within replica quorums on mixed
machines. Distributed-JBOD offers configurable erasure-code payload
overhead and a different offline format, while foregoing those availability
and interface features. Check Garage's [S3 compatibility matrix](https://garagehq.deuxfleurs.fr/documentation/reference-manual/s3-compatibility/)
for the operations the application actually uses; S3 compatibility is not
a claim to implement all of AWS S3.

## Ceph

Ceph combines the RADOS object layer with RBD block storage, CephFS file
storage, and the RGW object gateway. Its base cluster uses monitors, OSDs,
and managers. MDS daemons are needed for CephFS, not for an RGW-only
object deployment. Monitors agree on cluster maps, while clients and OSDs
use CRUSH to locate data instead of a whole-cluster object lookup.
[Ceph architecture](https://docs.ceph.com/en/squid/architecture/),
[Ceph Object Gateway](https://docs.ceph.com/en/squid/radosgw/).

Ceph supports unequal device capacities through CRUSH weights and can
express device, host, and rack placement boundaries. It offers replicated
and erasure-coded pools; pool policy and `min_size` affect degraded
operation. An erasure-code profile is a pool-level decision, and changing
coding geometry requires a new pool and migration rather than just
changing that pool in place. [CRUSH maps](https://docs.ceph.com/en/squid/rados/operations/crush-map/),
[erasure-coded pools](https://docs.ceph.com/en/squid/rados/operations/erasure-code/).

**Assessment:** Ceph is relevant when topology-aware placement, multiple
storage interfaces, and managed recovery are requirements. It is not
excluded merely because disks differ in size. Operating its daemon roles,
maps, placement groups, and pool policies introduces more concepts than
Distributed-JBOD's node/document/device model. That is an architectural
operations trade-off, not a measured RAM requirement or a claim that Ceph
cannot run on a small cluster. Distributed-JBOD's smaller scope can suit
an operator who accepts explicit repair, scan-based metadata operations,
and device-only fault separation; it does not reproduce Ceph's service
or failure-management model.

## SeaweedFS

SeaweedFS separates volume storage from namespace services. Masters track
volume locations, volume servers store blobs, and the filer adds named
files/directories with a metadata store. The S3 interface builds on that
stack. An embedded metadata backend and combined processes are possible;
an external database server is not mandatory for every installation.
Production availability still requires planning the master, filer
metadata, and volume layers. [Components](https://github.com/seaweedfs/seaweedfs/wiki/Components),
[deployment and architecture overview](https://github.com/seaweedfs/seaweedfs).

Its usual protection model replicates active volumes, then optionally
erasure-codes warm volumes. The community default EC geometry is `10+4`.
New writes go to ordinary volumes; EC repair operates on volume shards,
and reads can reconstruct missing pieces. This differs from
Distributed-JBOD's per-object encoding at PUT time and explicit repair
before a damaged normal GET can succeed. The hot-data replica policy and
the EC policy must both be specified in a comparison.
[Erasure coding for warm storage](https://github.com/seaweedfs/seaweedfs/wiki/Erasure-Coding-for-warm-storage).

SeaweedFS packs blobs into volume files, which changes inode and metadata
costs compared with a file and JSON record per object shard. That is a
format distinction, not a demonstrated small-object speed advantage here.
Its EC tooling also documents an optional checksum sidecar and checksum
scrubs, so integrity comparisons must name the enabled mechanism.
[Storage layout](https://github.com/seaweedfs/seaweedfs),
[EC bitrot detection](https://github.com/seaweedfs/seaweedfs/wiki/EC-Bitrot-Detection).

**Assessment:** SeaweedFS is relevant when an application needs its
S3/file interfaces or volume-based storage and lifecycle management.
Distributed-JBOD keeps one storage-node role and a uniform per-object
coding/recovery path. SeaweedFS introduces more service and metadata
choices, while offering capabilities beyond the native object interface.

## RustFS

RustFS is a Rust implementation of S3-compatible object storage. Its
architecture includes S3 and administration APIs, IAM/STS, a console,
inter-node RPC, and an erasure-coded storage engine. Sharing a language
with Distributed-JBOD does not imply sharing its disk format or placement
model, and does not establish relative performance.
[RustFS architecture](https://github.com/rustfs/rustfs/blob/main/ARCHITECTURE.md).

RustFS persists pool and erasure-set topology. Existing pool endpoints
and set width cannot simply be expanded in place; expansion adds a pool,
and moving old objects is a separate rebalance/decommission operation.
Objects hash to sets, with geometry stored in object metadata. Reads use
metadata quorum, verify integrity, and decode from surviving shards;
successful degraded reads can enqueue repair. This is a different
availability and growth model from Distributed-JBOD's all-node lookup,
individual-device placement, and fail-stop GET.
[Cluster lifecycle operations](https://docs.rustfs.com/en/operations/cluster-lifecycle).

The project is Apache-2.0 licensed, and its README distinguishes shipped
features from previews and links a compatibility matrix. Evaluate those
release-specific boundaries rather than assuming that every MinIO or AWS
feature is interchangeable, or that MinIO's on-disk data can be adopted
by a normal RustFS build. [RustFS feature status](https://github.com/rustfs/rustfs),
[licence](https://github.com/rustfs/rustfs/blob/main/LICENSE).

**Assessment:** RustFS belongs in an evaluation seeking a Rust-based S3
service with erasure coding and access controls. Distributed-JBOD instead
offers arbitrary eligible device selection and its own offline extraction
tool. Neither the implementation language nor project marketing resolves
which is faster, more economical, or more reliable in a particular
deployment; those conclusions need evidence beyond this feature comparison.

## Capacity and failure domains

For an uncompressed payload of size `S`, `r` complete replicas contain
approximately `r*S` payload bytes; an erasure code with `k` data and `m`
parity shards contains approximately `S*(k+m)/k`. These are arithmetic
coding ratios, not benchmark results or promises about a filesystem.

| Example policy | Stored/original payload ratio | Loss budget for the encoded data |
| --- | --- | --- |
| Three complete replicas | `3` | Two complete copies, assuming the remaining copy is intact |
| `3+1` erasure code | `4/3` | One shard per stripe |
| `4+2` erasure code | `6/4` | Two shards per stripe |
| `10+4` erasure code | `14/10` | Four shards per stripe |

The ratio is stored payload divided by original payload; a lower ratio
means fewer redundant payload bytes. These examples are not equivalent
fault-tolerance configurations. Three replicas distributed across three
zones address a different failure event from four coded shards on four
disks in one host. Metadata, compression, deduplication, filesystem
allocation, small objects, free-space reserves, and temporary replacement
copies also affect actual storage usage.

Distributed-JBOD additionally needs `k+m` individually eligible devices
for each write; extra free space on one disk cannot hold multiple shards
of that object. A fixed-width scheme can leave free space unusable in a
very uneven pool. MinIO and RustFS impose set/pool geometry, Garage
imposes replica placement constraints, and Ceph and SeaweedFS have their
own configured topology rules. None should be evaluated using only the
sum of advertised disk sizes.

## Recovery and operational responsibility

Distributed-JBOD's offline extraction is a useful and specific property:
given surviving device directories, a usable record, and enough intact
shards, `djbod-recover` can extract an object without any service running.
That is not a claim that the alternatives have no recovery tools. Ceph,
for example, supplies `ceph-objectstore-tool` to inspect and extract data
from a stopped OSD; recovering application-level objects can require
additional reconstruction of their service metadata.
[Distributed-JBOD recovery implementation](../crates/djbod-recover/src/main.rs),
[Ceph object-store tool](https://docs.ceph.com/en/squid/man/8/ceph-objectstore-tool/).

The preceding sections describe MinIO healing, Garage replica repair,
Ceph recovery, SeaweedFS volume reconstruction, and RustFS healing. Their
automation, fault boundaries, and required metadata differ. An evaluation
should check both service continuity during a fault and extraction after
the service can no longer be started. Recoverable bytes, readable online
objects, and an automatically healthy cluster are separate outcomes.

Distributed-JBOD moves responsibility to the operator for scheduling
scrubs, retaining scrub results, deciding permanent node loss, maintaining
certificates, and initiating repair or migration. It currently lacks the
per-user authorisation required for mutually untrusted tenants. Native
TLS authenticates a client but grants it broad authority; the browser UI
adds another access boundary. Compare an application's required operations
and security policy, not merely whether a product lists TLS or S3.

Finally, all six need a backup strategy for unwanted deletes, overwrites,
and losses beyond their protection policy. Erasure coding, replication,
and local recovery tools are not independent backups.

## Choosing by requirements

These are architectural assessments, not benchmark winners.

| Requirement | Implication for the evaluation |
| --- | --- |
| Existing S3 application with no custom client work | Evaluate the five alternatives' compatibility for the exact operations used. Distributed-JBOD has no S3 endpoint at this revision. |
| Unequal disks added one at a time | Distributed-JBOD directly models individual device additions. Garage, Ceph, and SeaweedFS also merit evaluation for heterogeneous capacity; MinIO/RustFS require attention to pool/set layout. |
| A node may be offline while ordinary object operations continue | Compare replica/quorum and topology rules. Distributed-JBOD's all-node lookup is a material limitation. |
| Explicit host/rack/zone separation | Use a system with suitable placement controls and verify the configured topology. Distributed-JBOD currently promises only distinct devices. |
| Block and filesystem services as well as objects | Ceph offers all three; SeaweedFS offers file interfaces. Distributed-JBOD's native object service is narrower. |
| Inspectable per-object records and standalone extraction | Distributed-JBOD provides this workflow; assess each alternative's recovery procedures against the same disaster scenario. |
| Rust plus S3 and IAM | RustFS directly targets that combination. Rust alone is not a performance or reliability comparison. |
| A new MinIO community deployment | Account explicitly for the archived upstream repository and ongoing maintenance source. |

## Performance comparisons placeholder

**Reserved for future experiments, performed separately at a later time.**
No performance experiment has been run for this comparison. All result
cells remain unmeasured; no upstream marketing or unrelated benchmark
numbers stand in for measurements.

| System | PUT/GET throughput | Latency distribution | CPU, memory, and network use | Scrub/repair and recovery impact |
| --- | --- | --- | --- | --- |
| Distributed-JBOD | Pending | Pending | Pending | Pending |
| MinIO | Pending | Pending | Pending | Pending |
| Garage | Pending | Pending | Pending | Pending |
| Ceph | Pending | Pending | Pending | Pending |
| SeaweedFS | Pending | Pending | Pending | Pending |
| RustFS | Pending | Pending | Pending | Pending |

The later report should record exact software revisions, hardware and
network, filesystems, object-size distribution, concurrency, cache state,
encryption/compression settings, redundancy policy, failure domains, and
acknowledgement guarantees. It should distinguish healthy operation,
degraded service, and active repair, including refused operations as
outcomes. Distributed-JBOD needs a native client workload; the others'
S3 interfaces and any gateways must be accounted for rather than assuming
the same load generator exercises identical paths.

Report repeated measurements and their variation, plus scripts and raw
results sufficient to reproduce them. Until that work exists, this
document makes no relative speed, resource-efficiency, or scale claim.
