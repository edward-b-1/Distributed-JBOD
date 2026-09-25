# Day to day

The client environment from [Setup](setup.md#the-client) is assumed:
`DJBOD_NODE` and `DJBOD_CLUSTER`.

## Objects

Keys are paths in the sense that you choose them. The cluster does not
have directories. `photos/cat.jpg` and `photos/dog.jpg` share nothing
except the prefix you can pass to `list`.

```sh
djbod put photos/cat.jpg cat.jpg --content-type image/jpeg
djbod head photos/cat.jpg
djbod get photos/cat.jpg copy.jpg
djbod list --prefix photos/
djbod delete photos/cat.jpg
```

`head` is the map of an object: size, whole-object checksum, the scheme
and block size it was written with, content type, and one line per shard
with the device that holds it. Use it before a `move-shard` or when you
want to know which machine an object depends on.

`get` to a named file removes that file if the read fails, and says so.
A read that reconstructed damaged blocks, or that trusted the record
without every copy, writes the file, prints what it went without on
standard error, and exits 2. `head` does the same for a missing record
copy and exits 2, without reconstructing anything. The reconstruction
line names the key, how many blocks were reconstructed, and for each run
of damage the stripe, the shard index, the device, and the fault
(`checksum mismatch`, `missing`, `unavailable, perhaps for now`, or an
I/O reason). The record line says how many of the `k + m` copies could
not be read. The fix for damage on a disk the node can still write is
`djbod repair <key>`, described in
[Scenarios](scenarios.md#a-bad-block-or-a-missing-shard-file). Repair
refuses while the missing copy is on a device the node cannot read.

`put` of a file streams it. `put` of `-` reads all of standard input
first. A `put` that skipped an unreadable device prints:

```text
stored photos/around.bin as version 01M3BYENKN8G1GR2NV43SEK3M9
photos/around.bin: placed around 1 unavailable device: <device> on node <node>; run `djbod status`
```

and exits 2. Two or more devices use `unavailable devices`. The object
is stored. The cluster is short a disk. A `put` that cannot find
`k + m` usable devices exits 1, stores nothing, and says
`InsufficientDevices`. A whole node that does not answer is a different
refusal, `NodeUnreachable`, because a write will not place shards while
a node is silent.

`list` with no prefix lists every key. `--prefix`, `--start-after`, and
`--limit` narrow it. A truncated page prints a hint on standard error
with the `--start-after` value to pass next. A listing that had to skip
unreadable devices names them. It exits 0 while fewer than `k + m`
devices are out, and the line says every key is still listed. It exits
2 once that many are out, because a key that lived only on them may be
missing. That count is the scheme in the document. An object written
under an older, narrower scheme can be missing from a listing that
claims to be complete. [Concepts](concepts.md#objects-shards-and-k--m)
is that case, and `reencode` is the remedy.

`delete` removes the current version. A second `delete` of a missing key
is `NotFound`, exit 1.

`djbod --json` before the subcommand is the machine-readable form. `put`
then prints `key`, `version`, and `unavailable` (the devices that were
skipped). `get` to a file prints the record. `head` prints the record.
`list` prints `keys`, `truncated`, `unread`, and `complete`.

## Exit codes

| Code | Meaning |
|---|---|
| 0 | The command finished. `status` is 0 even when it reports an unreachable node. `list` is 0 when it skipped unreadable devices and fewer than `k + m` devices are out. A `get` or `put` that had nothing to work around is 0. |
| 1 | The command failed. A `get` to a file left nothing behind. `put`, `delete`, `repair`, and `contents` fail this way when a node does not answer. |
| 2 | The bytes were handled and the cluster is not whole, or a listing may be missing keys. `get` exits 2 when it wrote the correct bytes and had to reconstruct, or when a record copy could not be read. `head` exits 2 for the missing record copy. `put` exits 2 when it stored the object and skipped an unavailable device. `list` exits 2 when `k + m` or more devices are out. `cluster drain` did not move every version. `cluster sync` left at least one node unreachable. `cluster reencode` left at least one object unmoved. `cluster remove-node --force` lost at least one object. `scrub` is the exception in the next table. |

`djbod scrub` uses four codes, because "we looked and found damage" is
different from "we could not finish looking":

| Code | `scrub` | `scrub --repair` |
|---|---|---|
| 0 | Finished, nothing wrong. | Finished, everything found was repaired. |
| 2 | Finished, damage remains. | Finished, some damage could not be repaired. |
| 3 | Did not finish, and no damage was seen. Run it again. | Same. An unfinished scan is not a clean repair. |
| 4 | Did not finish, and damage was seen. | Damage that was seen was repaired where it could be. More may exist. Run it again. |

An unavailable device makes the run unfinished (3, or 4 if something
else was also damaged). A node that does not answer does the same: the
scrub names it, stops the cross-node checks, and exits 3 when it saw no
damage. `--repair` does not rebuild a device's shards while it is still
a member and cannot be read. After `remove-device --force`, a repair
that has nowhere to put the rebuilt shard exits 2 and also prints
`scrub incomplete: WriteFailed`. The last line of the human output
states which of the four outcomes it was, and when a device was
unchecked it is followed by how many versions have a shard there.
`--json` prints one event per line and the exit code carries the outcome.

`djbod-node scrub`, the offline check of one machine, exits 0 when it
found nothing and 2 when it found damage. It never repairs.

`djbod-recover list` exits 2 when any version it found is short of `k`
intact shards, or when a file could not be read. `extract` exits 1 when
it cannot reassemble the object, and it will not overwrite `--out`.

## Move one shard

`move-shard` copies one shard of one object onto another device and
updates the record. It is the primitive `drain` uses.

```sh
djbod head photos/cat.jpg
djbod move-shard photos/cat.jpg 0
djbod move-shard photos/cat.jpg 2 --to nas1-bay3
```

The destination, when you omit `--to`, is chosen the way a write chooses
a device. The command prints whether the shard was copied or rebuilt
from the others, and the record's new revision. If the old device could
not be reached to delete its copy, the line says the source copy was
not removed and that scrub will report it as stale. `djbod scrub
--repair` deletes that stale copy.

## The web UI

`djbod-ui` is a client with a browser in front of it. It holds no
cluster state. Anything it can change, `djbod` can change, and the two
can run at the same time.

```sh
djbod-ui --listen 127.0.0.1:5264 --bootstrap-node 10.0.0.1:5263,10.0.0.2:5263
```

`--listen` defaults to `127.0.0.1:5264`. `--bootstrap-node` is one or
more `ip:port` addresses, comma-separated, also `DJBOD_BOOTSTRAP_NODE`.
They are tried in order, and the first that answers coordinates the
page's requests, for the same reason as `djbod --node`: that machine
does the encoding and the fan-out.
[Concepts](concepts.md#which-node-coordinates) is why a fast local
server belongs first. `--node` and `DJBOD_NODE` are the old spelling.
They still work, print a warning, and are ignored when the new spelling
is also set. After the UI has read the cluster document it also tries
the other node addresses listed there, and it retries the address that
answered last before the configured list. `--cluster` and the TLS file
flags match `djbod`. Towards the cluster it is a client, so a cluster
whose transport is `tls` needs a client certificate on this process.

A request the cluster refuses because a node or a device the object
needs is out is shown as "refused while a node is unreachable" or
"refused while a device is unavailable". "Cannot reach the cluster" is
reserved for the UI process itself having no node to talk to.

The page has five sections.

- **Overview** shows the cluster, the document version, and every device
  with its state and a free-space meter. You can set a label and mark a
  device draining or active from here.
- **Nodes** lists the machines and their devices.
- **Objects** lists keys, shows a record and where its shards sit, and
  offers verify, repair, move-shard, download, upload, and delete.
  Verify reads the object through the node without saving it.
- **Maintenance** runs a scrub and a drain, and shows their events.
  Mark the device draining on Overview first. The page says to rerun a
  drain that skipped versions, then remove the device.
- **Settings** changes the scheme, the block size, and the size limits.
  Changing them does not rewrite existing objects. The page tells you to
  run `djbod cluster reencode` for that. Reencode is not on the page,
  because it streams every object through the client. A larger block
  size is a larger stripe allocation on whichever node is coordinating,
  up to the frame limit. [Concepts](concepts.md#block-size) is that cost.
  The free-space meters already subtract headroom; the fraction itself
  is fixed at `init-cluster`.

Forced removal of a node that does not answer is also left on the
command line. The Overview text says so: `djbod cluster remove-node
<id> --force`.

The HTTP side has no authentication and no TLS. Anyone who can open the
port can administer the cluster with whatever certificate the UI process
holds. Bind it to localhost. From another machine, use an SSH tunnel.
`--host name` adds a DNS name the server will accept in the `Host`
header, besides IP addresses and `localhost`. A request for any other
name is refused with HTTP 403 and `unknown_host`, so a name someone else
points at the address does not become a working origin. A request that
changes state must come from the page itself (`Sec-Fetch-Site:
same-origin` or `none`).

`/api/status` returns the same facts as `djbod status` as JSON. The
other routes are in `crates/djbod-ui/src/lib.rs` under `/api/`.

## From Python

The `djbod` package wraps the same client library. Build notes are in
`crates/djbod-python/README.md`. A client learns the cluster id from the
node, takes a list of addresses and fails over, and can attach user
metadata that `djbod put` does not:

```python
import djbod
client = djbod.Client(["10.0.0.1:5263", "10.0.0.2:5263"])
client.put("photos/cat.jpg", open("cat.jpg", "rb").read(), content_type="image/jpeg")
```

`put` returns the version id. A degraded write is reported by the
library rather than by an exit code. The package readme shows the
exception types.
