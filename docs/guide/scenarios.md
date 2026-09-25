# Scenarios

Each scenario starts from what you can see, then gives the commands in
order and the check that says you are done. The client is configured
with `DJBOD_NODE` and `DJBOD_CLUSTER` as in [Setup](setup.md#the-client).
Device and node labels are the ones from [Deployment](deployment.md).

Have a spare device before you drain or force-remove anything that still
holds data. A rebuilt shard has to land on a device that does not
already hold a shard of that object. If no such device has room, the
object is lost even when `m` says a parity shard remains. The sections
on [replacing a device](#replace-a-device) and [a node that will never
come back](#a-node-will-never-come-back) both run into this, and the
second one shows the `LOST` line.

## A disk is unreadable

The disk failed, the filesystem is not mounted, or the device directory
was replaced by an empty mount point. The node process is still running
and its other disks are fine.

`djbod status` shows that device as `active, unavailable`, with zero
total and zero free, and prints:

```text
1 device(s) unavailable: their node cannot read them (disk failed, not mounted, or destroyed)
```

The document still says `active`. You have not decided anything yet.

A `get` of an object that had a record on that disk fails, rather than
returning reconstructed bytes:

```text
error: RecordsInconsistent: 2 record copies found, 3 expected (revision 0)
```

`djbod repair <key>` also fails, with `DeviceUnavailable` and the path,
while the disk is still a member. Repair will not rebuild onto other
disks until the membership change says the disk is gone.

A `put` can succeed by avoiding the disk. It stores the object, names
the device, and exits 2. `djbod contents` prints `unavailable` for that
device and continues with the others. `djbod scrub` reports the device
once, reads nothing from it, and exits 3:

```text
device unavailable: device at /mnt/disk2/djbod is unavailable: its directory or identity file cannot be read
...
incomplete, no damage seen; run it again
1 device(s) unavailable, not checked: restore or retire them, then run again
```

If the disk might come back, mount it and leave the document alone. The
next `status` shows it `active` again, and the node logs `device
available again`. Reads of the objects that failed start working,
because the missing record copy is back. Do not format the disk.

If the disk is dead:

```sh
djbod cluster remove-device nas1-bay2 --force
```

The command prints whether the node can still read the device, how many
active devices would remain, and how many shards `m` allows you to lose.
It then waits for you to type the device id. A wrong id changes nothing.
`--yes` skips the prompt for a script. With `--json` the same facts are
one JSON object before the confirmation.

```text
device <uuid> on node <uuid> is active and its node cannot read it
marking it removed loses every shard on it: versions with at most m = 1 shards there are rebuilt from the others by `djbod scrub --repair`; any with more are lost. Nothing is checked or moved now.
type the device id to mark it removed:
```

After you confirm, the device is `removed` in the document and nothing
has been copied yet. Rebuild:

```sh
djbod scrub --repair
```

For each object that had a shard there, the scrub reports
`RecordsInconsistent` (fewer record copies than the record listed) and
then `repaired <key>: 1 shard(s) rewritten`. A clean finish is exit 0
and the line `complete, everything found was repaired`. Then:

```sh
djbod get <a key that lived on that disk> /tmp/check.bin
djbod scrub
```

`get` should exit 0. `scrub` should exit 0. `status` shows the device
as `removed` until you restart the node.

Take the path out of that machine's `devices` list and restart the node.
An empty directory left at the old path is tolerated: the node logs that
the path has no identity file and starts. A directory that still has an
`objects/` tree and no `DISTRIBUTED-JBOD-DEVICE.json` is refused, and
the node does not start:

```text
<path> has an objects directory but no identity file; the identity file was deleted or the directory belongs to something else
```

Remove that path from the configuration, or restore the identity file
from backup if the disk's contents are actually intact. Do not delete
the `objects` tree of a disk you hope to read again.

An object with more than `m` shards on the dead disk cannot be rebuilt.
`scrub --repair` says so for that key and exits 2. Those bytes are gone
from the cluster. `djbod-recover` cannot invent them either if the disk
itself is unreadable. This is why a dead disk is a membership change
followed by a rebuild, and why the pool should have had a spare.

## A bad block or a missing shard file

The nodes are up, and the record files are intact. One shard's data is
corrupt, or the `.shard` file has been deleted.

`djbod get` writes the correct bytes and exits 2:

```text
photos/cat.bin: 1 block(s) reconstructed from parity; the data is correct, the damage on disk is not repaired, and every read pays again until `djbod repair photos/cat.bin` runs
  stripe 0  shard 0  device <uuid>  checksum mismatch
```

A deleted shard file uses the same shape with the fault `missing`.
`head` still works. Repair that key:

```sh
djbod repair photos/cat.bin
djbod get photos/cat.bin /tmp/check.bin
```

Repair prints each shard as `intact` or as the damage it found, and
`-> rewritten` on the ones it replaced. The following `get` exits 0.
The checksum of the file matches the original.

`djbod scrub` finds this before a client does. `djbod scrub --repair`
rewrites every such object. On a large pool, `--rate-mib 50` caps each
node's read rate. Run it from cron or a systemd timer on any one
administration machine. The exit codes are in [Day to day](day-to-day.md#exit-codes).
There is no record inside the cluster of when a scrub last ran, so keep
the job's output.

A single machine's disks can be checked with the node stopped, or with
it running:

```sh
djbod-node scrub --config /etc/djbod/node.toml
```

That command reads the local devices only, prints a line per finding,
and exits 2 if it found any. It does not repair and it does not talk to
the other nodes. `--json` prints one finding per line. `--rate-mib`
caps the read rate. To limit which paths it opens, pass `--device` for
the paths you want. That flag replaces the file's device list for this
invocation.

## Too many shards are damaged

Damage to more than `m` shards of the same stripe cannot be rebuilt.
With `2+1`, two bad shards is enough. `djbod get` exits 1 and, when the
output was a file, deletes the partial file:

```text
BlockChecksumMismatch: 2 damaged block(s) in stripe 0, 1 usable of 2 needed
```

`djbod repair` exits 1 and names the stripe and the damaged shard
indexes. It does not write a replacement. The object is gone unless you
have another copy outside the cluster, or `djbod-recover` can see `k`
intact shard files on disks you can still mount. Deleting the key
succeeds even in this state, if you want the damaged version gone:

```sh
djbod delete photos/two.bin
```

## A record file is unreadable

Something has damaged one `.meta.json` so it is no longer valid JSON, or
no longer a record. `djbod head` and `djbod repair` both fail with
`RecordsInconsistent` and the path of that file. Repair stops on the
bad file. It does not skip it and rewrite from the other copies while
the bad file is still there.

The error names the file. Move that one file aside (do not delete the
`.shard` file next to it, and do not touch the other devices' copies):

```sh
mv /mnt/disk3/djbod/objects/.../<version>.meta.json /var/tmp/bad-record.json
djbod repair photos/rec.bin
djbod head photos/rec.bin
```

Repair then succeeds. When the shard data was fine, the report can say
`0 shard(s) rewritten` and still have restored the record. `head`
showing the key again is the check. `djbod scrub` should then exit 0.

If two intact copies of the same revision disagree with each other,
repair refuses with `record copies at revision N disagree` and will not
choose. That object needs an external copy, or `djbod-recover` against
the device directories if you can decide which copy to trust by reading
them. Do not delete both.

## Finding damage before a client does

```sh
djbod scrub
djbod scrub --repair --rate-mib 50
```

Schedule the first form. Treat a non-zero exit as the alert. The second
form is what you run after you have read the findings, or from a job
that is allowed to rewrite data. An unavailable device makes the exit
code 3 or 4 until you restore the disk or retire it. A node that does
not answer fails the scrub with `NodeUnreachable` the same way a `get`
fails. Scrub is not a way to work around a missing machine.

## Add a device

The new disk is mounted, and the directory on it is empty.

1. Add the path to `devices` in that machine's configuration file.
2. Run `add-device` while the node is still up. It reads the file, so
   the path has to be in the file already, and it talks to the running
   node.

```sh
djbod-node add-device --config /etc/djbod/node.toml --path /mnt/disk4/djbod
```

The command prints the new document version and `restart the node to
serve the new device(s)`. A path that is not in the configuration is
refused. A device that is already in the document is refused. A
directory that is not empty is refused.

3. Restart the node. `djbod status` lists the new device as `active`.
   `djbod contents` shows zero versions until a later write or a drain
   places a shard there.
4. Label it: `djbod cluster set-label <new-uuid> nas1-bay4`.

The node does not pick up a new path from the file by itself. The
restart after `add-device` is required.

## Replace a device

You want a disk out, and the disk still reads. Add the replacement
first when the remaining active devices would otherwise drop below
`k + m`.

With exactly `k + m` devices, marking one `draining` and running drain
exits 2 without moving anything:

```text
draining <uuid> on node <node>: 2 version(s), 18.4 KiB to move; ... free on 2 active device(s), 3 needed per version
0 moved, 0 skipped, 0 deleted meanwhile
drain of <uuid> incomplete: InsufficientDevices: 2 active device(s), but every version needs 3; no version has a legal target; add capacity, or pass --partial to move what fits
```

The device stays `draining`. Put it back if you are not ready:

```sh
djbod cluster set-state nas1-bay0 active
```

`--partial` starts the pass anyway and moves what fits. Prefer adding
capacity. After [adding the new device](#add-a-device) and restarting:

```sh
djbod cluster set-state nas1-bay0 draining
djbod status
djbod cluster drain nas1-bay0
djbod contents nas1-bay0
```

`set-state` moves nothing. `status` should show `draining`. `drain`
prints an estimate, then one line per version:

```text
moved    archive/a.bin  shard 1 -> <new-device-uuid>
2 moved, 0 skipped, 0 deleted meanwhile
```

Exit 0 means every version moved. Exit 2 means some were skipped. The
skipped lines say why. Fix that and run `drain` again. The device keeps
serving the shards that remain. `contents` on a finished drain shows
zero versions. A device with zero versions is empty.

```sh
djbod cluster remove-device nas1-bay0
```

This refuses an `active` device, and it refuses a draining device that
still holds a current shard. The error tells you to drain first and
gives an example key. On success it prints the document version and
tells you to take the path out of the node's configuration and restart.

Edit `devices`, restart the node, and `djbod get` an object that used
to have a shard on the old disk. Exit 0 means the move is complete.
Pull the disk after that restart, not before.

`djbod cluster drain --node-id nas1` drains every device of that node
which is already `draining`, one after another. It does not mark them
draining for you.

## Add a node

A new machine, with its own `node_id`, its own disks mounted, and an
empty device directory on each. The configuration lists
`bootstrap_peers` so that later restarts can catch up. With any existing
node running:

```sh
djbod-node join --config /etc/djbod/node.toml \
  --peer 10.0.0.1:5263 --cluster "$DJBOD_CLUSTER"
djbod-node run --config /etc/djbod/node.toml
```

`join` prints `joined cluster <id> as node <uuid>`, the document
version, and the new device ids. Then:

```sh
djbod cluster set-node-label <new-node-uuid> nas2
djbod cluster show
djbod put archive/c.bin c.bin
djbod head archive/c.bin
djbod --node 10.0.0.2:5263 get archive/a.bin /tmp/a.bin
```

`cluster show` lists every node at one document version. `head` shows
which devices received the new object. Shards can land on the new
machine immediately, because its disks are empty. A `get` aimed at the
new node returns objects that were stored before it joined.

A join pointed at the wrong cluster id is refused. A device directory
that already belongs to this cluster and was removed is refused until
you pass `--wipe-removed-device`, which erases it. See
[Reusing a disk](#reusing-a-disk-the-cluster-has-removed).

## Replace a node

The old machine works. You have a new machine to take its place. This
is [add a node](#add-a-node), then a drain of the old machine, then
`remove-node`. Do it in that order so the shards have somewhere to go
while the old node can still serve them.

After the new node is up and labelled:

```sh
djbod cluster set-state nas2-bay0 draining
djbod cluster set-state nas2-bay1 draining
djbod cluster drain --node-id nas2
djbod contents --node-id nas2
djbod cluster remove-node nas2
```

`contents` should show zero versions on each of nas2's devices before
you remove the node. `remove-node` refuses while any of those devices
is `active`, and it refuses while any object still has a shard on them.
On success it prints the document version. The old process acknowledges
the new document, logs that it has been removed, stops accepting
connections, and exits. The log line is:

```text
this node was removed from the cluster at document version 20; stopping. Its devices can be reused with `djbod-node join --wipe-removed-device`, which erases them.
```

`djbod cluster show` no longer lists nas2. `djbod get` of an object that
used to have a shard there exits 0. You can power the old machine off
after its process has exited.

`remove-node` cannot remove the only node in the cluster.

## A node is switched off and will be back

Leave it in the document. While it is down:

- `djbod cluster show` prints the node as `unreachable:` and the reason
  (`Connection refused` if nothing is listening).
- `djbod status`, `djbod get`, and `djbod put` exit 1 with
  `NodeUnreachable` and the node id. This includes objects that have no
  shard on that machine.
- `djbod identity --node <an address that is up>` still answers.

Start the node again. It adopts a newer document from its bootstrap
peers if the cluster changed, and the `get` that failed works again
without `repair`. If `cluster show` then shows different document
versions, `djbod cluster sync` copies the highest version to every node
that answers. `sync` exits 2 if any node is still unreachable. It
prints `updated`, `already current`, or `unreachable` for each.

Do not force-remove a node you expect to boot. Force-removal drops its
devices from the document. The disks will not simply rejoin.

## A node will never come back

The process is stopped and will not be started. Ordinary `remove-node`
cannot succeed, because the node has to acknowledge. Check first that
the remaining machines have devices that do not already hold shards of
the objects stuck on the dead one. The usual way to get that room is to
[add a node](#add-a-node) or [add a device](#add-a-device) before you
remove the dead one.

```sh
djbod cluster remove-node nas2 --force
```

If the node still answers, the command refuses and tells you to drain
it instead. If it does not answer, the command waits a few seconds,
then prints the reason, how many devices it held, how many versions
have shards there, and which of those versions have more than `m`
shards on the dead node. Versions with more than `m` are listed as
unrecoverable. You then type the node id. `--yes` skips the prompt.

A run where one shard of `keep/obj.bin` sat on the dead node, and a
spare device remained on a live node, printed:

```text
node <uuid> at 10.0.0.2:5263 does not answer: cannot reach peer 10.0.0.2:5263: I/O error: Connection refused (os error 111)
it holds 1 device(s); 1 version(s) have shards there
every one of them can be rebuilt from the other shards (at most m = 1 on the dead node)
node <uuid> removed (document version 3); rebuilding 1 version(s)
rebuilt  keep/obj.bin  1 shard(s) placed on other devices
1 rebuilt, 0 lost
```

`djbod get keep/obj.bin` then exits 0 and the bytes match. `djbod head`
shows the rebuilt shard on a device that is still in the cluster.

A run that removed a node without leaving a free device lost the object
instead, and exited 2:

```text
it holds 2 device(s); 1 version(s) have shards there
every one of them can be rebuilt from the other shards (at most m = 1 on the dead node)
node <uuid> removed (document version 3); rebuilding 1 version(s)
LOST     keep/obj.bin  InsufficientDevices: 1 shard(s) are on devices no longer in the cluster, but only 0 active device(s) with 6160 bytes free hold no shard of this version
0 rebuilt, 1 lost
```

The parity shard was not enough on its own, because the replacement
shard had nowhere to be written. The node is already removed at that
point. Add devices and the lost object does not come back from parity
that was discarded with the dead node. Add the spare first.

After a clean force-removal, `djbod scrub` should exit 0. The dead
machine's disks will not rejoin as they are. See the next section.

## Reusing a disk the cluster has removed

A device that has been removed, or a node that has been removed, still
has `DISTRIBUTED-JBOD-DEVICE.json` on the disk. `init-cluster`, `join`,
and `add-device` refuse that directory. `--wipe-removed-device` erases
it and initialises a new device id. The flag's help text says it
destroys the data. The removed node's own exit message names the flag.

```sh
djbod-node join --config /etc/djbod/node.toml \
  --peer 10.0.0.1:5263 --cluster "$DJBOD_CLUSTER" \
  --wipe-removed-device
```

There is no undo. Use it when the copies you care about are already on
the remaining cluster, or when you have given up on this disk's bytes.

## Change the scheme

```sh
djbod cluster set-scheme --k 4 --m 2
```

This refuses while fewer devices are `active` than the new `k + m`.
The error says how many are active and how many the scheme needs. On
success it moves no data. New writes use the new scheme. The command
reports how many objects are still on another scheme:

```text
scheme is now 1+1 with 1048576 byte blocks (document version 21); new writes use it
3 object(s) are stored at another scheme and stay readable as they are; `djbod cluster reencode` rewrites them
```

`--block-size` is optional and must be a multiple of 4096 between 64 KiB
and 64 MiB. Omit it to keep the current block size.

```sh
djbod cluster reencode
```

This streams each object through the client and writes it back under the
current scheme. A re-encoded object gets a new version id. The command
prints one line per object (`2+1 -> 1+1` and the old and new version
ids) and a total. It is safe to interrupt and run again. A rerun
rewrites only what is still on the old scheme. Exit 2 means at least
one object failed. Those lines say why.

You can run with a mixture of schemes for as long as you like. `head`
shows the scheme of the object you asked about, which can differ from
`cluster-config`.

## Change the size limits

```sh
djbod cluster set-limits --max-object-bytes 109951162400
djbod cluster set-limits --max-key-bytes 8192 --max-user-metadata-bytes 1048576
```

At least one of the three flags is required. The command prints all
three current values and the document version. The limits apply to new
writes. An object already stored above a new maximum stays where it is.
A write that exceeds the object limit fails with `ObjectTooLarge` and
the size it saw. `cluster-config` shows the values the document holds.

## A node moved address

Edit `listen` or `advertise` in the configuration to the new address.
Leave `bootstrap_peers` pointing at a node that is up. Restart. Before
it serves, the node proposes a document that lists the configured
address. The log line is:

```text
the cluster document now lists this node at its configured address from=<old version> to=<new version> listed=["10.0.0.1:5263"] configured=10.0.0.4:5263
```

`djbod cluster show` then shows the new address. Point clients at it.
If the other nodes cannot be reached, the node does not start, and the
error tells you to make them reachable or to configure the address the
document already has.

To update the document while the node is still answering at the old
address, use `djbod cluster set-address nas1 10.0.0.4:5263` first, then
change the file and restart so the process is actually listening there.
`set-address` refuses if the node is not answering at the address the
document currently lists.

## Nothing will start

`djbod-recover` reads device directories. It does not need a node, a
cluster document, or the identity file. It does not write to a device.

```sh
djbod-recover list /mnt/disk0/djbod /mnt/disk1/djbod /mnt/disk2/djbod
djbod-recover extract photos/cat.jpg --out /tmp/cat.jpg \
  /mnt/disk0/djbod /mnt/disk1/djbod /mnt/disk2/djbod
```

`list` prints every key and version it can find, the record revision,
the size, how many shards are present out of how many the record wants,
and `recoverable` or a reason. The summary line is `<n> version(s), <n>
not recoverable, <n> problem(s)`. Exit 2 means something was short of
`k` shards or a file could not be read. A shard whose record is missing
is listed under the key hash, because the key itself was in the record.

`extract` reassembles one key. It takes the newest version unless you
pass `--version`. It verifies blocks, decodes from any `k` intact
shards, checks the whole-object checksum, and writes `--out`. It
refuses when `--out` already exists, and it refuses when fewer than `k`
intact shards are on the paths you gave. The paths can be copies of the
device directories. Three intact devices of a `2+1` object are enough,
which is what a run against the devices that remained after a dead disk
was retired produced: `3/3` and `recoverable`, and `extract` wrote a
file that matched the original.

This is the path when the nodes will not run and you can mount the
disks on a machine that has the `djbod-recover` binary. It is not a
faster `get`. A running cluster should be repaired with `scrub
--repair` or `remove-device --force`, which put the shards back into
the cluster. `extract` only writes a file on the side.
