# Coordinated bitrot testing

`djbod-bitrotter` deliberately corrupts shard payloads on disposable test
clusters. It is a separate, unpublished package. Product crates do not
depend on it; ordinary builds and the product Docker image exclude it.

**Deliberately damaging data is for testing purposes only. Use disposable
test data. This can cause permanent data loss even when n <= m, because
existing corruption or other sources of bitrot outside this process may
already have consumed some or all of the erasure tolerance.**

**Damaging more than m shards in the same stripe will result in certain
data loss: normal repair cannot reconstruct the affected stripe from
the remaining shards in this k+m set.**

## Build

From the repository root on Linux/Unix:

```sh
cargo build --release --locked -p djbod-bitrotter
cargo test --locked -p djbod-bitrotter
```

The package builds two executables sharing the same library:
`target/release/djbod-bitrotter` for the coordinator machine, and
`target/release/djbod-bitrotter-worker` for participating storage nodes.
Install each explicitly where it is needed. Nothing starts automatically.
Tests create their own temporary clusters, launch three separate worker
executables, and require localhost networking.

## Workers and certificates

Run one worker per storage node, with access to that node's selected
device roots. The default listener is **TCP `0.0.0.0:6666`**. Workers have
their own configuration, TLS protocol, and journals. They do not change
the node protocol, cluster document, or shard format.

Every connection requires mutual TLS and an explicitly allowed controller
certificate. Use a dedicated testing CA. The repository's existing PKI
helper can issue the certificates, without changing the production setup:

```sh
scripts/djbod-pki.sh --dir ./bitrot-pki init-ca --name bitrot-testing
scripts/djbod-pki.sh --dir ./bitrot-pki node worker-a 10.0.0.1
scripts/djbod-pki.sh --dir ./bitrot-pki node worker-b 10.0.0.2
scripts/djbod-pki.sh --dir ./bitrot-pki node worker-c 10.0.0.3
scripts/djbod-pki.sh --dir ./bitrot-pki client controller
target/release/djbod-bitrotter fingerprint ./bitrot-pki/controller.crt
```

Keep the CA private key on the issuing machine. Install the CA certificate
and each worker's own certificate/key on its node, and the controller's
certificate/key on the coordinator. Private keys must have restrictive
permissions, such as `0600`. The worker certificate must cover its endpoint
IP, or the `server_name` configured by the coordinator. Workers check the
SHA-256 fingerprint of the controller's leaf certificate, in addition to
verifying its certificate chain. Restart workers after changing an allowlist.

Example `worker.toml` on node A; replace IDs with the product's actual
node and cluster UUIDs, paths with its test device roots, and the fingerprint
with the command's output:

```toml
node = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa"
cluster = "cccccccc-cccc-4ccc-8ccc-cccccccccccc"
listen = "0.0.0.0:6666"
devices = ["/mnt/test-disk1", "/mnt/test-disk2"]
journal = "/var/lib/djbod-bitrotter/worker.jsonl"
allowed_controllers = ["REPLACE_WITH_64_HEX_DIGIT_CONTROLLER_FINGERPRINT"]

[tls]
ca = "/etc/djbod-bitrotter/ca.crt"
cert = "/etc/djbod-bitrotter/worker-a.crt"
key = "/etc/djbod-bitrotter/worker-a.key"
```

Create the journal's parent directory first, outside every device root.
Start the worker under an account that can read and write only the intended
test devices and its journal:

```sh
djbod-bitrotter-worker --config /etc/djbod-bitrotter/worker.toml
```

Paths in either TOML configuration are relative to its containing directory
unless absolute. Device and journal paths must not traverse symlinks or `..`.
No two workers may open the same device. Starting a worker displays the
testing warning but does not authorize any corruption.

## Plan centrally

Create `workers.toml` on the coordinator. List each participating product
node's actual UUID and worker endpoint:

```toml
[tls]
ca = "bitrot-pki/ca.crt"
cert = "bitrot-pki/controller.crt"
key = "bitrot-pki/controller.key"

[[workers]]
node = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa"
address = "10.0.0.1:6666"

[[workers]]
node = "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb"
address = "10.0.0.2:6666"

[[workers]]
node = "dddddddd-dddd-4ddd-8ddd-dddddddddddd"
address = "10.0.0.3:6666"
```

An optional `[product_tls]` table with `ca`, `cert`, and `key` configures
read-only discovery through a TLS-enabled product cluster. It is separate
from mandatory worker TLS. Optional `server_name` on a worker overrides
the TLS name derived from its endpoint. A hostname without a port uses 6666;
use `[IPv6-address]:6666` for IPv6 endpoints.

```sh
djbod-bitrotter plan \
  --bootstrap-node 10.0.0.1:5263 --workers ./workers.toml \
  --key test/object --shards 2 --seed 42 --out ./bitrot-plan.json
```

Planning reads the authoritative metadata record through `djbod-client`,
then verifies physical record copies and shard framing through workers.
It never changes device data. All `k+m` devices for every selected version
must be covered and reachable, even when only one shard will be damaged.
Incomplete listings, inconsistent metadata, or impossible counts fail
preflight; there is no fallback to another node's unrelated files.

Repeat `--key` for multiple keys, use `--prefix test/` for a prefix, or
omit both to list all current objects. Empty objects are reported and
skipped. `--shard-index` can be repeated to restrict the eligible indices;
there must be at least `n` distinct valid indices for every selected version.
For example, `--shards 2 --shard-index 0 --shard-index 4` pins those indices
for every compatible object. `n` must be between 1 and that version's `k+m`.

The saved JSON pins cluster identity, worker endpoints, worker journal
identities, device roots, object versions, placement revisions, each scheme,
and exactly `n` distinct indices per object version. Its content hash is the
plan ID. Identical inputs and seed yield identical selection and event order.
Formatting changes to JSON are harmless; changes to plan contents require
a new ID and fresh confirmation. Plan output files must not already exist.

## Confirm and run

```sh
djbod-bitrotter run --plan ./bitrot-plan.json \
  --events 10 --interval 1s --journal ./bitrot-run.jsonl
```

The command displays the plan and warning on stderr, including under
`--json`. Interactive execution requires typing:

```text
DAMAGE TEST DATA <complete-plan-id>
```

If **any** selected version has `n > m`, it also requires:

```text
ACCEPT CERTAIN DATA LOSS <complete-plan-id>
```

This includes any positive `n` for an `m=0` version. A wrong answer, EOF,
or the 60-second prompt timeout causes refusal before mutation. An
unattended run must explicitly supply `--confirm-test-damage <plan-id>`
and, when applicable, `--confirm-data-loss <plan-id>`. There is no generic
yes flag. Workers independently verify the complete global plan and
acknowledgements, not just their local share of the damage.

An event selects one object version (round-robin), one deterministic stripe,
and one payload bit in each of its fixed `n` shard indices. All selected
shards refer to the **same stripe across the cluster**. They need not live
on the same node. The fixed set of indices is reused throughout a run and
on resume; a failed request never causes an additional shard to be chosen.

Before a fresh event, the coordinator checks every shard of that stripe.
Existing damage skips the event. It prepares all selected workers before
recording an apply decision. A completed event requires exactly the selected
`n` bad blocks, `k+m-n` intact blocks, and unchanged placement. Each mutation
changes one payload bit and leaves metadata, headers, footers, file lengths,
and stored checksums untouched.

Scheduling options:

| Option | Behavior |
| --- | --- |
| No scheduling flag | One event |
| `--events N` | At most N events, including skipped attempts |
| `--duration 30s` | Stop scheduling after elapsed wall time, including downtime on resume |
| `--continuous` | Continue until interrupted or the mutation budget is exhausted |
| `--interval 1s` | Positive delay between events; also accepts `ms`, `m`, and `h` |
| `--max-mutations N` | Reserve n byte changes before each apply decision; partial events keep their reservation |
| `--json` | JSON Lines events and final summary on stdout; warnings remain on stderr |

The first three scheduling choices are mutually exclusive. Duration must
be at least one second. The mutation budget never starts an event that
could exceed the remaining allowance. Repeated events may select a stripe
already damaged by the run, in which case it is skipped until repaired.

## Interruption, uncertainty, and resume

SIGINT/SIGTERM stops new scheduling; an event whose apply decision is already
durable is allowed to finish and report. A clean coordinator interruption
returns exit code 130. Other failures return a nonzero status and retain
the journal. A distributed mutation cannot be atomic: failure after apply
begins may leave fewer than `n` changes. The tool reports partial or uncertain
outcomes and stops new damage. It does not attempt rollback.

Keep the plan and both coordinator and worker journals. Each intent is
synced before the byte is written, and each result is synced before its
acknowledgement. Operation IDs survive restarts. Retries reconcile the same
file identity, byte, and block checksum; they do not XOR blindly. An
already applied operation returns its recorded result even if repair has
since replaced that shard. A torn final journal append is discarded on
recovery; a malformed complete record is an error. Lost/replaced worker
journals invalidate the old plan, rather than grant permission to replay it.

```sh
djbod-bitrotter run --plan ./bitrot-plan.json \
  --journal ./bitrot-run.jsonl --resume
```

Resume repeats the warnings and confirmation and retains the original run
limits. Do not pass new scheduling flags. An interrupted event without a
durable apply decision is cancelled. An event with a decision reconciles
with its original workers before any more work. An unresolved intent blocks
other runs on that worker, including after its controller lease expires.
Do not delete journals to bypass this check. If a file has changed so that
the intent cannot be reconciled, retain the audit files and recreate the
disposable fixture to start a new test.

Worker journals include the authorized plan and per-operation phase,
run/event IDs, selected device/path/index, stripe, offset, bit mask, original
and changed byte, checksums, and file identity. Coordinator journals record
limits, consent, baseline/final probes, apply decisions, outcomes, and errors.
All records have timestamps. Journals are locked, must stay outside device
trees, and are an audit trail rather than an undo mechanism.

## Interpreting results and limits

For a verified exact-count test, pause repair, moves/drains, re-encoding,
overwrites, and deletion of targeted versions while applying and verifying
the event. Ordinary reads and unrelated-object traffic may continue.
Metadata and file identity are rechecked, but descriptor-based access
cannot freeze concurrent product activity. A change or ambiguous outcome
stops the run; create a new plan for a new version or placement.

For an otherwise healthy 3+2 object, two corrupted blocks in one stripe
remain reconstructable. GET reports reconstruction of damaged data shards
and returns the original bytes, but leaves the disk corruption in place.
Parity-only corruption may be invisible to GET. Use the ordinary product
scrub to detect it and explicit repair to restore the files. Three corrupted
blocks in that stripe are unrecoverable from that shard set. Independent
bitrot after preflight can make even an n <= m test unrecoverable.

The tool currently targets current, nonempty versions in the `default`
bucket. Its adapter accepts blocks up to 256 MiB and footer checksum tables
up to 64 MiB. Plans and protocol messages are limited to 8 MiB; split large
scopes into smaller plans. Journals and operation history are retained in
memory as well as on disk, so bound long campaigns and archive completed
runs before starting a fresh campaign with new plans/journals. There is no
automatic journal compaction or automatic worker installation.
