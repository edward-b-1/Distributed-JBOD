# Versions and revisions

Every metadata record carries two counters. They answer different
questions and are easy to confuse.

## Version: which body is this?

A **version** identifies the content of an object: the bytes, their size,
their checksum, and the shards encoded from them. Every PUT generates a
new version id, a ULID (SPEC 9.2.3), even a PUT that replaces an existing
key. The cluster keeps at most one version per key: a replacing PUT
writes the new version fully, then deletes the old one (9.2.4). If that
delete never reached a device, the key briefly has two versions; reads
always take the newest by ULID, and the scrub reports the older one as a
stale version for repair to remove.

Two records with different version ids describe two different objects
that share a key. Nothing about one says anything about the other.

## Revision: where is this body right now?

Within one version, the **revision** counts placements (SPEC 18.8.1). It
is 0 when the version is written and rises by one each time a shard is
moved: by a drain, by a repair that relocates a lost shard, or by
`move-shard`. The body is untouched by a move. Only the record's map of
which device holds which shard changes, and the new record is written at
the next revision to every device that holds a shard of the version.

Two copies of one version at different revisions therefore describe the
same bytes with a different map of where the shards sit. The revision
never changes the version id.

## Why reads care

A re-placement writes the shard to its new device first, then the
new-revision record to each device in turn. If it stops halfway, some
devices hold revision 1 and some still hold revision 0 of the same
version. The highest revision is the truth to complete towards, because
its shard is already in place. The revision 0 copies describe the same
body (same version, size, checksum, scheme), so they vouch that the
revision 1 record is a genuine record of this version and not a
corruption. That is what "counting lower-revision copies of the same
body" means in SPEC 9.4.4 and 18.4.2.

A read trusts the highest revision, uses its shard map, and reports the
lagging device with a `stale` fault carrying the revision it is stuck at.
Repair finishes the move forwards, never backwards, and rewrites the
record at the current revision on that device.

## In a report

- A `stale` record-copy fault (`djbod get`, `djbod head`, the Python
  `DegradedRead` warning) is always the same version at an older
  placement, never an older object.
- A stale *version* found by the scrub (`StaleCopy`, an older ULID under
  the same key) is a different object whose deletion did not finish. It
  is handled by delete and repair, never by reads, which pick the newest
  version present.

References: SPEC 9.2 (version identifiers), 9.4.4 (the read rule),
18.4.2 (missing record copies), 18.8.1 (record revision), 18.8.2
(re-placement).
