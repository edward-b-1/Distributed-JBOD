# Proposal: configurable responses to data corruption

Status: discussion proposal, 22 September 2026. Records user feedback and
design choices; it does not adopt a new default, change SPEC.md, or implement
configuration options. Names below describe possible settings, not existing
CLI flags or environment variables.

The implementation observations were checked against
[`d9601b7`](https://github.com/edward-b-1/Distributed-JBOD/commit/d9601b7cc961ca8750fa1f102621338ae1409370).
This document belongs alongside the other proposals because the choices span
reads, writes, repair, configuration, and reporting, beyond scrub scheduling.

## 1. Motivation and the maintainer's stated preference

User feedback identifies two priorities:

- Keep reading and writing wherever valid data and sufficient healthy storage
  remain available, including reconstructing damaged data from Reed-Solomon
  shards during a read.
- Make damage stop normal activity early and visibly, so an administrator
  investigates before the system continues using suspect storage.

The maintainer prefers the second for personal use. Read reconstruction has
extra I/O, network, and coding costs when recovery is needed, and read repair
adds writes too. **Reconstruction on read must be opt-in**, even if most users
eventually choose it. Returning an error remains the proposed read default.
The cost needs measuring; this proposal does not assume a fixed slowdown or
that healthy reads must fetch parity.

The current combination of failing the damaged read while allowing further
writes to the same device is simple, but the feedback questions whether that
is the right overall default. **The default restriction on writes remains
undecided:** quarantine the implicated device, or stop writes throughout the
cluster. Continuing all writes remains an option to describe explicitly.

These priorities need independent settings, rather than a single "liberal"
or "strict" switch. Successful reconstruction must still produce a visible
damage report; availability and reporting are separate decisions.

## 2. What the implementation does today

| Area | Current behavior and source |
| --- | --- |
| Ordinary GET | Reads the data shards, verifies block checksums, and fails on detected damage. It does not fetch parity to recover a failed read. It also checks the whole-object checksum at the end. See [SPEC 11](../../SPEC.md#11-read-path-get) and [`get_object`](../../crates/djbod-node/src/coordinator.rs#L728). |
| Scope of a read failure | Fails that request. It does not install a device quarantine or a persistent cluster write lock. Damage confined to an unread parity shard need not be discovered by an ordinary GET. |
| Subsequent PUT | Chooses active devices with enough free space. A previously observed bad shard does not make that device ineligible. Other existing errors can still fail the PUT; "writes continue" is not a guarantee of success. See [SPEC 10](../../SPEC.md#10-write-path-put), [`place`](../../crates/djbod-node/src/coordinator.rs#L1035), and the holder's active-device check in [`put_shard`](../../crates/djbod-node/src/local_ops.rs#L512). |
| Verification during PUT | Checks incoming body and shard-block checksums, sequence, length, and shard geometry; reports write/flush errors. It writes and fsyncs the shard, renames it, and fsyncs the directory. It does **not** read back the completed shard's payload to verify storage. See section 6 below. |
| Repair | Explicit administrative repair can reconstruct damaged shards. It verifies the reconstructed object's checksum before its rewrite pass. For a device still present, repair normally rewrites there; relocation handles removed holders. Quarantining a device would therefore require changes to repair placement as well as PUT placement. See [`repair_object`](../../crates/djbod-node/src/coordinator.rs#L1681) and [SPEC 18](../../SPEC.md#18-membership-add-drain-remove-repair-rebalance-re-encode). |
| Scrub | Local scans check shard structure, every data/parity block, and records; the cluster scrub adds cross-node checks. `djbod scrub --repair` can then repair affected keys. Scheduling is external, such as cron or a systemd timer. See [SPEC 20.1](../../SPEC.md#201-scrubber). |
| Reporting | Findings stream to the invoking client, and CLI errors/exit status provide immediate signals, but there is no persistent cluster scrub history. A completed scan is distinct from a clean result. See section 8. |
| Remembering damage | The [damage-mark proposal](damage-marks.md) and open [PR #62](https://github.com/edward-b-1/Distributed-JBOD/pull/62) propose persistent observations. They are not implemented on this baseline, and deliberately do not change read/write behavior. |

Reconstruction alone would not remove the current requirement for agreeing
metadata, responding nodes during lookup, or matching cluster-document
versions. Relaxing those requirements needs its own consistency design;
recovering a checksummed shard is not permission to choose an ambiguous
object version. See [SPEC 16](../../SPEC.md#16-failure-semantics).

## 3. Independent choices

| Choice | Alternatives to retain in the design |
| --- | --- |
| Scheduled detection | No scheduled scrub; or a scheduled scrub with an explicit interval, scope, rate limit, and owner. Manual scrubbing remains possible in either case. |
| Scheduled repair | Report findings only; or run repair after detection. Enabling a schedule must not silently enable repair. |
| Write response to detected damage | Continue using all otherwise eligible devices; quarantine the implicated device from writes; or block ordinary writes throughout the cluster until the incident is addressed. |
| Read response to damaged data | Return an error; reconstruct and return verified data without repairing storage; or reconstruct, return verified data, and repair storage. Both reconstruction choices are opt-in. |
| Verification of new writes | Existing checks without storage read-back; synchronous read-back before reporting success; or deferred verification with separately reported completion. Defaults and verification scope remain open. |

Scheduling changes *when* damage is discovered, not whether the selected
response applies. A bad shard detected by a normal read must trigger the same
configured response even when no scrub schedule exists. Conversely, a scrub
may trigger a restriction before any user reads the object.

### 3.1 What does "lock the whole system" mean?

The feedback includes both "do not write to any disk" and "lock the whole
system." Preserve both possibilities:

- **Block cluster writes:** healthy reads, and reconstructed reads if enabled,
  can continue. Stop ordinary data mutations throughout the cluster.
- **Block all ordinary data access:** stop reads as well as writes until an
  administrator intervenes. This is a stronger alternative, and takes
  precedence over the read-reconstruction setting while active.

Likewise, device quarantine needs an explicit scope: prohibit writes only,
or prohibit ordinary reads from that device too. The write-only form can
still use checksum-verified intact blocks from it as reconstruction sources.
The stronger form may reduce the number of available recovery sources.

Status, diagnosis, and the administrative recovery path must remain usable.
Define whether deletion, overwrites, moves, draining, re-encoding, metadata
updates, and automatic repairs count as blocked mutations. They must not
become accidental ways to bypass a restriction on writes to a device.

## 4. Detection and attribution

All checking paths should produce a common incident description: detection
time, source operation, reporting node, holder and device identities, object
key and version, shard/stripe when known, error kind, and supporting detail.

| Detection source | What it can discover and what needs deciding |
| --- | --- |
| Normal read | Corrupt blocks or shard structures it actually reads, I/O failures, and metadata/whole-object validation failures. A read does not establish the health of unexamined parity. |
| Online or offline scrub | Previously unread damage, including parity and records. An offline discovery must be reconciled with incident/restriction state before that node resumes normal service. |
| PUT | Invalid incoming data, metadata problems encountered by lookup, and failures of storage operations. Optional read-back would add detection of newly stored bytes that cannot be read or verified. |
| Repair, move, drain, or re-encode | Damage found while checking existing data. These findings need the same reporting and policy evaluation, including damage on a proposed repair destination. |

A corrupt shard is evidence about data, not proof that the physical device
caused it. A conservative policy may quarantine the device on the first
confirmed bad shard, but the UI should say why it was quarantined rather
than presenting a hardware diagnosis as established fact.

The trigger table needs explicit treatment of checksum mismatches, malformed
or missing shard files, device I/O errors, bad records, and inconsistent
metadata. A transit checksum failure or an unreachable node must not
automatically be blamed on a disk. A whole-object checksum mismatch may not
identify a single bad holder. Decide how an unlocalized incident affects
device quarantine versus a cluster restriction. Temporary files, concurrent
deletes, and stale placements are not automatically corruption incidents.

## 5. Read/write combinations and repair

The matrix below uses *write-only* restrictions. A lock on all ordinary data
access overrides the read column entirely. Every cell must still report and
remember detected damage.

| Response to damage | Read returns an error | Read reconstructs, without repair | Read reconstructs and repairs |
| --- | --- | --- | --- |
| Continue all writes | Current combination. The affected read fails; the device can receive later writes. | The requested availability-oriented option: verified reads and otherwise valid writes continue, including writes to the implicated device. | Adds repair writes. Decide whether rewriting the suspect device is allowed or relocation is preferred. |
| Quarantine implicated device | The affected read fails; new writes can use other eligible devices. | Recover the read from sufficient verified shards; keep the device excluded as a destination. | Repair must use an allowed destination, usually a healthy replacement, or wait for an explicit maintenance decision. |
| Block cluster writes | The affected read fails; ordinary writes stop everywhere. | Verified reads can continue while writes remain blocked. | Read reconstruction can proceed, but repair writes require a defined maintenance exception or must be deferred. |

Excluding a device can leave fewer than `k+m` eligible destinations; ordinary
PUT then fails. Continuing reads requires at least `k` usable blocks for
every stripe being reconstructed and trustworthy metadata. No mode can
promise recovery with insufficient redundancy, including unrecoverable
damage when `m = 0`.

### 5.1 Reconstruction without repair

Keep the healthy read path inexpensive. On a recoverable data-shard failure,
fetch sufficient alternate shards, check every input block, reconstruct, and
validate the result against the existing whole-object checksum. Determine
how to handle a shard that fails partway through its stream without mixing
versions or delivering a stripe twice.

The existing streaming contract still matters: whole-object validation is
only complete at the end, and a terminal checksum error invalidates the
body already delivered. Clients and HTTP/UI adapters must propagate that
outcome. Never claim success merely because the decoder returned bytes.

Returning reconstructed data does **not** repair the stored shard or clear
its damage observation. Another read may pay the recovery cost again.

### 5.2 Reconstruction with repair

Record both requested variants: reconstruct only, and reconstruct with new
blocks written to repair storage. For the latter, decide whether repair is
synchronous with the read or queued after returning data. If verified data
is available but repair fails, decide whether the read fails too or succeeds
with a separately visible pending/failed repair. An asynchronous queue needs
durable jobs, retry limits, and deduplication.

Repair must use the exact object version and placement revision that was
verified, respect concurrent deletion/replacement, and avoid competing
repairs of the same shard. Reuse the existing verified reconstruction and
atomic shard-replacement mechanisms; do not introduce unprotected in-place
patching. Replacement of an entire shard file versus block-level repair is
an implementation choice that still needs evaluation.

A restriction cannot prevent every route to its own recovery. Specify which
administrative repair writes are permitted during a cluster lock and how
they are authorized. A read-triggered or scheduled repair does not acquire
that exception implicitly. Likewise, successful repair on another device
does not establish that the original device is safe to accept new writes.

## 6. Side question: can writes discover bad shards?

**Yes, writes can detect errors during their own work, but there is no
post-write payload read-back today and they do not scan existing shards.**

The coordinator checks incoming body checksums in
[`stream_body_to_holders`](../../crates/djbod-node/src/coordinator.rs#L1297).
The holder recomputes each incoming block's checksum before writing in
[`ShardFileWriter::append_block`](../../crates/djbod-core/src/shardfile.rs#L415).
Its `finish` writes the footer/trailer and calls `sync_all`; then
[`ShardWrite::finish`](../../crates/djbod-core/src/device.rs#L632) renames the
file and fsyncs its directory. These checks can reject invalid input and
surface I/O/flush failures. `prepare_put` also validates metadata encountered
by its existing-key lookup. None of this reopens and verifies the newly
stored shard payload.

Possible verification modes:

| Mode | Success contract and costs |
| --- | --- |
| Current checks | Incoming checksums, structure/geometry checks, and successful durable-write operations. No additional verification read. |
| Synchronous read-back | After flushing, read and verify the new shard's structure and all blocks before its write is acknowledged as verified. Decide whether all `k+m` shards and their metadata copies are covered, and whether a separate reconstructed whole-object comparison is required. Adds read traffic and latency. |
| Deferred verification | A background check verifies new writes later. PUT success must explicitly mean verification is still pending; a later failure creates an incident and applies the configured response. Needs persistent tracking and reporting. |

Synchronous verification must fail the PUT if its required verification
fails, with defined cleanup or replacement behavior before committing the
object and retiring its previous version. Repair/rebuild writes need an
explicit verification policy too. A checksum mismatch before storage is
not evidence of a bad disk; a read-back mismatch is a different event.

Read-back can be satisfied by caches, so its guarantee needs precise wording
and a decision about cache/flush semantics. It is not proof of indefinite
retention and cannot replace periodic scrubbing of older data. This proposal
does not choose to impose its cost by default.

## 7. Configuration and persistent restrictions

Recommend storing behavioral policy in the versioned cluster document,
using the existing change/adoption procedure in [SPEC 6.2](../../SPEC.md#62-cluster-wide-versioned-document-held-by-every-node).
Every coordinator and holder needs the same effective policy. A per-process
environment override could make the outcome depend on the contacted node.
Environment variables may be useful for scheduler deployment or initial
configuration, but must not silently override an adopted cluster policy.
Client-side recovery overrides, if ever offered, must not bypass the
cluster's device/write restrictions.

Keep three things distinct:

1. **Policy:** the administrator's agreed choice of what to do.
2. **Evidence:** observations and history of what was found.
3. **Enforcement state:** which devices or operations are currently blocked,
   why, and under which policy version.

The damage ledger in PR #62 is deliberately a best-effort hint: loss or a
failed update must not change an operation's correctness. It cannot simply
become the sole authoritative write gate. Reconcile that design explicitly
if enforcement is added. A repaired shard's mark can be cleared while a
device quarantine or an unacknowledged incident remains active.

Enforcement needs decisions before implementation:

- Gate both coordinator placement and holder-side mutations. A holder must
  refuse a quarantined destination even when the coordinator has stale
  information. Cover shard, record, deletion, and maintenance paths according
  to the chosen scope, not just new-object placement.
- Define the instant a restriction takes effect and the outcome for in-flight
  PUTs: abort, finish under an existing grant, or some other explicit rule.
  Recheck at the appropriate commit boundary. Avoid implying an instantaneous
  global stop from an asynchronous damage notification.
- A strict cluster-wide write stop needs propagation and ordering across
  nodes, including simultaneous detections and partitions. The existing
  cluster-document update requires reachable, agreeing nodes; it cannot be
  assumed to complete when the incident includes an unavailable node.
  Specify the behavior while the restriction is unconfirmed and how stale
  nodes are prevented from acknowledging new writes.
- Persist restrictions across restarts and rejoin. Define behavior when
  their storage cannot be written or read. Do not silently reopen writes
  because a best-effort mark or process memory was lost.
- Define release: repair and successful verification, explicit administrative
  clearance, replacement/removal of the device, or a combination. No expiry
  or mere acknowledgment should silently assert that the hardware is healthy.
  Clearing one incident must not release restrictions held by another.
- Show effective policy, policy version, restrictions, and reasons in status
  and the UI. Mixed builds must follow the existing unknown-field refusal
  rule. Decide new-cluster defaults separately from migration of old clusters;
  do not silently turn on reconstruction or stronger write restrictions.

The automatic restriction state may need a separate mechanism from ordinary
configuration updates; this proposal leaves that protocol design open.

## 8. Scheduling and reporting

[Issue #137](https://github.com/edward-b-1/Distributed-JBOD/issues/137) already
covers automatic scrub/repair and reporting in the UI. A Python utility is
one possible scheduler, but a cron job or systemd timer invoking the existing
scrubber is also valid. Scheduling should orchestrate the existing checking
engine rather than introduce another integrity implementation.

Periodic checks address damage in data that may otherwise remain unread for
a long time. The motivation includes time in storage as well as write wear;
the policy need not assume a particular Poisson rate or failure model.
Write verification checks newly stored data, while scheduled scrubbing
revisits older data and parity as well.

Record interval/time zone, scope, rate/concurrency limits, missed-run and
overlap behavior, scheduler ownership/failover, and whether repair is
enabled. Multiple timers must not accidentally launch competing cluster
scrubs. A schedule being configured is not evidence that a run completed.

Persist enough information for an unattended job to be useful: run identity,
start/end time, last successful scan, scope and completed coverage, findings,
repairs attempted/succeeded/failed, unresolved incidents, restrictions
triggered, and execution failures or cancellation. Distinguish:

- Clean scan.
- Completed scan with damage, with any repairs and unresolved findings shown.
- Failed, interrupted, or incomplete scan.
- Scheduled run missed or overdue.

The UI is the requested primary destination: an obvious health/incident
indicator and a scrub-run view with device/object context and actions. CLI
status and structured API results can expose the same persisted information
for an external scheduler; logs are supplementary. Notification destinations
beyond the UI remain open. Neither a successful reconstructed read nor a
subsequent repair should erase the historical fact that corruption occurred.

### 8.1 Existing reporting caveat for automation

The current scrub already emits findings and terminal errors, so the gap is
not a complete absence of reporting. The missing part is durable, discoverable
results after the invoking client exits.

There is also a concrete exit-status caveat in the baseline's
[`Command::Scrub`](../../crates/djbod-cli/src/main.rs#L767): human-readable
output counts findings and exits 2 when findings exist without `--repair`;
the `--json` branch emits each event and continues before incrementing that
counter. A JSON scan can therefore finish successfully with findings when
there is no terminal error. The
[`scrub` coordinator](../../crates/djbod-node/src/coordinator.rs#L3123) reports
failed nodes or failed repairs in its terminal status, not every finding.
An unattended runner must consume the findings as well as completion status.
Consistent machine-readable outcome/exit semantics belong in the scheduling
and reporting work; this documentation does not fix that behavior.

## 9. Related work and decisions still required

- [Issue #137: Automatic scrub and repair](https://github.com/edward-b-1/Distributed-JBOD/issues/137)
  owns scheduling and unattended result reporting. Link this proposal there
  without expanding that issue into every policy decision.
- [Damage marks](damage-marks.md) and [PR #62](https://github.com/edward-b-1/Distributed-JBOD/pull/62)
  cover persistent evidence, per-device counts, and last-local-scrub results.
  Their initial behavior-neutral scope can remain useful independently.
- [Issue #13: Clearer errors for structurally corrupt shard files on read](https://github.com/edward-b-1/Distributed-JBOD/issues/13)
  covers accurate classification and actionable context. It remains relevant
  for error-returning reads and for incidents recorded by recovery modes.
- [PR #69: Logs in the UI](https://github.com/edward-b-1/Distributed-JBOD/pull/69)
  can aid diagnosis. Recent logs alone are not persistent incident or scrub
  history.
- [Issue #135: Bitrotter process](https://github.com/edward-b-1/Distributed-JBOD/issues/135)
  can support repeatable corruption scenarios for validating these choices.
- SPEC 16.5 already defers inline reconstruction. Adoption would also touch
  configuration (6), writes (10), reads (11), failure semantics (16), repair
  and placement (18), protocol outcomes (19), and operations/reporting (20).

Before implementation, settle the write default and exact lock scope; error
classes and attribution; enforcement/propagation guarantees; administrative
recovery and release; synchronous versus queued read repair and its failure
contract; write-verification coverage/default; schedule ownership; and
retention/notification details. Preserve the explicit opt-in for read
reconstruction regardless of the other decisions.

Validation should cover data and parity corruption; corrupt structures and
records; recoverable versus insufficient shards; detections from reads,
scrubs, verification, and maintenance; quarantine with too few replacement
devices; repair during a write lock; concurrent writes/detections/repairs;
restart, offline scrub and rejoin; disagreement or failed propagation; and
durable UI/API outcomes after a scheduler or client exits. Measure healthy
reads, reconstructed reads, repair, and write read-back separately before
choosing performance-related defaults.
