# Proposal: remembering damaged shards

Status: adopted into SPEC.md as section 20.7 on 19 September 2026, with
the changes to 6.1, 11.4, 11.7, 14.2, 16.1, 18.4, 19.1.3, 20.1.2, 20.1.4,
and 22 that section 7 lists. The review that settled the open questions
added: `ClearDamage` after a clean read goes only to shards the lookup
showed marked; the node and the offline scrub lock the ledger for their
read-modify-write; repeated detections are coalesced; the cross-node
`ShardMissingOnHolder` finding marks the holder; marks carry the
reporting coordinator's node id; `ListDamage` is paged; per-device
counters replace a history; and the last local scrub's time and summary
live in the ledger (20.7.6). This document is kept for the alternatives
it weighed.

## 1. The problem

Every read in this system is fail-stop (SPEC 11.4, 16.1): a block that
fails its checksum stops the read, and the error names the device, shard,
and stripe (16.2). That is the right behaviour for the reader. It is a
poor one for everybody else, because nothing remembers what was found.

- The next reader of the same object trips over the same block and gets
  the same error. Between the two, nothing shows the object is damaged.
- An operator learns about damage from a client's error message or from
  the node's log, and from nothing in the store itself. `djbod head`,
  `djbod status`, and the web UI show a damaged object exactly as they
  show an intact one.
- The scrub (20.1) finds damage on a schedule and reports it to whoever
  ran it, then forgets it too. Two scrubs a week apart find the same
  block twice.
- Repair (18.3, 18.4) has to be told which object to repair. Nothing
  collects the candidates.

The web UI made this visible: a download of a damaged object ends in the
browser saying "download failed" and nothing else, because the failure
is known to the node and the UI server for an instant and then gone. The
UI now keeps its own in-memory note of recent read failures so the
object panel can say what happened. That is a stopgap: it lives in one
process, dies with it, and knows only about failures that went through
that process. It also reaches the page only by asking: a link download
runs outside the page, so the page polls the UI server for a minute
after Download is clicked, which catches damage early in an object at
once and damage late in a large or slow download late or not at all,
until the next periodic refresh.

## 2. Requirements

1. A shard found damaged by any operation that checks it stays marked
   until something checks it again and finds it intact, or rewrites it.
   The checking operations today are the read path (11.4), repair (18.4),
   move-shard's rebuild path (18.8.2), drain's skip (18.2.1), and both
   halves of the scrub (20.1.2).
2. The marks are visible wherever an object or a device is described:
   `HeadObject`, `Status`, `djbod head`, `djbod status`, the web UI, and
   a listing of every mark in the cluster for the operator who wants to
   repair everything at once.
3. The marks are hints. No operation may trust a mark for correctness.
   Reads still verify every block; repair still verifies everything it
   reads; a false or stale mark cannot lose data or serve wrong data. A
   mark may make an operation faster or better informed, never less
   careful.
4. Marking must never make a failing operation fail worse. If the mark
   cannot be written, the operation's own outcome is unchanged and the
   failure to mark is logged.
5. No new consistency problem. The cluster document procedure (6.2.6)
   exists because the document must agree everywhere. Marks must not need
   that.

## 3. Where the marks could live

### 3.1 In the metadata record

The record (9.4) is copied to every holder and carries its own checksum.
Adding a `damaged` list to it means rewriting k+m copies on every
detection, on every holder, through the record-writing path, while the
very failure being recorded may be that one holder cannot be written or
reached. Records that disagree are `RecordsInconsistent` (9.4.5, 16.1),
so a partial update turns a damaged shard into an unreadable object. The
placement revision (18.8.1) exists precisely to manage the one kind of
record change the system allows, and health is not placement. Rejected.

### 3.2 A file at the device root

`DISTRIBUTED-JBOD-DAMAGE.json` beside the device identity file (9.1),
listing the damaged shards on that device. It travels with the disk,
which is attractive, and `djbod-recover` (20.2) could read it. But the
device is the thing under suspicion: a failing disk is the one place a
mark about it may not be writable, and a device that is unreadable
altogether gets no marks at all. The scrub also finds records and shards
that are on the wrong device (`ShardMisplaced`, `RecordNotForThisDevice`),
which have no natural home in a per-device file. Possible as a secondary
copy; not as the store of record.

### 3.3 A ledger in the node's state directory

The state directory (6.1) already holds the cluster document and belongs
to the node, not to a disk. A single file there, `damage.json`, lists
every mark about shards on this node's devices. One writer, the node
itself. Written like the cluster document: temporary name, fsync, rename
(9.4.3). Survives the loss of any device. Lost with the node, which is
acceptable: a node that is gone has bigger problems than its marks, and
a scrub rebuilds them.

### 3.4 In the cluster document

Global, versioned, agreed everywhere, and changed by the procedure of
6.2.6 that needs every node to acknowledge. Damage is frequent, local,
and transient; the document is rare, global, and permanent. Rejected.

## 4. Recommendation: a per-node damage ledger

### 4.1 The ledger

`<state_dir>/damage.json`, a JSON document with one entry per damaged
shard file on this node's devices, keyed by device, key hash, version,
and shard index so that a repeated detection updates rather than
appends:

```json
{
  "format_version": 1,
  "marks": [
    {
      "device": "b92e3a9b-a187-45ad-a339-9b3eaa03f93a",
      "key_hash": "a7d77ebf...",
      "key": "Zed-x86_64.exe",
      "version": "01M2X2WM2VP309XHD8HAV3A110",
      "shard_index": 0,
      "kind": "block_checksum_mismatch",
      "stripes": [0],
      "detail": "1 damaged block(s) in stripe 0",
      "first_seen": "2026-09-19T14:52:10Z",
      "last_seen": "2026-09-19T15:03:44Z",
      "seen_by": "get_object",
      "count": 2
    }
  ]
}
```

`kind` takes the values the scrub already uses for a shard (20.1.2:
`shard_unreadable`, `shard_blocks_corrupt`, `shard_misplaced`,
`record_without_shard`, and so on) plus what the read path can tell
(`block_checksum_mismatch`). `key` is carried for the operator's sake and
is not authoritative; the record is. The file is bounded by the number
of damaged shards, which is small when things are well and is the
operator's problem when it is not; a node refuses to grow it past a
configurable limit (say 100 000 entries) and logs instead.

### 4.2 Who writes a mark

The holder of the shard writes marks about its own devices, and nobody
else. Two paths lead there:

- **The node found it itself.** The local scrub (20.1.2) runs on the
  holder and reads its own disks; every shard finding becomes a mark,
  and every shard it checks clean clears any mark on it.
- **The coordinator found it.** Blocks are verified by the coordinator
  (11.4), not by the holder that streams them, so the read path, repair,
  move-shard, and drain all learn about damage on another node's device.
  They tell the holder with a new node-to-node message, `MarkDamage`,
  carrying the fields above. It is fire-and-forget: sent after the
  operation's own outcome is decided, its failure logged and otherwise
  ignored (requirement 4). A holder that is unreachable is not damaged
  and gets no mark; `NodeUnreachable` stays what it is (16.1).

The node writes the file at most once per detection, after the
operation, so a read that trips on stripe 0 costs one small file rewrite
on one node.

### 4.3 Who clears a mark

- Repair rewrites the shard file (18.4) or relocates it (18.3): the
  coordinator sends `ClearDamage` to the old holder for that shard, and
  the new file starts unmarked.
- The local scrub verifies the shard file clean: cleared by the holder.
- The version is deleted (14): `DeleteVersion` clears the holder's marks
  for it.
- A read completes and the whole-object check passes (11.7): the
  coordinator clears marks on every shard it read, since it has just
  proved them intact. This is what makes a stale mark disappear on its
  own.

Nothing clears a mark merely because time has passed.

### 4.4 Who reads a mark

- `LocalLookup` (13, 15.2.2) already returns every record copy a node
  holds for a key hash. Each `LocatedRecord` gains an optional `damage`
  list with the marks for that record's shards on that device. The
  coordinator has this in hand for every `HeadObject`, so `HeadObject`
  gains `damage` at no extra round trip, and `djbod head` and the UI's
  object panel show it.
- `LocalStatus` gains a per-device `damaged_shards` count, so `Status`,
  `djbod status`, and the UI's device table show it.
- A new client operation `ListDamage`, paged like `ListKeys` (15.2.1),
  fans out a new `LocalDamage` to every node and merges: every mark in
  the cluster, for `djbod damage list` and a UI page. `djbod damage
  repair` runs `RepairObject` over every key listed, which is the
  collector the repair path lacks today (section 1).

### 4.5 What a mark is allowed to change

Nothing, at first. Reads, repair, and scrub behave exactly as now; the
marks are shown, not used. Two later uses are worth naming because they
shape the design:

- **Inline reconstruction on read** (16.5, deferred) would consult marks
  to pick which k shards to read, avoiding a known-bad one. The read
  still verifies what it gets, so a stale mark costs at most a
  reconstruction.
- **Scrub scheduling** could revisit marked shards first.

Neither is proposed here.

### 4.6 What this does not cover

- Damage to record copies (`RecordCorrupt`, `RecordsInconsistent`) and
  cluster-level findings (`StaleCopy`, `HolderUnavailable`) are about
  the object, not a shard file on a device. They belong in the same
  ledger under their own kinds, but the first version can leave them to
  the scrub report as today.
- `djbod-recover` (20.2) does not read the ledger, since it lives in a
  state directory and not on the disks. Section 3.2's device-root copy
  could be added later if recovery needs it.

## 5. Costs

- One file per node, rewritten atomically on each detection or clearing.
  A few hundred bytes per mark.
- One extra node-to-node message per coordinator-side detection and per
  repair, after the operation.
- A few fields on `LocatedRecord` and `LocalStatus`, absent when empty,
  so old and new nodes interoperate during an upgrade: a node that does
  not know the fields ignores them, and one that does treats their
  absence as "no marks".

## 6. Effect on the tools

- `djbod head` prints a `damaged` line per marked shard.
- `djbod status` gains a `DAMAGED` column.
- `djbod damage list [--prefix]` and `djbod damage repair` are new.
- The web UI shows the marks in the object panel in place of its
  in-memory note, a count on each device row and an Overview tile, and
  a Damage page listing every mark with a repair button. Its in-memory
  note is then removed, since the store remembers.

## 7. Changes to SPEC.md if adopted

- 6.1: `damage.json` in the state directory and the entry limit.
- 11.4 and 11.7: the coordinator marks on failure and clears on a clean
  whole-object check.
- 14: `DeleteVersion` clears marks.
- 16.1: a marked shard is not an error condition; errors are unchanged.
- 18.3, 18.4, 18.8.2: repair and move-shard clear and set marks.
- 19.1.3: `MarkDamage`, `ClearDamage`, `LocalDamage`, `ListDamage`; the
  `damage` field on `LocatedRecord` and `HeadObject`; `damaged_shards`
  on `LocalStatus` and `Status`.
- 20.1.2: the local scrub writes and clears marks.
- 20.3: the UI shows them.
- 22: remove the UI's in-memory note from consideration; add the two
  later uses of 4.5 as deferred.

## 8. Open questions

1. Should a mark set by the coordinator carry the coordinator's node id,
   so a mark that keeps reappearing from one coordinator and never from
   others points at that coordinator's network rather than the disk?
2. Should `ListDamage` be a streamed operation like `Scrub` rather than
   paged like `ListKeys`? Paged fits the size; streamed fits the UI.
3. Retention of `count` and `first_seen` across a repair: a disk that
   keeps corrupting the same shard is a disk to replace. Keeping a
   per-device history after the mark is cleared would show that, at the
   cost of a second, growing list. Suggest a per-device counter in the
   ledger rather than history.
