# Concepts

## Nodes, devices, and the cluster

A **node** is one `djbod-node` process on one machine. It listens on one
address, usually port **5263**. Every node runs the same program. Any node
can answer a client. There is no separate master process.

A **device** is one directory on one filesystem, normally one disk mounted
on its own. The node is told the paths in its configuration file. On first
use it writes `DISTRIBUTED-JBOD-DEVICE.json` into an empty directory and
from then on that directory belongs to one cluster. The device's permanent
name is a UUID in that file. The path is only on that machine.

The **cluster document** is one JSON file, `cluster.json`, in each node's
state directory. Every node holds a copy. It lists the nodes, the
addresses clients and peers use, the devices and which node owns each, the
erasure scheme, the size limits, and the transport (`plain`,
`tls-optional`, or `tls`). Changes to it are numbered. `djbod cluster show`
prints the version each node holds. `djbod cluster sync` pushes the
highest version out to every node that answers.

A node's configuration file is not the cluster document. It is that
machine's own paths, its UUID, its listen address, and, when you use TLS,
the certificate files. Membership lives in the cluster document.
`djbod-node` will not invent a disk that the file does not list, and a
disk that is only in the file is not in the cluster until `init-cluster`,
`join`, or `add-device` puts it there.

## Objects, shards, and k + m

An object is stored under a key, such as `photos/cat.jpg`. A key is a
string, up to the cluster's key-length limit (16 KiB unless you change
it). The current bytes of a key are one **version**. Writing the same key
again stores a new version and the old one is replaced.

Each version is split into stripes and erasure-coded. The scheme is two
numbers:

- `k` is the number of data shards.
- `m` is the number of parity shards.

Any `k` of the `k + m` shards are enough to rebuild the object. The
cluster stores one shard on each of `k + m` different devices.

| Scheme | What it stores | What one failure costs | Overhead |
|---|---|---|---|
| `1+0` | One copy of the object, on one device. Checksums are still checked on every block and on the whole object. | The device that holds it. There is no parity to rebuild from. | none |
| `1+1` | Two copies. This is a mirror. | One lost shard. | 100% |
| `3+1` | Three data shards and one parity shard. | One lost shard. | 33% |
| `4+2` | Four data shards and two parity shards. | Two lost shards. | 50% |

`1+0` is a legal scheme (`init-cluster --k 1 --m 0`, or `cluster
set-scheme --k 1 --m 0`). It is the right setting when the pool is a
single copy with checksums, for example because another system already
keeps a second copy, or because you are about to add disks and will
raise the scheme and re-encode. Any `k` from 1 to 32 and any `m` from 0
to 8 are accepted, with `k + m` at most 64. A one-device cluster created
with `--k 1 --m 0` stores an object and reads it back. `head` shows
`scheme 1+0`.

A `1+0` object has its only record copy on one device. Reads and
listings decide whether they can see every key by counting unreadable
devices against the scheme in the document, which is the scheme new
writes use, and they assume each object has a copy on `k + m` devices.
While the document is still `1+0`, one unreadable device is enough for
`list` to exit 2 and say a key stored only there may be missing, and
for `get` of that key to fail because the device could not be read.
If you later raise the scheme and do not reencode, the old object still
has one copy. With the document at `2+1` and only that one device out,
`list` exits 0, says every key is listed, and omits the key, and `get`
says `NotFound`. The bytes are still on the device that is out.
`djbod cluster reencode` is what gives those objects the new number of
copies, which is what the listing and the read are counting on.

`m` is a property of how the object was written. It is the number of
shard failures a read of that object can reconstruct. It is not a
promise about a machine that is switched off. See
[When a machine is off](#when-a-machine-is-off).

The scheme is a field of the cluster document, so new writes all use the
same one. Each object's record remembers the scheme it was written with.
`djbod cluster set-scheme` changes the document and moves no bytes. Old
objects stay readable. `djbod cluster reencode` rewrites them one at a
time onto the new scheme.

## Block size

The default block size is 1 MiB. `init-cluster --block-size` and
`cluster set-scheme --block-size` take a multiple of 4096 between 64 KiB
and 64 MiB. A stripe of an object is `k` of these blocks placed end to
end, so a `3+1` object with the default block is striped every 3 MiB.

One shard block travels inside one data frame. The payload is a 16-byte
prefix followed by the block. The largest payload a peer will accept is
64 MiB plus 4096 bytes. A longer length is refused and is not allocated.
That cap is why the block size stops at 64 MiB: the block and its prefix
have to fit in the frame.

Memory on the coordinating node scales with the block size, and stops
at that same cap. While it writes an object it holds one stripe of the
body, `k × B` bytes, and the `k + m` encoded shard blocks, each `B`
bytes, for the stripe it is sending. A read that is intact holds `k`
blocks of `B`; a read that has to reconstruct holds `k + m` of them.
`B` is that object's block size. Raising the document's block size from
1 MiB to 64 MiB therefore multiplies the coordinator's working set for
each new write by 64, up to the frame limit, and multiplies it again by
how wide `k + m` is. The client's upload is chopped into 1 MiB frames
regardless. The shard block size is the number that moves.

Changing the block size moves no existing bytes. Each object keeps the
block size it was written with, and a later read of that object uses
that size, so a cluster can hold a mixture. `djbod cluster reencode`
rewrites the old objects at the current block size, and during that
rewrite the coordinator allocates for the new size. The node has to be
able to allocate the largest block size it will be asked to encode or
decode. A machine that is comfortable at 1 MiB can be a poor coordinator
for 64 MiB stripes. See [which node coordinates](#which-node-coordinates).

## Where a shard is placed

For each write, the cluster picks the `k + m` active devices with the most
free space that it can currently read, and puts one shard on each. It does
not try to put those shards on different machines. Two shards of one
object can sit on two disks in the same server.

A device marked `draining` receives nothing new. A device the node cannot
read is skipped, and the write says so. If fewer than `k + m` usable
devices have room, the write stores nothing and fails with
`InsufficientDevices`.

## Headroom

Headroom is a fraction of each device's filesystem size that placement
will not use. The default is `0.05`, set by `init-cluster --headroom`,
and it has to be between 0 and 0.5. There is no later command to change
it.

The free space `djbod status` prints, and the free space a write
consults, is the filesystem's available bytes minus `headroom` times the
filesystem's total size. On a 10 TiB disk at the default, 512 GiB is
held back. A device whose remaining space is inside that reserve is
treated as full for new shards, drains, and repairs. The point of the
reserve is that a write which has already been accepted still has room
to finish, and the filesystem is not packed solid underneath the
objects.

This is separate from the percentage ext4 reserves for the root user
(`tune2fs -m`). That reservation is also subtracted by the filesystem
before "available" is reported, so the two stack. Headroom is the
cluster's own reserve, the same on every filesystem, whatever the
filesystem's own reserved-block setting is. In-flight shard allocations
are already gone from the filesystem's available count, because the
node reserves them with `fallocate`.

## Names

You will see three kinds of name.

- The **cluster id** is a UUID. `djbod --cluster` and `DJBOD_CLUSTER` take
  it. `init-cluster` prints it. `djbod get-cluster-id --node <address>`
  prints it again, with no `--cluster`. A **cluster name**, set with
  `init-cluster --name` or `djbod cluster set-name`, is shown beside the
  id. The name can contain spaces. The id is what the client sends.
- A **node id** is the UUID in that machine's configuration. It does not
  change when the address changes. A **node label** (`djbod cluster
  set-node-label`) is a short name, unique in the cluster, 1 to 128
  bytes, with no spaces and not itself a UUID.
- A **device id** is the UUID in `DISTRIBUTED-JBOD-DEVICE.json`. A
  **device label** (`djbod cluster set-device-label`) has the same rules as a
  node label. Commands that take a device or a node accept either the
  UUID or the label.

## Device state and availability

The cluster document stores a **state** for each device. That is a
decision you record:

| State | Meaning |
|---|---|
| `active` | Eligible for new shards. |
| `draining` | Serves what it already holds. Receives no new shard. `djbod cluster drain` is allowed to move its shards off. |
| `removed` | Retired. The id stays in the document so the same disk is recognised if it reappears. It is not a place to store anything. |

**Unavailable** is not a state. It is what a node reports when it cannot
read a device it still owns: the directory is missing, the disk is not
mounted, or `DISTRIBUTED-JBOD-DEVICE.json` cannot be stat'd. `djbod
status` prints `active, unavailable` or `draining, unavailable`. The
document still says `active` or `draining` until you change it.

`djbod status` lists every device in the document, including ones marked
`removed`. A removed device that the node does not have open is shown as
`removed` with no free space. The state is `removed` rather than
`unavailable`: it was retired on purpose, and its readability is no
longer the question.

## Which node coordinates

Any node can answer a client. The node the client is actually connected
to is the **coordinator** for that request. `djbod status` names it:
`answered by node`.

The coordinator is the machine that does the work of the operation. It
asks the other nodes for records and free space, chooses devices, encodes
or decodes the stripes, checks the checksums, and streams the object
body to or from the client. The other nodes read and write the shard
files on their own disks and send blocks back. A large `put`, `get`,
`repair`, or `reencode` is therefore CPU and network on the coordinator,
and mostly disk on everyone else.

You choose the coordinator by which address you pass. `djbod --node`
takes one or more `ip:port` values, comma-separated, also as
`DJBOD_NODE`. They are tried in order. The first that accepts the
connection is the coordinator. If that connection later fails, the next
address in the list is tried. The list is not a filter on which disks
the operation may use. A `get` coordinated by nas1 still reads shards
from nas2 and nas3. The list is only which machine does the
coordinating.

That choice matters when the machines are not alike. One server may have
more CPU and RAM than the others, and the block size section above is
why the RAM matters. The links between machines may not be the same
width or latency: a coordinator on a gigabit port reconstructing an
object whose disks are on a 100-megabit port spends the read waiting on
the slow link, and a coordinator that sits in another region pays a
round trip to every node for every request. Put `--node` in the order
you want the work done. A powerful machine on the same site as the
client, with a wide path to the disks, should be first. A small board,
or a machine in another region that is only there to hold a disk, can
still be in the list so the client has somewhere to go when the first
is down. It should not be the machine that decodes every large read.

`djbod` tries only the addresses you gave it. `djbod-ui` is wider. Its
`--bootstrap-node` (environment `DJBOD_BOOTSTRAP_NODE`) is the same
ordered list, and `--node` / `DJBOD_NODE` is the old spelling, still
accepted with a warning. After the UI has read the cluster document it
also tries the other node addresses the document lists, so a node
leaving does not by itself take the page down. It remembers the address
that answered last and tries that one first the next time. The page's
requests are still coordinated by whichever node it connected to.

## When a machine is off

A node that does not answer is reported, and different commands do
different things with that report.

`djbod status` still prints. The down node's devices are
`active, unavailable` with no free space, and a line on standard error
says the node is unreachable and why. `djbod cluster show` puts
`unreachable:` and the reason in that node's version column.

`djbod get` and `djbod head` use the record copies they can read. If at
least `k` agreeing copies are on nodes that answered, the read returns
the object, names the copies it went without, and exits 2. Shard blocks
on the down node are reconstructed from parity when at most `m` shards
are out. The read fails when fewer than `k` record copies can be read,
or when more than `m` shards are on machines that do not answer.

`djbod list` also goes around what it cannot read. It names those
devices. It exits 0 while fewer than `k + m` devices are out, and the
line says every key is still listed. That matches objects written at the
document's current scheme, which each have a record copy on `k + m`
devices. An older object with fewer copies can be missing from that
page. See the `1+0` note above. `list` exits 2 once `k + m` or more
devices are out, because an object stored only on them would be missing
from the listing.

`djbod put`, `djbod delete`, `djbod repair`, and `djbod contents` do not
go around a missing node. They fail with `NodeUnreachable` and the node
id. A write that cannot ask every node does not guess at placement, and
repair will not rewrite an object while a node that might hold a copy
is silent. `djbod scrub` does run, reports that the node could not be
scrubbed, stops the cross-node checks, and exits 3.

When the node is up and one of its devices cannot be read, `put` skips
that device. The write exits 2 when `k + m` other devices are usable,
and it fails with `InsufficientDevices` when they are not. `contents`
names the device and prints the others. `repair` fails with
`DeviceUnavailable`, because it would write the replacement back onto
that device.

When the machine comes back, the node reads its saved document, asks its
`bootstrap_peers` for anything newer, and serves again. Reads that were
exiting 2 because of that node go back to exiting 0 without a repair.
`bootstrap_peers` is a list you write in that machine's `node.toml`.
`join` does not fill it in. The membership itself, which nodes exist and
at which addresses, lives in the cluster document and is what `join`
updates. [Deployment](deployment.md#three-servers) says which field is
which.

If the machine is not coming back, `djbod cluster remove-node` cannot
drop it, because the node has to acknowledge. `djbod cluster remove-node
--force` is the command for a node that does not answer. It rebuilds
shards that lived on that node onto other devices, and it can only do
that when some other device has room and does not already hold a shard
of that object. [Scenarios](scenarios.md#a-node-will-never-come-back)
walks through both the case that rebuilds and the case that loses the
object.

## Damage the cluster can still read

These are different from a machine that is off or a disk that is gone,
because the nodes are up and the record copies can still be gathered.

- A shard block whose checksum does not match, or a shard file that has
  been deleted, is reconstructed for that read. `djbod get` writes the
  correct bytes and exits 2. The damage is still on disk. Every later
  read of that object pays the reconstruction cost until `djbod repair
  <key>`.
- A write that had to skip a device the cluster cannot read stores the
  object on other devices. `djbod put` prints those devices and exits 2.

Exit 2 in both cases means the bytes you asked for were handled and the
cluster is not whole. Exit 1 means the operation did not succeed: the
object was not stored, or the read did not produce the object. A `get`
that exits 1 removes a partial output file when the output was a named
file.

`djbod scrub` is how you find damage before a client does. It does not
run on its own. [Scenarios](scenarios.md#finding-damage-before-a-client-does)
has the exit codes and a cron line.

## What the cluster will not do

Nothing moves data unless you run a command that says it will: `repair`,
`scrub --repair`, `move-shard`, `cluster drain`, `cluster reencode`, or
`cluster remove-node --force`. There is no alert daemon. The signal is
the command's exit code and the line it prints. A scheduled `djbod scrub`
whose exit code is checked is the health check.
