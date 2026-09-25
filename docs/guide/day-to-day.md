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
A read that reconstructed damaged blocks writes the file, prints the
damage on standard error, and exits 2. The line names the key, how many
blocks were reconstructed, and for each run of damage the stripe, the
shard index, the device, and the fault (`checksum mismatch`, `missing`,
or an I/O reason). The fix is `djbod repair <key>`, described in
[Scenarios](scenarios.md#a-bad-block-or-a-missing-shard-file).

`put` of a file streams it. `put` of `-` reads all of standard input
first. A `put` that skipped an unreadable device prints:

```text
stored photos/around.bin as version 01M3BYENKN8G1GR2NV43SEK3M9
photos/around.bin: placed around 1 unavailable device(s): <device> on node <node>; run `djbod status`
```

and exits 2. The object is stored. The cluster is short a disk. A `put`
that cannot find `k + m` usable devices exits 1, stores nothing, and
says `InsufficientDevices`.

`list` with no prefix lists every key. `--prefix`, `--start-after`, and
`--limit` narrow it. A truncated page prints a hint on standard error
with the `--start-after` value to pass next.

`delete` removes the current version. A second `delete` of a missing key
is `NotFound`, exit 1.

`djbod --json` before the subcommand is the machine-readable form. `put`
then prints `key`, `version`, and `unavailable` (the devices that were
skipped). `get` to a file prints the record. `head` prints the record.
`list` prints `keys` and `truncated`.

## Exit codes

| Code | Meaning |
|---|---|
| 0 | The command did what it said, and it did not have to work around damage or an unreachable peer. |
| 1 | The command failed. A `get` to a file left nothing behind. |
| 2 | `get` wrote the correct bytes and had to reconstruct. `put` stored the object and skipped an unavailable device. `cluster drain` did not move every version. `cluster sync` left at least one node unreachable. `cluster reencode` left at least one object unmoved. `cluster remove-node --force` lost at least one object. `scrub` is the exception in the next table. |

`djbod scrub` uses four codes, because "we looked and found damage" is
different from "we could not finish looking":

| Code | `scrub` | `scrub --repair` |
|---|---|---|
| 0 | Finished, nothing wrong. | Finished, everything found was repaired. |
| 2 | Finished, damage remains. | Finished, some damage could not be repaired. |
| 3 | Did not finish, and no damage was seen. Run it again. | Same. An unfinished scan is not a clean repair. |
| 4 | Did not finish, and damage was seen. | Damage that was seen was repaired where it could be. More may exist. Run it again. |

An unavailable device makes the run unfinished (3, or 4 if something
else was also damaged). `--repair` does not rebuild that device's
shards while it is still a member. The last line of the human output
states which of the four outcomes it was. `--json` prints one event per
line and the exit code carries the outcome.

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
djbod-ui --listen 127.0.0.1:5264
```

`--listen` defaults to `127.0.0.1:5264`. `--node`, `--cluster`, and the
TLS file flags are the same as `djbod`, including the `DJBOD_*`
variables. Towards the cluster it is a client, so a cluster whose
transport is `tls` needs a client certificate on this process.

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
- **Settings** changes the scheme and the size limits. Changing the
  scheme does not rewrite existing objects. The page tells you to run
  `djbod cluster reencode` for that. Reencode is not on the page,
  because it streams every object through the client.

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
