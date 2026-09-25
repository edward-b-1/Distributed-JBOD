## [d0b8133] - 2026-09-25

Pull request #211: Status reports an unreachable node instead of failing (SPEC 19.1.3, 5.6)

### Fixed

- `Status` reports a node it cannot reach with `reachable: false`, the reason, and no build, since there was no `Hello`, and lists that node's devices from the document as unavailable with no space, instead of failing the whole request on the first unreachable node. From the cluster's side that is what they are, so the same rows and the same rule apply as for a disk its node cannot read (SPEC 19.1.3, 5.6) (#209).
- `djbod status` exits 0, marks those rows `active, unavailable` and prints the node and the reason on stderr, with `--json` carrying the fields. A node holding a different document version is still reported as `DocumentVersionMismatch` rather than as unreachable, and every other operation keeps the fail-stop broadcast.

## [17dd946] - 2026-09-25

Pull request #207: Web UI: take a list of bootstrap nodes and fall back to the cluster's other nodes

### Added

- `--bootstrap-node <ADDR[,ADDR...]>` with `DJBOD_BOOTSTRAP_NODE`, connection endpoints tried in order and never a filter on what an operation covers. `djbod-ui` accepted exactly one node address and used it for every request, so when that node stopped the whole page went dark although the other nodes were serving (#176).
- A fallback order: the node that answered last, then the configured addresses in order, then every address the cluster document lists, each once. The learnt addresses are refreshed every time the document is fetched, which the page does on every refresh, so an interface started against one node keeps working when that node is stopped, restarted or retired.
- `via` on `/api/status`, the address the answer came through, shown in the header's node tooltip, with the startup line listing every configured address and the 502 listing every address tried with its reason and an `addresses` field carrying them on their own.

### Changed

- `--node` and `DJBOD_NODE` remain as the deprecated spelling and print a notice on stderr, with the new spelling winning when both are given.

## [0cb7a49] - 2026-09-25

Pull request #208: Web UI: drop 'The page retries every 30 seconds' from the unreachable banner

### Changed

- The unreachable-node banner names the message and says to start the node or start `djbod-ui` against another node; the retry cadence is not the reader's concern.

## [e9241a2] - 2026-09-25

Pull request #206: Web UI: name the address and say why when the node cannot be reached

### Added

- `ApiError::Unreachable` for a failed connection attempt: the 502 keeps the code `node_unreachable` and carries a message naming the address and the reason in plain words, the address on its own, and the operating system's own text as detail. Connection refused, reset, aborted, timed out, host or network unreachable, network down, address not available and permission denied are worded plainly, and anything else keeps the system's text without its error number. The page had shown the wire layer's text verbatim as a toast on every failed call, without saying which address was tried (#177).
- One critical banner for such an error, with the raw detail as its tooltip, no dismiss button, and clearing itself on the next successful call, while other errors still toast.

### Fixed

- The transport banners no longer assume `plain` before the first status has been fetched.

## [79dac71] - 2026-09-25

Pull request #204: remove-device --force: retire a device that cannot be drained (SPEC 18.2.1.1)

### Added

- `djbod cluster remove-device <device> --force [--yes]`, for a device the cluster cannot read or one the administrator has given up on. It reports the device's state and whether its node can read it, says what marking it removed means, warns when fewer than k+m active available devices would remain, asks for the device id to be typed back unless `--yes`, and proposes the document with the device removed. It is an ordinary unanimous proposal with no reference scan, because a scan of every record would take as long as the scrub that follows (#170).

### Changed

- Repair treats a shard or record copy on a device listed as removed exactly as one on a device dropped from the document, so the cross-node scrub finds every version that lost a copy and `scrub --repair` relocates each shard, with no second implementation of that logic in the command. A removed device is never written to or cleaned, and is no longer scrubbed or flagged unavailable, since it is retired (SPEC 18.3, 18.2.1).

## [3668032] - 2026-09-24

Pull request #200: A read reconstructs around a shard it cannot open, and reports ranges of stripes (SPEC 11.4)

### Changed

- A data shard that cannot be opened at all, because its file is missing, truncated or unreadable, or because its device is unavailable, has left the document, or its node cannot be reached, is treated as erased from the first stripe, so parity is opened at once and the read succeeds whenever at most m shards are out. To a read these are one kind of failure, one shard's share of a stripe that k others can supply, and a corrupted block in the same shard was already reconstructed. Nothing is written (#199).
- More than m shards out is refused before any block of the body, with the first shard's own code, since a node that is down and a file that is gone call for different actions, and the message lists every shard that could not be read. A node that answers wrongly is still an error.
- A `Reconstruction` entry is one shard, one fault and a range of stripes, so a shard that could not be opened is one entry however large the object, instead of one per stripe. `FaultKind` gains `Unreadable` for a file that will not mend itself and `Unavailable` for a device or node that could not be reached, which may be temporary, and `djbod get` says "unavailable, perhaps for now" for the transient kind. A read cannot tell a disk that died from one unmounted for a minute, so it does not try; repair already draws the same line.

## [40b0938] - 2026-09-24

Pull request #185: A device that becomes unreadable while the node runs is unavailable, and no write rebuilds it (SPEC 5.6)

### Changed

- A device whose identity file can no longer be read is unavailable. The space report behind `status` and the scrub look for it with one `stat`, since that is the only thing that tells an empty mount point from a disk, while writes and listings do not check first: a write makes the partition and key directories with a plain `mkdir` and treats a missing parent as the device being unavailable, and a listing treats a missing bucket the same. There is therefore no window between a check and the act, and no write ever recreates a device's tree on whatever filesystem sits at its path. The objects tree is made by `initialise` and by nothing else (#173).
- A write to the device is refused with `DeviceUnavailable`, so a repair no longer recreates the dead path, and `contents`, `drain`, `remove-device` and the removal scan give a clear refusal instead of an I/O error.
- The scrub reports an unavailable device once and does not expect a record copy from it in the cross-node checks, so the versions naming it are no longer each reported as `RecordsInconsistent`. It is not counted as damage, so the run is incomplete and the summary says how many devices went unchecked and that the remedy is to restore or retire them (SPEC 20.1.2.3).
- The node logs the loss once when first seen and once more when the device is readable again.

## [7da5ff6] - 2026-09-24

Pull request #188: README: how failures show up, and where the line is drawn

### Added

- A `README.md` section stating the intended use, one person's data on a few aging machines, and the priorities that follow: durability, detection, then the availability the coding buys, up to m disks. It says how reads and writes behave now, and states plainly that there is no alerting subsystem and none is planned, since the result of the operation carries the warning or the error, `status` and the web interface show the same on demand, and a scrub from cron exits non-zero.

### Changed

- The line the `README.md` draws, in place of the earlier "opposite of high availability", which the reconstruction work made untrue: the operations are forgiving, the cluster is not self-managing, and nothing moves data or changes the cluster without a person deciding.

## [028c4f2] - 2026-09-24

Pull request #186: A node starts without a device whose path is missing or empty (SPEC 5.6)

### Changed

- A configured path whose directory is missing, or which holds neither identity file nor objects tree, no longer stops the node from starting. The node logs the path and the device it should have held and reports every device the document lists for it that no path opened as unavailable, because a disk failure is a device failure and not a node failure. A directory with an objects tree but no identity file is still refused (#173).
- `LocalStatus` and `Status` carry `available` per device, `djbod status` prints `active, unavailable` with no space and counts them on stderr, and placement leaves an unavailable device out as it leaves out a full one, so a write goes ahead if k+m devices have room and is refused with `InsufficientDevices` naming the unavailable devices only if not. Re-placement, repair targets and the drain estimate do the same (SPEC 19.1.3).
- A write is one placement from the space report and one attempt: a refusal fails it and is not retried on another device (SPEC 10.7).

### Added

- The devices placement went around, carried in the `PutObject` response beside the version, as a read's terminating status lists what it reconstructed. `put` and `put_from_reader` return an `ObjectWrite`, the Python client raises a `DegradedWrite` warning, `djbod put` prints them on stderr and exits 2, and the web interface's answer and `djbod put --json` carry them.

## [31e8d23] - 2026-09-24

Pull request #197: GET reconstructs a damaged block from parity, reports it, and repairs nothing (SPEC 11.4)

### Changed

- A read that meets a data block failing its checksum no longer fails: the coordinator opens the parity shards from that stripe on, decodes the stripe from any k good blocks with the decoder the repair path uses, verifies it, delivers it, and keeps reading to the end of the object. Nothing is written to any device. A stripe with more than m unusable blocks is still refused with `BlockChecksumMismatch` naming the stripe, and the whole-object checksum is verified over the delivered bytes as before (#196).
- The terminating `StreamEnd` carries a required `reconstructed` list, each entry the stripe, shard index, device and fault. It is the frame a client must read before trusting the body, so there is no node state and no second request (SPEC 11.7).
- `get` and `get_to_writer` in the Rust client return an `ObjectRead` holding the record and the list, the Python client raises a `DegradedRead` warning and carries the list on `ObjectInfo`, `djbod get` writes the correct bytes, prints each reconstructed block on stderr with the note that `djbod repair <key>` fixes it, and exits 2, and a download in the web interface completes instead of failing part way while the key is noted as damaged until a repair.

## [1cb6d05] - 2026-09-23

Pull request #179: djbod status: builds of the answering node and of every node; the build is required on the wire

### Added

- A header line in `djbod status` giving the build of the node that answered, from its `Hello`, which the client already holds for the connection that served the request, and naming the client's own build when it differs (SPEC 19.1.5) (#178).
- A `NODE BUILD` column on every device row, since each node reports its build in `LocalStatus` and the coordinator passes them on in `Status` as a list of nodes, with `--json status` gaining the answering node's build and the node list, carried too by the Rust client's `Status`, the Python `Status` and `/api/status`.

### Changed

- `LocalStatus.build` and `NodeStatus.build` are plain strings and `Status.nodes` has no default (SPEC 19.1.5.2) (#180).

## [e792d8e] - 2026-09-23

Pull request #184: Fields that were optional only for older builds are required

### Changed

- `Status.transport` and `LocalStatus.tls_ready` are required on the wire, and `max_key_bytes`, `max_object_bytes`, `max_user_metadata_bytes` and `transport` are required in the cluster document; all were defaulted when absent only for documents and messages written before they existed. Every `init-cluster` since those fields were added has written them (SPEC 6.2, 19.1.5.2) (#181).
- SPEC 6.2.2 no longer says a document without the limits or transport means the defaults, and 6.2.6.4's rolling-upgrade rule rests on every field being required rather than on absent fields meaning defaults.
- The cluster name, node and device labels, `GetMeta.shard_present`, the paging cursors, `content_type`, `user_metadata`, `ErrorDetail`'s context, `ListQuery`, `MoveShard.to`, the scrub rate caps, `StreamEnd`, `ShardRepair.relocated_to`, `NodeDocument.build`, the `RepairReport` vectors and the `NodeConfig` defaults stay optional, because absence is a legitimate value for each.

## [f5a98e2] - 2026-09-23

Pull request #183: The build is a required field on the wire

### Changed

- `Hello.build` is a plain string. Every binary has a build and every node and client sends it; the option existed only so that a message from a build predating the field could be read, and there is no installed base to do that for. SPEC 19.1.5.2 states the rule: a field is optional only when its absence is a legitimate value (#180).
- `NodeDocument.build` in the client library stays optional, since it is absent when the node could not be reached, and `cluster show` prints a dash for it as before.

### Removed

- The "older, unreported" wording in `cluster show` and in the web interface's node table and health banner, the option on `Identity.build` in the Rust and Python clients, and the command-line fallback that recovered a cluster id from an old node's refusal of the nil-id ask.

## [3c3fd58] - 2026-09-23

Pull request #134: Keep command-line tables aligned for long labels and values

### Fixed

- Command-line tables size every column across its header and all displayed rows before printing, so long labels, addresses, build strings, object sizes, recovery keys and revisions and certificate names no longer push later columns away from their headers. The fix covers `djbod status`, `djbod contents`, `djbod cluster show`, `djbod list`, `djbod-recover list` and `scripts/djbod-pki.sh list` (#132).

### Changed

- The Rust tools use `comfy-table` with its default features off, with a short `render` function in each tool applying the borderless style, two-space separators, right alignment for numeric columns and no trailing spaces, and the PKI helper collects its certificate fields before choosing widths. Full values, numeric alignment, JSON output and command behaviour are preserved, and the old minimum column widths go, since columns now size themselves.

## [dd84671] - 2026-09-23

Pull request #164: Scrub exit codes: four outcomes, four codes

### Changed

- `djbod scrub` has one exit code per outcome in place of the single code 2 for both damage and an unfinished run: 0 complete and nothing wrong, 2 complete with damage, 3 incomplete with no damage seen, and 4 incomplete with damage seen. Incomplete means a stream that ended naming a node that could not be scrubbed or checks that stopped; a stream that ends only because repairs failed is a complete run (SPEC 20.1.2.3) (#156).
- The last line of the human output states the verdict in words after the counts, with `--json` printing the events alone and the exit code carrying the verdict. The mapping is one function with the table as a test.

### Fixed

- The events were not counted at all in JSON mode, so `--json scrub` always exited 0; they are counted now, whatever the output mode.

## [d5b2333] - 2026-09-23

Pull request #162: The cross-node scrub as a merge of per-device record streams

### Changed

- The cross-node phase is a merge of one paged record stream per device, opened all at once and held for the phase, with heads merged in key hash and version order and each group of equal heads judged as one version's copies across the cluster, giving the same findings as before. There is no lookup, no probe and no key list; memory is one page per device plus the current group, and connections are one per device rather than one per key (SPEC 15.2.2, 15.2.3) (#152).
- A stream that cannot be opened, or that fails mid-way, stops the merge after the group in hand, and `CrossCheckStopped` carries the versions checked with no count of the remainder, which cannot be had without a second pass.

### Added

- `shard_present` on each streamed record, gathered with one `stat` as the page is built.
- `CrossCheckProgress` every 10,000 versions, printed by the command on stderr and shown as a count in the web interface's progress text.

### Removed

- `DeviceForShardNotInClusterDocument`, which turns out to be unreachable: a device the document no longer lists has no stream, so its copy is absent and the version is `RecordsInconsistent` first, which is what it is, and repair rebuilds the shard elsewhere from that.

## [50c559e] - 2026-09-23

Pull request #163: LocalRecords pages in key hash order and reads only what it returns

### Changed

- `LocalRecords` pages by key hash and version, the order a device's directories are already in, and `Device::walk_records_from` starts its walk at the cursor's directory and stops when the page is full, so a page costs the records it returns. Before this, each page sorted by key text and so walked and parsed every record on the device: one with 250,000 records was walked about seventy times to be streamed once, which `contents`, the drain's estimate and the removal scan all paid for (#121).
- SPEC 15.2.2 states that a page must cost no more than the items it returns, and 15.2.2.1 records why the page is 8 MiB with the arithmetic.

### Added

- A flag on each walked record saying whether the device has its shard file, carried within the device layer for the scrub merge to use.

## [48eb00e] - 2026-09-23

Pull request #159: Retire "holder": say device, or node, throughout

### Changed

- `SPEC.md` says device, or node, throughout. The word "holder" was used about forty times and never defined, meaning sometimes the device a record lists for a shard and sometimes that device's node, while "device" already means a disk everywhere in the project.
- The coordinator's `Holder` struct, an open shard write to one device with its connection and request id, is `ShardWriter`, with `stream_body_to_holders`, `abort_holders` and `holders_new_first` following, and the scrub finding `ShardMissingOnHolder` is `ShardMissingOnDevice`, which changes that finding's JSON name in `--json` output and in the event stream.

## [0815ec4] - 2026-09-23

Pull request #158: Scrub: a node that cannot be asked is unchecked, not damage

### Changed

- A node that cannot be reached, or that refuses or answers out of protocol, no longer produces a finding in the cross-node phase. Every key needs every node, and so does listing the keys, so the first such node ends the phase with one `CrossCheckStopped` event naming the node and the reason and saying how many keys were checked and how many were not. Findings made before that point stand, and a key that could not be checked is never queued for repair (#153).
- The run ends as incomplete with `NodeUnreachable` whenever a node could not be scrubbed or the checks stopped, whether or not damage was also found, and the message carries all three counts.
- The first failure to reach a node is reported, with no retry.

### Removed

- `HolderUnavailable`, which mixed a node that could not be asked, now unchecked, with one real-damage case: a record naming a device that is no longer in the cluster document. That case becomes `HolderNotInDocument`, with the shard index.

## [33ec9fd] - 2026-09-23

Pull request #150: Keep the palette exploration under docs/brand

### Added

- `docs/brand/exploration`, the comparison sheets behind the current colours, kept because the files are cheap and the reasoning in them is expensive to reconstruct: the mark on the interface's own surfaces in four colour pairs, showing why `--serious` was rejected as too washed out to carry the parity diagonal; five yellows, showing why the brightest starts losing contrast against the light surface; flat against glow against a lit-dome gradient, which was dropped as too subtle for its complexity; the glow at 64, 48, 32 and 16 px; and the throwaway `stack()` variant the optional filter parameter grew from.

## [be4ed14] - 2026-09-23

Pull request #149: Remove the stale device.json name; the identity file is DISTRIBUTED-JBOD-DEVICE.json

### Removed

- `layout::DEVICE_IDENTITY_FILE`, which still named `device.json`. Nothing referenced it, but `layout` is public and is where the other on-disk names live, so a new caller could have picked it up and then failed to find any device. The one remaining definition is `device::DEVICE_IDENTITY_FILE`, `DISTRIBUTED-JBOD-DEVICE.json` (#148).

### Fixed

- The uppercase name corrected in the `layout` module's diagram, the `DeviceId` doc comment, the SPEC 9.1 on-disk layout diagram and the milestone 2 note.

## [d9601b7] - 2026-09-21

Pull request #120: The client's last three methods: move_shard, scrub, drain

### Added

- `Client::scrub` and `Client::drain`, returning an `EventRun` that owns the connection for the run and yields events through `next_event()` until the stream's end, whose error says whether the run failed, after which the client reconnects for whatever follows.
- `Client::move_shard`, returning a `MoveShardReport` with the record at its new revision, the source device, whether the source copy was cleaned and whether the shard was rebuilt rather than copied.
- The same three on the blocking facade, whose `EventRun` blocks on each event using the client's runtime.

### Removed

- The raw-connection seam: `raw_connection` in the tool and `into_connection` in the library are gone and `Client::connection()` is private, so the tool touches nothing below the client API. Re-encode uses two clients for its concurrent read and write instead of two bare connections.

## [27d82c7] - 2026-09-21

Pull request #119: djbod contents: what each device holds, counted from its records

### Added

- `DeviceContents`, a client-facing operation that fetches a device's record copies from its node page by page and reports the versions with a shard on the device, the distinct keys, the blocks and the shard bytes computed from each record's size and scheme. No shard is read, so it is a directory walk on one node rather than a scrub, and an unknown device is `NotFound` (SPEC 19.1.3).
- `djbod contents [<device>...] [--node-id <node>]`, printing device, label, node label, state, versions, keys, blocks and shard bytes, covering every device that is not removed when given no arguments, accepting a UUID or label, saying on stderr how many devices hold nothing, and giving the rows under `--json`.
- `Client::device_contents` in the Rust library, in the blocking facade too, and `client.device_contents(device)` in Python.
- SPEC 18.2.3 stating why records rather than free space are the measure, with 18.2.1 listing the command among the drain steps. `djbod status` reports each device's bytes from `statvfs`, which counts the whole filesystem and so cannot say whether a device is empty.

## [6f4d6a3] - 2026-09-21

Pull request #117: Error names by the workspace convention: ConnectionError and ClientError

### Changed

- `connection::ClientError` becomes `ConnectionError`, what one connection can meet, and `client::Error` becomes `ClientError`, what the client API can meet, whose `Connection` variant wraps the former. The client crate's error types are now `WireError`, `TlsError`, `ConnectionError`, `ClientError` and `AdminError`, one per module.

## [e893ec2] - 2026-09-21

Pull request #116: Error names by the workspace convention: AdminError and MembershipError

### Changed

- The client's `membership` module is renamed `admin`, since administration is what it holds, and its error becomes `AdminError`, following the workspace's `<Thing>Error` convention.
- The node's `membership` module keeps its name, since it now holds exactly this node's own membership, and its error takes the plain `MembershipError` back, with an `Admin(AdminError)` variant for the network step and the rest the node's own.

## [06f318f] - 2026-09-21

Pull request #115: Administration in the client library

### Changed

- The purely network membership procedures move from `djbod-node` to `djbod_client::membership`: every change to the cluster document and everything built on them, including `propose`, `sync`, the `set_*` procedures, `resolve_device`, `resolve_node`, `remove_device`, `remove_node`, `scan_references` and the forced-removal pair (SPEC 6.2.6). The node keeps only what touches its own state directory: `join`, `add_devices`, `adopt_from_peers`, the startup address adoption and `connector_for`.
- `MembershipError` in the client is what a client can meet, with the node-only variants gone and a `Document` variant replacing the old detour through `NodeError::InvalidDocument`, while `djbod_node::membership::LocalMembershipError` wraps it plus the node's own.
- `djbod-cli` and `djbod-ui` depend on `djbod-client` alone, so the crates line up as core, proto, client, node and the front ends.

## [89edbfd] - 2026-09-21

Pull request #114: djbod on the client library: a node list, failover, no raw requests

### Changed

- `djbod` is built on `djbod_client::Client`, retiring the last duplicated client code. `--node` and `DJBOD_NODE` take a comma-separated list of addresses tried in order, with the library's rule about what may be retried, and one address behaves as before.
- Every command uses the client's methods rather than building raw requests and matching on responses, and the tool's own paging loop is gone. The event streams, `move-shard` and the re-encode pipe take a connection from the client through `Client::into_connection`.
- The `cluster` commands still take one peer address but use the node the client reached, so a dead first address is skipped there too.
- `get-cluster-id` and `identity` ask each configured node in turn, and when a node runs a build from before the nil-id ask, its refusal names its cluster, so the id is read from that message, printed as usual, and a note on stderr says the node wants upgrading (SPEC 19.1.5.1).

## [84f2bb5] - 2026-09-21

Pull request #118: README: the system runs on three machines

### Changed

- The `README.md` status section says that a cluster of three machines with six disks between them is running, in place of the statement that the system had not yet run for long on several real machines, and keeps a caution since the software is young.

## [458a4e8] - 2026-09-20

Pull request #112: The djbod Python package: the blocking client wrapped with PyO3

### Added

- `crates/djbod-python`, wrapping `djbod_client::blocking::Client` with PyO3 and built by maturin as an `abi3` extension module, so one wheel serves Python 3.10 and later.
- `Client(nodes, cluster=None, tls_ca=None, tls_cert=None, tls_key=None)`, taking what the `djbod` command takes, with failover and cluster-id discovery from the Rust client. Every method blocks and releases the interpreter lock while it waits on the network, and `put_file` and `get_to_file` stream with one chunk in memory.
- Exceptions defined in Python so they subclass naturally: `Error`, `Unreachable` with the addresses tried, `NodeError` with the code and detail as the node reported them, and `NotFound` as its subclass, with bad inputs raising `ValueError`.
- Results with structure of their own returned as plain dicts with the JSON field names, so what the command prints and what Python sees are the same, with `ObjectInfo`, `KeyEntry`, `ListPage`, `Status` and `Identity` as small classes, plus type stubs and `py.typed`.
- `scripts/python-tests.sh`, which builds the node, makes a virtual environment, runs `maturin develop` and runs a pytest suite against a real `djbod-node`.

## [ebc4340] - 2026-09-20

Pull request #111: The Rust client API: node addresses with failover, one method per operation

### Added

- `djbod_client::Client`: `ClientOptions::new` takes several node addresses tried in order, since any node answers any request, with the cluster id given by `.cluster(id)` or learned from the first node that answers, and Transport Layer Security set by `.connector(...)` (SPEC 19.1.5.1).
- Failover over one kept connection: when it fails, the next request goes over a fresh connection to the next address, and only requests safe to repeat are retried on the client's own initiative, which is reads, `head`, listing, `status` and the document. A write, a delete, a repair or a refusal by the node is never repeated.
- One method per operation: `put` and `put_from_reader`, `get` and `get_to_writer`, `head`, `delete`, `list` with `ListPage::next_start_after` or `list_all`, `repair`, `status`, `identity` and `cluster_document`.
- `Error::detail()`, giving the node's `ErrorDetail` when the node answered with one, `is_not_found()` for the common case, and `Unreachable` listing every address tried and why it failed (SPEC 16.2).
- `djbod_client::blocking::Client`, the same client without `async`, running on a current-thread runtime it owns and using `std::io::Read` and `Write` for the streaming methods.
- SPEC 20.8 stating the library's obligations and 20.8.1 the bindings plan, with the protocol remaining the contract for native clients.

## [dfb98ce] - 2026-09-20

Pull request #109: UI: a dark theme switch in the header

### Added

- A dark theme switch at the right of the header, a moon reading "Dark" on the light theme and a sun reading "Light" on the dark one, remembered per browser and applied by a one-line script before the first paint so there is no flash. With nothing chosen the system's preference applies as before, and the tooltip says so.

### Changed

- The dark tokens gain a `data-theme="dark"` selector beside the media query, `color-scheme` follows so form controls and scrollbars match, and the header's lockup is swapped by script rather than by a media query, which cannot see a manual choice.

## [75cdbf2] - 2026-09-20

Pull request #110: Extract the djbod-client crate

### Added

- `crates/djbod-client`, holding the code that had lived inside `djbod-node`: `wire`, frames over tokio streams; `connection`, the `Hello` exchange, requests, responses, the streaming object operations and the node-to-node shard transfers, which are the same conversation; the client half of `transport`; and `build.rs` with the `BUILD` constant, since the build id travels in `Hello`. The crate depends on `djbod-core`, `djbod-proto`, tokio and rustls alone, so a client program no longer pulls in the server, clap, toml or tracing.

### Changed

- The node's `transport` keeps `TlsMaterial`, its own certificate which both accepts and connects, and `accept`, and re-exports the client half those are built on. Everything else in the node uses `djbod_client` directly, with no other re-export shims.

## [7da535a] - 2026-09-20

Pull request #108: Yellow parity slabs everywhere; the glowing status-light lockup in the web UI's header

### Changed

- The web interface header uses the status-light one-line unhyphenated lockup, light and on-dark, whose yellow parity slabs carry the glow filter, and everything else uses the plain yellow kit: the tab favicon as SVG and 32 px PNG, the application icon, and the `README.md` lockup in both schemes. The routes and build-versioned paths are unchanged, so browsers pick the new files up without a hard reload.

## [05c09ca] - 2026-09-20

Pull request #102: Match the logo blue to the UI accent, with yellow and status-light alternates

### Changed

- The mark uses the web interface's `--accent` in both tones, in place of the cyan-leaning sky blue chosen before there was anything to match, so the logo and the running application are no longer visibly different blues. The brand `README.md` records where the values come from so the two cannot quietly drift again.

### Added

- Two parity-colour alternates beside the orange in use, all three shipped so they can be judged side by side with a proof sheet showing them on both surfaces: a yellow set using the interface's own `--warning`, and a status-light set that brightens it and puts a soft glow behind the parity slabs so the diagonal reads as a row of lit indicators. The glow holds down to 16 px and below about 32 px simply warms the colour.
- Palette, glow and output directory taken from the environment in `build.sh`, so every variant comes off one code path, with `gen.awk`'s `stack()` gaining an optional filter id that leaves the output byte-identical when unset.

## [32ca228] - 2026-09-20

Pull request #107: UI: the pagination key on its own line, and no 'more follow'

### Changed

- The paging row says only how many keys are on this page, since the Next button already says whether more follow, and the cursor moves to a line of its own beneath the row, empty on the first page.

## [6a062f3] - 2026-09-20

Pull request #106: UI: the key table scrolls in its own box and pages with Previous and Next

### Changed

- The key table scrolls in its own box rather than with the window, with its header held in place, taking the height left under the controls.
- The list pages 100 keys at a time with Previous and Next in place of a More button that appended keys without end, showing the page number, how many keys the page holds and whether more follow, and the key the page starts after, which is the node's own paging cursor (SPEC 15.2.1). Going back reuses the cursors already seen, so Previous is exact, and changing the prefix or pressing List returns to page 1.

## [f80202a] - 2026-09-20

Pull request #105: djbod get-cluster-id and identity: ask a node who it is

### Added

- A handshake in which a client sends the nil universally unique identifier (UUID) as its cluster id to ask who is there; a node accepts that from a client only, never from a node, answers with its own `Hello` carrying the id, the cluster's name and its build, and closes the connection. Nothing else is served to a client that did not name the cluster, so a client pointed at the wrong cluster still fails before it can act (SPEC 19.1.5.1).
- `djbod get-cluster-id --node <addr>`, needing no `--cluster` and printing the id alone so it can be captured in a shell variable, with `--json` adding the cluster name, the node and the build.
- `djbod identity --node <addr>`, which asks the same way, connects with the answer and fetches the document to say in words who is there: the cluster, the node and its address, the build, the document version and the transport.

### Changed

- The nil UUID is never a cluster id, and the document validator refuses it (SPEC 6.2.1). A node from before this change refuses the nil id as a mismatch, and its refusal message names the cluster it serves, so the id is learned either way.

## [ebf522e] - 2026-09-20

Pull request #104: UI: destructive buttons are filled red, styled like the primary button

### Changed

- Delete on an object, and Remove and Remove node on the Nodes tab, are filled with the page's red and white text in the shape of the primary buttons, so every button on the page is plain, primary blue or destructive red. Primary and destructive buttons darken slightly on hover, and a disabled Remove node keeps its faded look.

## [66f733a] - 2026-09-20

Pull request #103: UI: the rail's head has a title at the left and the toggle at the right

### Changed

- The top of the navigation rail is a head row carrying a small uppercase title at the left and the collapse chevron at the right, so the row is filled rather than a lone button. Collapsed, the title is hidden like the section labels and the chevron sits centred.

## [3b7b5d8] - 2026-09-20

Pull request #100: UI: the one-line unhyphenated lockup in the header

### Changed

- The header shows the one-line unhyphenated lockup, and its on-dark variant under a dark colour scheme, in place of the two-line horizontal one, at 48 px high and so 203 px wide, above the kit's 200 px minimum for the inline lockup. The `README.md` keeps the two-line lockup, which the kit names for that use.

## [e88bb62] - 2026-09-20

Pull request #101: Fix the title on the unhyphenated inline lockups

### Fixed

- The unhyphenated inline lockups carried the hyphenated `<title>`, so their accessible name contradicted the artwork. `inline_src` in `docs/brand/build.sh` had hardcoded the hyphenated title for all four inline lockups and now takes the title as an argument alongside the joiner, so the two spellings cannot drift apart again.

## [50c4936] - 2026-09-20

Pull request #98: docs: add six alternative logo concepts under docs/brand/concepts

### Added

- Six exploratory logo directions under `docs/brand/concepts`, none wired into the application: Mosaic Pool, Lattice, Stripe D, Bunch of Disks, Block Hyphen and Erasure D, each with standalone marks and lockups for light and dark, Inkscape exports, the editable design boards, and a `README.md` describing each concept, its accent colour and the shared type and colour system.

## [ddb12cc] - 2026-09-20

Pull request #99: Add one-line lockups, hyphenated and not

### Added

- One-line lockups in both spellings of the name, `Distributed-JBOD` matching the repository name and `Distributed JBOD`, each with a dark-surface variant, and a contact sheet of all four. The mark sits at 76 px against a 48 px setting, optically centred on the cap height rather than the em box, and the type is converted to outlines as with the rest of the kit.
- Generation of the new lockups in `build.sh`, so the whole kit still rebuilds from scratch with one command and a rebuild reproduces every pre-existing asset byte for byte, with the brand `README.md` gaining rows for the new files, a note on choosing between the inline and two-line lockups, and a minimum width of 200 px for the inline one.

## [ec9094c] - 2026-09-20

Pull request #97: Docs: the design comparisons behind the UI's choices

### Added

- The four comparison pages shown during the interface work, saved under `docs/design` so the decisions and the alternatives they beat are on record: how the sections are presented, how the page says whether nodes agree, how to get from the card to each node, and one family of status marks. Each page is self-contained HTML with no build step, drawn in the console's own colour tokens with light and dark side by side, and the `README.md` there lists the question and the decision for each.

## [617b7e2] - 2026-09-20

Pull request #96: UI: status marks are shapes, drawn in CSS

### Changed

- One family of status marks across the page, drawn in CSS from the existing `ok`, `bad`, `warn` and `state` classes: a dot for pass or active, a square for fail, a triangle for warning or draining, and a ring for off or removed, in green, red, amber and grey, so the shape carries the meaning as well as the colour.

### Removed

- The font glyphs used until now, which a font draws inconsistently, and the health card's typed check, exclamation and identity marks.

## [97da9e5] - 2026-09-20

Pull request #95: Use the brand kit: lockup in the header and README, favicon and app icon

### Changed

- The header shows the horizontal lockup at 34 px high, with the on-dark variant under a dark colour scheme, in place of the mark beside the typed name.
- The browser tab uses the 2 by 2 reduced favicon meant for 16 to 24 px, with a 32 px PNG as the fallback for browsers without SVG favicons, and the 256 px application icon as the touch icon for home screens.
- The `README.md` opens with the lockup, in its dark variant under GitHub's dark scheme, instead of the mark at the right.

### Removed

- The second copy of the mark under `crates/djbod-ui/icon`, since the files are embedded from `docs/brand` at build time.

## [354db81] - 2026-09-20

Pull request #94: Add a logo, brand kit, and new app icon

### Added

- `docs/brand`, holding the mark, the horizontal and stacked lockups for light and dark surfaces, one-colour variants, reduced marks for small sizes, an application icon and PNG exports from 16 px to 1024 px, with a `README.md` covering the palette, the clear space and the minimum sizes. The mark is four columns of pill-shaped slabs, where a column is a node and a slab a chunk of capacity, the columns are deliberately of different heights, and one orange slab per column steps diagonally across the array for the parity striped over every node.
- `docs/brand/gen.awk` holding the geometry and `docs/brand/build.sh` driving Inkscape to regenerate every asset, with the type converted to outlines so the SVG files render without Segoe UI installed.

### Changed

- The application icon is the new mark in place of the placeholder node graph, keeping the light and dark pair at 256 by 256 on transparent.

## [c1374bb] - 2026-09-20

Pull request #93: UI: the Reachable column says ping ok or ping failed

### Changed

- The nodes table's Reachable column reads "ping ok" and, on a failure, "ping failed" with the error, in place of "answers" and "unreachable". The check is unchanged: the interface server opens a connection to the node and fetches its document on every page refresh. No age is shown, because the check runs only on refresh and would always equal the refresh time in the header.

## [8085a6f] - 2026-09-20

Pull request #92: UI: one nodes table with every check and the node's devices

### Changed

- One nodes table combines the plain table and the closed health-checks table, with columns for the node, its addresses, whether it answers, its document version and what to do when it is behind, its build, its Transport Layer Security mode, and its devices with an indicator, the count, the free space and the active and draining counts. Each check cell is a pass, warning or fail mark with the value it judged and, when not a pass, why.

## [71ba68b] - 2026-09-20

Pull request #89: UI: a Nodes tab for the nodes and devices tables; the Overview is the summary

### Changed

- The Overview becomes the summary page, holding the health card and the tiles alone, with the nodes table and its Sync button and the devices table moved unchanged to a new Nodes tab between Overview and Objects. Its rail count shows the number of nodes, or the number off in red when any node is unreachable or at another document version.
- Nothing is shown in two places: the card summarises and the Nodes tab holds the detail, with the card's Sync link still running the same sync.

## [04f1a8d] - 2026-09-20

Pull request #91: UI: the Nodes and Devices tiles open the Nodes tab

### Changed

- The Nodes and Devices tiles on the Overview are links to the Nodes tab, with an arrow after the label and the accent outline on hover, opening on a click or on Enter. A tile becomes a link only when the tab that holds its detail exists on the page.

## [c2d6341] - 2026-09-20

Pull request #90: UI: a wider sidebar with a collapse toggle

### Changed

- The navigation rail grows from 184 px to 232 px, and a chevron at its foot collapses it to 60 px of icons with each section's label as a tooltip and the counts hidden, remembered per browser. On a narrow screen, where the rail is already a row above the content, the toggle is hidden.

## [102b696] - 2026-09-20

Pull request #83: UI: a health card at the top of the Overview

### Added

- A full-width health card leading the Overview and answering one question, whether all nodes hold the same configuration: green when every node listed in the document answered and holds the document's version; red naming the nodes that do not and the version they hold, with a Sync documents link, or naming the unreachable nodes with their addresses and errors; and amber when the nodes agree but their builds differ, saying to upgrade before the next change to the document (SPEC 6.2.6, 6.2.6.4).
- A Details link that scrolls to the Nodes table.

## [a823f69] - 2026-09-20

Pull request #85: UI: no document version in the header

### Removed

- The document version from the web interface header, since it is the version held by whichever node answered the status request, which the Nodes table reports per node and the health card judges.

## [ec4d2f2] - 2026-09-20

Pull request #88: README: a typical multi-node deployment

### Added

- A `README.md` section extending the quick start from one node with directories to the intended shape of one node per machine with its own disks: three machines with three disks each, through mounting the disks, the configuration file with the `listen` against `advertise` note, `init-cluster --name home-nas --k 4 --m 2` with what the scheme costs and tolerates, `join` on the others, running each under a service manager, and using the cluster from any machine.
- A "What to expect" ending: shards are placed per disk without regard to machine, so a machine that is switched off can take three shards of an object with it and, under `4+2`, that object is unreadable until the machine returns, with nothing lost.

## [6131634] - 2026-09-20

Direct commit: README: "Built with Rust", and the alternatives section reworded

### Changed

- The `README.md` tagline says the language, and the comparison section is headed "Alternatives?" with an opening that names the use case: non-uniform nodes with non-uniform storage devices.

## [fb6c470] - 2026-09-20

Pull request #87: README: why not MinIO, Garage, SeaweedFS, or Ceph

### Added

- A `README.md` section stating the job Distributed-JBOD is built for, then one paragraph each on MinIO, Garage, SeaweedFS and Ceph saying what each is good at and why it does not fit that job, the bare-disk recovery story none of them offer, and a plain statement of where they win and that this project is young.

## [dd3238d] - 2026-09-20

Pull request #86: License: AGPL-3.0-only

### Added

- `LICENSE`, the canonical text of the GNU Affero General Public License (AGPL) version 3 from gnu.org, with a License section in the `README.md` stating the terms in plain words, including what the network clause asks of anyone offering a modified djbod to others, and SPEC 21.5 recording the decision and the reasoning.
- `THIRD-PARTY-NOTICES`, the copyright and licence notices of all 181 dependencies, generated by the new `scripts/third-party-notices.py` from `Cargo.lock` and the local registry. Every dependency is permissive and AGPL-compatible, and their one condition is that these notices travel with binaries. Identical texts are written once with the packages they apply to, and the two packages that ship no licence file have theirs named instead.
- A `source` link to the repository in the web interface header, the usual way to satisfy the AGPL's requirement to offer remote users the source.

### Changed

- The licence is `AGPL-3.0-only` in the workspace `Cargo.toml`, inherited by every crate; it had said MIT while the repository carried no licence text at all.

## [f2fdc63] - 2026-09-20

Pull request #81: A human-readable cluster name: proposal and implementation

### Added

- `docs/proposals/cluster-name.md` and its implementation: `ClusterDocument.name`, optional and validated with the label rules, so an existing document is valid and unnamed. A node on an older build refuses a document carrying a name, so `set-name` needs every node upgraded first (SPEC 6.2.6.4).
- `djbod-node init-cluster --name <name>` or `DJBOD_CLUSTER_NAME` at creation, and `djbod cluster set-name <name>` and `set-name --clear` afterwards, through `membership::set_cluster_name`.
- The name beside the id in `djbod status` and `cluster show`, as `cluster_name` in `--json`, in the web interface header and tab title, in the node's running log line, and in the refusal a node gives a client of another cluster.
- Optional `cluster_name` on `Status` and on a node's `Hello`, both ignored by older readers, with SPEC 6.2.5.3 added and 6.2.2, 19.1.3 and 19.1.5 listing the new fields.

## [4d8ab2e] - 2026-09-19

Pull request #65: Spec: the web UI's browser link, its checks, and its state

### Added

- SPEC 20.3.2, recording the browser link as it is: plain HTTP, no authentication, localhost by default, with the same-origin check on changes and the host-name check on every request, and the statement that these are the browser's own word and not authentication.
- SPEC 20.3.3 and open question 21.4, recording the two routes to encrypting and authenticating that link, a Transport Layer Security (TLS) reverse proxy or TLS terminated by `djbod-ui` with a browser client certificate, and deferring the choice to the users-and-permissions item.
- A note in SPEC 20.3.1 that the web interface is a client like `djbod`, so a `tls` cluster requires it to hold a certificate, and C.4.7 listing what the interface has gained since its first version.

## [da9ff6b] - 2026-09-19

Pull request #82: UI: shorter mixed-builds banner

### Changed

- The mixed-builds banner is shorter and no longer repeats what a node does with a document it cannot represent.

## [5ddd227] - 2026-09-19

Pull request #72: UI: a progress bar for the scrub

### Added

- A progress meter under the scrub controls, advancing by device because the node reports each device once, when it has finished it, showing devices done out of the devices to scrub with the bytes read and the rate, then filling for the cross-node phase and any repairs, which have no count in advance, and ending with the time taken, the devices and the bytes read, or the reason for a failure.
- A moving stripe showing the bar is alive while nothing has changed, held still under `prefers-reduced-motion`. Progress within a device would need the node's local scrub to send progress items as it reads.

## [e11dec5] - 2026-09-19

Pull request #79: UI: show each node's build and the UI's own

### Added

- `build` on each node row of `GET /api/cluster`, null for an unreachable node or one from before builds were sent, and `ui_build` on `GET /api/status`, this server's own build.
- Each node's build under its document version in the Nodes table, with the build lines marked and the Nodes tile saying how many builds are in use when they differ, a banner naming the builds when they are mixed, and `ui build` beside the transport in the header.

## [140707f] - 2026-09-19

Pull request #78: Refuse a document with a field this build does not know

### Changed

- `ClusterDocument`, `NodeEntry` and `DeviceEntry` refuse a field the build does not know, on the wire and when a node reads its own `cluster.json`, so a node never holds a version it cannot represent. An older node had received a document carrying `NodeEntry.label`, dropped the field and saved the version without it, after which two nodes held version 11 with different content and every further change was refused. Optional fields added later still default when absent, so a document without them is accepted by old and new builds alike (SPEC 6.2.6.4).
- A request a node cannot decode is answered on the request's id with `ProtocolViolation`, naming the reason, the node's build and its id, before the connection closes, so the proposer's `Superseded` or `Partial` error names the node to upgrade. Until now such a request ended the connection with no reply.

### Added

- `Hello.build`, the crate version plus the git commit, optional in serde so builds before this one send nothing and read it as absent, with a `BUILD` column in `cluster show` and the same string from `djbod-node --version` and `djbod --version`. The commit comes from a `build.rs` running `git rev-parse`, overridden by `DJBOD_GIT_COMMIT` for builds without a checkout.
- SPEC 6.2.6.4 on mixed builds, stating the rule, the consequence for a rolling upgrade, and that a build from before this change still drops unknown fields, so every node must be upgraded before a change that uses a newer field.

## [2030b3b] - 2026-09-19

Pull request #76: UI: show node labels first, with the UUID in parentheses

### Changed

- The page shows a node's label first with its short UUID in parentheses, and a bare UUID when there is none, across the nodes table, the device table's node column, an object's shard table, the header, the move-shard and drain choices, the scrub and drain logs, and the node field of an error.

### Added

- A Label button on the nodes table, reading Rename once a label is set, backed by `POST /api/nodes/{id}/label`, which calls the same membership procedure as the command.

## [b23eaaa] - 2026-09-19

Pull request #80: The icon: browser tab, page header, and README

### Added

- The application icon in the browser tab, served from the binary with a day's cache so browsers stop asking for a missing favicon; as a 26 px mark beside the page title, with an ink-inverted version chosen under `prefers-color-scheme` for the dark theme; and at the top right of the `README.md`.

## [bc7048d] - 2026-09-19

Pull request #77: Change a node's address after it has joined

### Added

- `djbod cluster set-address <node> <ip:port>[,...]`, replacing a node's address list as a document change like any other, where a node's address could previously enter the document only at `init-cluster` and `join`.
- Startup adoption that proposes the node's own advertised address when the document disagrees, skipping itself in the proposal because it is not yet serving, adopting and retrying from a change that lands meanwhile, and refusing to start when the proposal fails, since serving where no other node knows the address would be the same as being down (SPEC 18.1.2.1).
- Validator rules that every node lists at least one address, each an IP address and port, and that no address is listed for two nodes, each with its own error variant.
- SPEC 6.2.5.2, stating that a node's address list is never empty, is unique, that the first is the one used, the two ways the list changes, and the last resort for moving every node at once, which neither procedure covers.

### Changed

- `cluster show` prints every listed address, with the JSON output gaining an `addresses` array beside the existing `address`.

## [7e10db5] - 2026-09-19

Pull request #75: Flush every frame; abandon a stream that goes silent (SPEC 10.12)

### Fixed

- `PutObject` hung under the TLS transport because `EndOfStream` frames were never flushed: `tokio-rustls` reports plaintext as written even when the encrypted records left the socket pending, and only the next write or a flush sends them. `write_message` now flushes after writing, which covers every frame on every path, since it is the only place the node, the client and the interface server write to a connection. `TcpStream` flushing is a no-op, so plain transport is unchanged (SPEC 10.12).
- A killed client no longer leaves the holders' `.tmp` files and the coordinator's connections open indefinitely: the new `stream_idle_timeout_secs`, defaulting to 120 and also settable as `--stream-idle-timeout-secs` and `DJBOD_STREAM_IDLE_TIMEOUT_SECS`, bounds how long a holder or the coordinator waits for the next frame. On expiry the holder's write is dropped, which removes its temporary, and the coordinator aborts every holder and reports `WriteFailed`. The timeout is per frame, so a slow but active sender is unaffected.

## [ac29f71] - 2026-09-19

Pull request #74: UI: a blue List button, and space between the upload strip and the filter

### Changed

- The List button takes the accent style, and the filter row sits 20 px below the upload strip so the two read as separate groups.

## [6dd3a03] - 2026-09-19

Pull request #70: UI: upload controls first, the prefix filter under them, with a Clear button

### Changed

- On the Objects page the upload controls form the first row with Upload as its primary button, and the prefix filter and its List button sit beneath, with a Clear button that empties the prefix and lists every key.

## [473cc7b] - 2026-09-19

Pull request #71: Node labels, under the same rules as device labels (SPEC 6.2.5.1)

### Added

- `NodeEntry.label`, omitted from the JSON when absent so existing documents parse unchanged, under the same rules as a device label and unique among nodes. Node labels and device labels are separate namespaces, since no command takes both, so `nas1` may name a node and `nas1-bay0` one of its disks.
- `djbod cluster set-node-label <node> <label>` and `--clear`, a document proposal like `set-label`, and a UUID or label accepted by `remove-node`, `remove-node --force` and `drain --node-id`. The typed confirmation of a forced removal remains the UUID, as SPEC 6.2.6.3 says.
- A `NODE LABEL` column in `status` and a `LABEL` column in `cluster show`, both in `--json` output as well, and the `node_by_label`, `node_by_name` and `node_name` helpers on the document.

## [dd15f01] - 2026-09-19

Pull request #61: UI: warn while a link is unencrypted

### Security

- A banner under the header, dismissable for the session, warning while a link is unencrypted: red while the cluster's transport is `plain`, since connections between nodes and from clients are then unencrypted and unauthenticated, amber while it is `tls-optional` as a passing state, and nothing under `tls`, each naming the command that moves it on.
- A red banner while the page itself is served over plain HTTP from anywhere but the machine the server runs on, since every action then crosses the network in the clear and anyone who can reach the port can administer the cluster; it suggests an SSH tunnel or a reverse proxy with TLS.

## [6c42b2b] - 2026-09-19

Pull request #68: README: written for the person who will run it

### Changed

- The `README.md` is reordered for the person who will run the system rather than for a developer: a quick start of build, configuration, `init-cluster`, `run` and a put, list, get and status in one screen; what you get, in the user's terms, with concrete `3+1`, `4+2` and `1+1` examples; the tools one line each; an honest security section with the default port folded in; the specification, proposals, crates and tests moved to a developer section near the end; and the status last.

## [00fdb23] - 2026-09-19

Pull request #67: UI: remember the section without scrolling to it

### Fixed

- Opening the Objects section no longer scrolls the page down. The page records the open section in the URL fragment so a reload reopens it, but a fragment is also a scroll target and the key list's table body carries the id `objects`; the section is now recorded with `history.replaceState`, which scrolls nothing.

## [6754093] - 2026-09-19

Pull request #64: UI: navigation as a left rail

### Changed

- The four sections move from a row of tabs to a rail down the left, one button each with an icon and, where it means something, a live count: devices on Overview, keys in the current listing on Objects with a `+` when more follow, and on Maintenance the number of objects with a remembered read failure or of draining devices. The rail stays beside the content as it scrolls and becomes a row above the content on a narrow screen.

## [0b7b149] - 2026-09-19

Pull request #55: UI: keep the object panel's width fixed

### Fixed

- Opening the record JSON no longer widens the right column and squeezes the key list. A grid column will not shrink below its widest unbreakable content and the record's lines are long, so both columns of the Objects tab may now shrink below their content, leaving the JSON to scroll sideways in its own box, and the panel stays in view and scrolls on its own when it is taller than the window.

## [2606d6a] - 2026-09-19

Pull request #66: README: say who the system is for and what problem it solves

### Changed

- The `README.md` introduction leads with the problem, a large pool of networked storage from a few commodity machines and whatever disks they have, easy to grow and shrink, then what the system does about it and that the balance between durability and efficiency is configurable.

## [e747fe7] - 2026-09-19

Pull request #63: README: bring it up to date with what is built

### Changed

- The `README.md` is brought up to date with what is built: what the system does, a table of the binaries and tools with their commands and the crate list, what the getting-started walkthrough covers, the status of the milestones and the proposals under `docs/proposals`, and the security posture of plain by default with what TLS does and does not give. Every command and section name was checked against the current binaries and guide.

## [33a4047] - 2026-09-19

Pull request #60: UI: device endpoints accept a label as well as a UUID

### Changed

- The `state`, `label`, `drain` and `remove` device endpoints accept a label as well as a universally unique identifier (UUID), resolved through `membership::resolve_device` as every `djbod` command that takes a device does. The page still sends UUIDs, so this serves anyone calling the interface by hand or from a script, at the cost of one document fetch.

## [c321575] - 2026-09-19

Pull request #59: UI: show the cluster's transport

### Added

- The cluster's `transport` forwarded by the status handler, with `ui_to_node_tls` saying whether this server's own connection to the node uses Transport Layer Security (TLS), shown in the header and in an Overview tile with a line saying what the value means.

### Fixed

- The status handler matched the node's reply with `..` and forwarded four fields, so the `transport` the TLS work added was dropped and the page could not say whether the cluster is `plain`, `tls-optional` or `tls`.

## [27d9f52] - 2026-09-19

Pull request #58: UI: say whose fault a failed document change is

### Changed

- Each variant of `MembershipError` maps to its own status and a code that is its name, where every membership error had come back as 502 with the code `membership`, so a refusal to remove a device looked the same as an unreachable cluster: 502 for a node that could not be reached or a change that got part way round, 404 for something that does not exist, 409 for a well-formed request the store's state refuses or a race a retry settles, and 500 for a fault in this server's own configuration.
- The page titles the toast by status: refused, not found, bad request, or cannot reach the cluster.

## [8118ff7] - 2026-09-19

Pull request #57: UI: refuse cross-site changes and unknown host names

### Security

- Any method but GET or HEAD must carry `Sec-Fetch-Site: same-origin` or `none`, and a client with no such header passes only if it sends no `Origin` or an `Origin` naming this host. The server has no authentication, and a page on any other site could otherwise make an administrator's browser post to the body-less endpoints, since a plain form post needs no preflight (SPEC 20.3.2).
- Every request, the page included, must name a host this server serves: an IP literal, `localhost`, or a name given with the new repeatable `--host NAME`, because a domain name someone else points at this address looks same-origin to the browser.
- Both checks live in one middleware on every route. This is not authentication, only the browser's own word about where a request came from, and the guide says so.

## [ea6ea1d] - 2026-09-19

Pull request #56: Proposal: note that the UI's stopgap is found by polling and can be late

### Changed

- `docs/proposals/damage-marks.md` records that the user interface's in-memory note reaches the page only by polling after a download, so damage early in an object is seen at once and damage late in a large or slow download late or not at all, until the next periodic refresh.

## [fbd6ef6] - 2026-09-19

Pull request #54: UI: show device labels first, with the UUID in parentheses (to main)

### Changed

- Wherever the page names a device, one with a label shows the label first and its short UUID in parentheses, and one without shows the UUID alone.

### Added

- A Label button on the devices table, reading Rename once a label is set, backed by `POST /api/devices/{id}/label`, which is `djbod cluster set-label`.

## [55a0e99] - 2026-09-19

Pull request #53: UI: remember reads the node stopped for damage, and show why (to main)

### Added

- An in-memory note per key in the user interface server of the last read the node stopped, whether a download or a verify, with the error's device, shard and stripe and how many bytes went out first, cleared when a download, verify or repair of the key succeeds or the key is deleted.
- `GET /api/read-failures` listing the notes, the note shown on the object panel, a column of its own marking the key in the list, and polling for the note after Download is clicked. The guide documents this as best effort, with the durable answer in `docs/proposals/damage-marks.md`.

## [ecb79e0] - 2026-09-19

Pull request #41: Device labels: a name beside each device UUID (SPEC 6.2.5.1)

### Added

- `DeviceEntry.label` in the cluster document, omitted from the JSON when absent so every existing document parses unchanged, and required by the validator to be 1 to 128 bytes, free of whitespace and control characters, not parseable as a universally unique identifier (UUID) so it can never be mistaken for one, and unique across the document.
- `djbod cluster set-label <device> <label>` and `set-label <device> --clear`, a document proposal like `set-state`, where setting a label a device already has is a no-op.
- A label accepted in place of a UUID by `set-state`, `drain`, `set-label`, `remove-device` and `move-shard --to`, resolved through `membership::resolve_device` against the current document.
- A `LABEL` column in `status`, fed by an optional `label` field on `DeviceStatus` that each node fills from its document, with labels shown in `cluster-config` as part of the document.

### Changed

- Errors from nodes still carry the device UUID, because putting the label into `ErrorDetail` would touch every construction site in the coordinator for a cosmetic gain while `status` maps UUID to label in one command; SPEC 6.2.5.1 says so.

## [dcde5e2] - 2026-09-19

Pull request #48: UI: a Verify button that reads the object through and names any damage

### Added

- A Verify button beside Download that reads the whole object through the node without saving it, so every block is checked against its checksum and the whole object at the end, and shows either that it verified, with the size read and the version, or the node's error with the device, shard and stripe and a pointer at Repair (SPEC 11.7).
- `POST /api/verify/{key}`, returning 200 with `verified: true` or `verified: false` plus the node's error detail, since a damaged object is an answer rather than a failed request, with 404 for a missing object and 502 for an unreachable node.

## [a85af24] - 2026-09-19

Pull request #44: UI: explain a download cut short in the Download button's tooltip

### Added

- A tooltip on the Download button explaining that a download that meets damage is cut short of its declared length, because the whole-object check of SPEC 11.7 arrives after the body and nothing else is possible over HTTP, and pointing at Repair.

## [dd8cf67] - 2026-09-19

Pull request #50: Proposal: remembering damaged shards in a per-node ledger

### Added

- `docs/proposals/damage-marks.md`, a discussion document asking why the store should remember what a read, repair, drain or scrub found damaged, when today every such finding is reported once and forgotten. It weighs four places the marks could live and recommends a per-node `damage.json` in the state directory, written only by the holder of the shard, set by the local scrub and by a fire-and-forget `MarkDamage`, and cleared by repair, by a clean scrub, by deletion and by a read whose whole-object check passed. Marks would be hints changing no operation's behaviour.

## [94d84fc] - 2026-09-19

Pull request #45: One place decides how a client connects

### Added

- `Connector::from_client_options` in `djbod_node::transport`, the one place that turns the three client TLS settings into a connector: plain with none given, TLS presenting no certificate with the authority alone, and TLS with the client's identity with all three. A certificate without its key, or either without the authority, is refused with `TlsError::IncompleteClientIdentity`.

### Removed

- The identical connector function that `djbod` and `djbod-ui` each carried, and the `ClientTlsPaths` import from both binaries.

## [b8c518d] - 2026-09-19

Pull request #49: Spec: scrub history as an open item (20.1.4)

### Added

- SPEC 20.1.4, recording scrub history as an open question: a scrub's findings live only in the stream sent to the client that asked for it, so nothing in the cluster records that a scrub ran, when, or what it found. The section states the want and the points to settle: where the record lives, given that a node-local scrub has only its own state directory while the cross-node findings belong to no single node; how much to keep; and how it is shown in `djbod status`, in a history listing and in the web interface.

## [ca3cfa2] - 2026-09-19

Pull request #47: UI: make click-to-copy work over plain HTTP

### Fixed

- Click-to-copy works over plain HTTP, where `navigator.clipboard` does not exist because it is offered only on secure origins. The page falls back to the older selection-based copy, reports "copied" only when a copy succeeded, and otherwise shows the full id in the toast to copy by hand.

## [c3be4db] - 2026-09-19

Pull request #46: Rewrap SPEC 20.3.1 and comment the download's trust in a clean stream end

### Changed

- SPEC 20.3.1 is rewrapped at 72 columns with its words unchanged.
- A comment on the download handler's clean-end arm records why verifying each chunk's checksum in transit is enough: the coordinator checks the delivered length and the whole-object checksum against the record before ending the stream cleanly, and ends with `ObjectChecksumMismatch` otherwise, which is what `djbod get` relies on too.

## [01433e3] - 2026-09-19

Pull request #43: UI: show upload progress, with a cancel button

### Added

- An upload meter showing bytes sent of the total, the percentage, the rate and the time left, driven by `XMLHttpRequest` upload progress events in place of `fetch`, which reports nothing until the response arrives.
- A note reading "waiting for the node to finish writing" between the last byte sent and the node's answer, which covers the node's fsync and the server's body drain after a refusal.
- A Cancel button that aborts the request; the node sees the connection end and removes its reservations, as it already did for any dropped client.

## [51e1afc] - 2026-09-19

Pull request #42: UI: deliver a refused upload's reason, and check the room first

### Added

- `GET /api/upload-check?size=N`, judging a write as the coordinator does: k+m active devices each with a shard file's worth of free space within headroom, plus the object size limit, so the page refuses locally with the numbers when a file cannot fit. The node remains the authority when the upload runs, and devices sharing one filesystem make the check optimistic, which the message says (SPEC 10.4).

### Fixed

- The node's refusal mid-upload reaches the page with its code, device and message. The server had answered and closed a connection whose request body it had not read, which a browser reports as a network failure, dropping the response; the upload handler now reads and discards the rest of the body before answering.

## [b3f3dbf] - 2026-09-19

Pull request #36: Milestone 5: the administration web UI, djbod-ui (SPEC 20.3.1)

### Added

- `djbod-ui`, a crate serving one embedded page and a JSON API under `/api` on a local HTTP port, where every call is one node operation or one membership procedure, so the user interface holds no state of its own and every action it offers is also a command-line operation (SPEC 20.3.1).
- An Overview of nodes with their document versions and reachability and devices with a used-space meter and state, with set-state, remove and sync.
- An Objects section listing by prefix with paging, upload, the record and shard placement, download, repair, move-shard and delete.
- A Maintenance section showing scrub and drain events live as newline-delimited JSON, and a Settings section for the scheme, the limits and the raw cluster document.
- Object bodies streaming through in both directions without being held, with a download that meets damage cut short of its declared `Content-Length` so the browser reports a failed download rather than saving a wrong file.
- `djbod-ui --listen`, taking `DJBOD_NODE` and `DJBOD_CLUSTER` like the client and binding to localhost by default. Forced node removal and re-encode stay on the command line, and there is no authentication until the protocol has it.

## [6b3b858] - 2026-09-19

Pull request #40: scripts/djbod-pki.sh: certificate issuing for administrators

### Added

- `scripts/djbod-pki.sh`, wrapping the `openssl` commands of the guide's TLS section as `init-ca`, `node`, `client` and `list`. djbod itself still generates no keys and signs nothing (SPEC 19.1.6.1).
- `init-ca` creates the authority on the P-256 curve with `CA:TRUE` and `keyCertSign`, keeping the directory and `ca.key` owner-only and `ca.crt` world-readable since it is copied everywhere.
- `node` issues a certificate with one IP subject alternative name per address given and prints the `[tls]` table to paste, refusing anything that is not an IP address because the document lists nodes by IP and port; `client` issues a `clientAuth` certificate and prints the three `DJBOD_TLS_*` exports.
- A refusal to overwrite any existing file, and requests made under `umask 077` so keys are never readable by others even briefly.

## [fb4c4de] - 2026-09-19

Pull request #39: Milestone 5 (d): the TLS walkthrough and the spec status

### Added

- A TLS section in the getting-started guide: one certificate authority with `openssl`, a node certificate carrying an IP subject alternative name for the address the document lists and `serverAuth,clientAuth` key usage, a client certificate, installing the files with owner-only permissions, moving a cluster from `plain` to `tls-optional` to `tls` with what each refuses, joining a new node under TLS, and withdrawing a certificate by rotating the authority with a two-root bundle for the transition.

### Changed

- SPEC 19.1.6 and its subsections move from proposed to decided, noting that document addresses are IP and port so node certificates carry IP names, and that an extended key usage extension, where present, must include `serverAuth` for nodes and `clientAuth` for anything connecting. C.4.6 records milestone 5 complete.

## [7572a08] - 2026-09-19

Pull request #38: Milestone 5 (c): every node setting from argument, environment, or file

### Added

- One `ConfigArgs` group shared by `init-cluster`, `join`, `add-device`, `run` and `scrub`, covering the node id, listen and advertised addresses, state directory, devices, bootstrap peers, temporary-file maximum age, the shared-filesystem override and the TLS paths, each with a `DJBOD_` variable. An argument wins over a variable, which wins over the file, and a list given as an argument or a variable replaces the file's list (SPEC 20.6).
- A node fully describable by its environment: with no `--config`, the node id, state directory and at least one device must come from arguments or the environment, and the error names the flag and the variable that would supply a missing one.

### Changed

- `scrub --device` keeps its meaning of scrubbing only that device but is now the shared device override rather than a separate filter, so the binary has one `--device`.
- `NodeConfig::read` parses a file without validating it so the overlay can complete it first, while `load` still validates.

## [e02dcdb] - 2026-09-19

Pull request #37: Milestone 5 (b): client certificates in djbod

### Added

- `djbod --tls-ca`, `--tls-cert` and `--tls-key`, global to every subcommand, with `DJBOD_TLS_CA`, `DJBOD_TLS_CERT` and `DJBOD_TLS_KEY` as the environment equivalents; the certificate and key require each other and the authority (SPEC 19.1.6.2, 20.6).
- Three ways to connect: no authority means plain, the authority alone means Transport Layer Security (TLS) with the server verified but no client certificate presented, which a `tls-optional` cluster accepts and a `tls` cluster refuses, and all three mean mutual TLS.
- `Connector::from_client_paths` in the transport module, building the anonymous or authenticated client configuration from paths, with the key permission check applied when an identity is given.

### Changed

- Every command uses the connector, the `cluster` subcommands included, and the `Connector::plain()` placeholders are gone.

## [99bfa9f] - 2026-09-19

Pull request #35: Milestone 5 (a): TLS transport, node certificates, and the three modes

### Added

- `djbod_node::transport`: `TlsMaterial::load` reads a node's certificate, private key and authority bundle from PEM files, `Stream` puts plain TCP and TLS behind one read and write type so the frame code and the handlers are unchanged, and `accept` looks at the first byte, since a TLS handshake record is `0x16` and no frame type is (SPEC 19.1.6).
- A `[tls]` table with `cert`, `key` and `ca` paths, `--tls-cert`, `--tls-key` and `--tls-ca` on `run`, `join` and `add-device`, and `DJBOD_TLS_CERT`, `DJBOD_TLS_KEY` and `DJBOD_TLS_CA`, flags over variables over file, paths only and never material (SPEC 20.6).
- `ClusterDocument.transport` as `plain`, `tls-optional` or `tls`, with `membership::set_transport` and `djbod cluster set-transport`; leaving `plain` is refused with `NodeNotTlsReady` until every node reports material loaded, which `LocalStatus` carries as `tls_ready`, and a node refuses to start or to adopt a document under a TLS transport without material (SPEC 19.1.6.4).
- A `&Connector` first parameter on every membership function, so the caller chooses plain or TLS; `join` and `add-device` try TLS first when the node has material and fall back to plain, so a node with its own certificate joins a `tls` cluster with nothing registered in advance (SPEC 19.1.6.5).
- Counters of connections accepted by transport, which the migration test uses to prove node-to-node traffic switched.

### Security

- `TlsMaterial::load` refuses a private key file readable by anyone but its owner (SPEC 19.1.6.2).

## [1708dc4] - 2026-09-19

Pull request #34: Spec: TLS design (19.1.6) and configuration sources (20.6)

### Added

- SPEC 19.1.6, the Transport Layer Security (TLS) design: one certificate authority per cluster with everything issued by `openssl`, since djbod generates no keys and signs nothing; material named by path through a flag, an environment variable or a `[tls]` table and never in the configuration file or the cluster document; verification by chain to the authority and, for a server, the dialled host; the three modes `plain`, `tls-optional` and `tls`, with nodes speaking mutual TLS in both TLS modes and the listener telling TLS from plain by the first byte; join authenticated by the node's own certificate with nothing pre-registered; and revocation as authority rotation staged with root bundles.
- SPEC 20.6, the configuration rule: argument over environment variable over configuration file, `DJBOD_` naming, with secrets always in their own files.
- Milestone 5 in SPEC C.4 in four steps, and `rustls` with tokio-rustls recorded as the implementation, leaving the frame layer, the recovery tool and the offline scrub unchanged.

## [a4ab9e5] - 2026-09-19

Pull request #33: Report a version deleted mid-drain as deleted, not as a stale copy (#30)

### Fixed

- A drain reports a version deleted or replaced between the listing and that version's turn as `DrainEvent::Deleted` rather than as a stale copy and a failure of the run, since nothing is stale and the pass has not failed (#30). Only when copies exist but the current record does not place a shard on this device is the copy a genuine leftover for the scrub.

### Changed

- The stale-copy decision moves into `shard_to_drain`, a pure function over the device, the listed copy and the current versions, so it can be tested without staging a race inside one request.

## [27868a7] - 2026-09-19

Pull request #32: Send the listing cursor and limit down to every node (#29)

### Changed

- `coordinator::list_keys` forwards the client's query, cursor and limit to every node and takes one page from each, so the coordinator holds at most one page per node. Before this, every page moved every key in the cluster over the network and held all of them in memory, costing N x P in transfer and N in memory per page for N keys in P pages (#29).
- The merged page is cut no further than the smallest last key among the nodes that reported more, the horizon rule, because under the byte bound a node's page may stop before a long key while the merged page still has room for a shorter key that sorts after it; that shorter key would otherwise become the cursor and the long key would never be asked for again (SPEC 15.2.1).
- The truncation flag also reports whether any node had more, since one version's record sits on k+m devices and the same key collapses to one entry, which can make a page shorter than the bound.

### Fixed

- A client limit of zero is treated as one, instead of producing an empty page marked truncated so that a walk never advanced.

## [6c7580f] - 2026-09-19

Pull request #31: Refuse to remove an active device or a node with active devices (#28)

### Fixed

- `remove-device` refuses an active device with `MembershipError::DeviceActive` and `remove-node` refuses a node with any active device with `NodeHasActiveDevices`, both naming the devices and pointing at `set-state` and `drain`. Before this, nothing stopped a client write from placing a shard on a still-active device between the reference scan and the proposal, after which the device was marked removed while holding data (#28). A draining device receives no new shards, so once the state is established the scan cannot be invalidated by a write. `remove-node --force` is unaffected, since a dead node cannot receive writes.

## [564af37] - 2026-09-19

Pull request #27: Milestone 4 (e): size limits in the cluster document, bounded records, paged listings

### Added

- `max_key_bytes`, `max_object_bytes` and `max_user_metadata_bytes` in the cluster document, all with serde defaults so existing documents parse unchanged, bounded by the validator, and set by `djbod-node init-cluster` or `djbod cluster set-limits` (SPEC 6.2.2, 21.3).
- A bound of 8 MiB of key text on a `ListKeys` or `LocalList` page and 8 MiB of encoded records on a `LocalRecords` page, each with a truncation flag and a cursor, with every internal walk following pages to the end (SPEC 15.2.2). Before this, a cluster of about 4,100 objects with 16 KiB keys made every unlimited listing, and so every scrub, fail with a frame-size protocol violation.
- `MetadataTooLarge`, refusing a content type over its fixed 1 KiB bound or user metadata over the document's limit before any shard is stored.
- Paging for `LocalLookup` by encoded size with a version and device cursor, since with records this large a node holding several copies of one version could not answer a lookup in one frame; the coordinator asks every node concurrently and follows each to the end.

### Removed

- The compiled-in size constants, in favour of the document's fields, with the refusal messages naming the field to change.

## [dc9aa77] - 2026-09-18

Pull request #26: Milestone 4 (d): set-scheme and reencode

### Added

- `djbod cluster set-scheme --k K --m M [--block-size B]`, which proposes a document with the new values and moves no data, refusing with `TooFewActiveDevices` when fewer devices are active than the new k+m, and reporting how many objects are stored at another scheme. Existing objects stay readable indefinitely, because each record carries the scheme it was written with.
- `djbod cluster reencode`, the migration: it pages through every key and, for each version whose recorded k, m or block size differs from the document's, streams a `GetObject` on one connection into a `PutObject` on another through an in-process pipe, keeping content type and user metadata. An interruption leaves either the old version or the new one, and a rerun re-encodes only what is left (SPEC 18.9).
- `put_object_with_metadata` on the client, carrying the user metadata map, with `put_object_from_reader` delegating to it.

### Changed

- The shortcuts SPEC 18.9 allowed when only m changes are deferred, because the shard file header records the scheme and the scrub checks it against the record, so either would need a header rewrite on every shard. A re-encoded object gets a new version id and creation time, since to the store it is a new write of the same key.

## [51b4221] - 2026-09-18

Pull request #25: Milestone 4 (c): djbod-recover, the offline recovery tool

### Added

- The `djbod-recover` crate, depending on `djbod-core` alone (SPEC 20.2).
- `list <device-path>...`, which walks each path's `objects/default` tree directly so a disk that has lost its identity file works like any other, printing key, version, revision, size, present shards over k+m and whether k structurally sound shard files remain, and exiting 2 when any version is short of k shards or any file was damaged.
- `extract <key> [--version <id>] --out <file> <device-path>...`, which opens every shard whose header and footer describe the same object, refuses up front if fewer than k are sound, decodes stripe by stripe, and renames a `.partial` file into place only after the whole-object checksum matches. It refuses to overwrite an existing output and never writes to a device.

## [81238a2] - 2026-09-18

Pull request #24: Milestone 4 (b), second half: remove-device, remove-node, and forced removal

### Added

- `membership::scan_references`, which asks every node for the records on each of its devices, keeps the highest revision per version, and counts the shards current records place on the devices in question, running inline rather than as a background job (SPEC 18.5).
- `remove_device`, refusing with `StillReferenced` and example keys while any current record names the device and otherwise proposing it as `removed`, with the entry kept so a disk that comes back is recognised; and `remove_node`, which does the same for all of a node's devices and then drops the node, refusing to remove the last node.
- `remove-node --force`: `plan_forced_removal` refuses a node that answers a document fetch within five seconds, computes the affected versions and those with more than m shards on the dead node, and the command prints both counts and the unrecoverable keys and requires the node id typed back unless `--yes`. `execute_forced_removal` proposes through `propose_skipping`, which neither asks the dead node for its version nor sends it the document (SPEC 6.2.6.3).
- `--wipe-removed-device` on `join`, `add-device` and `init-cluster`, since a device initialised for the cluster but absent from the document is a removed device and is otherwise refused with `RemovedDevice`. `Device::erase` and `Device::wipe_and_initialise` are the only code that deletes data.
- `relocate_lost_shards`, which picks a distinct new active device per lost shard, most free space first and excluding holders, so repair rebuilds a shard whose device has left the document; `ShardRepair` gains `relocated_to` (SPEC 18.3).

### Changed

- A node that adopts a document no longer listing itself logs a warning and stops serving, and `djbod-node run` waits one second for its acknowledgement to reach the proposer, says why it is stopping, and exits.

### Fixed

- The cluster scrub test asserted no local finding on the third damaged device, which can coincide with one of the other two; the assertion now checks for findings about that object alone.

## [1f608f8] - 2026-09-18

Pull request #23: Milestone 4 (b), first half: set-state and drain

### Added

- `djbod cluster set-state <device> draining|active`, which moves no data and is safe to repeat, since asking for the state a device already has is a no-op and an unknown device is `MembershipError::UnknownDevice`. Placement and `MoveShard` already consider only active devices, so a draining device stops receiving shards with no further code.
- `Drain`, answered with `DrainStarted` and a stream of `Estimate`, `Moved` and `Skipped` events, refused unless the device is draining, with a message pointing at `set-state`.
- `LocalRecords`, the node-to-node operation that lists a device's records, since a device holds a record copy for every version it has a shard of and the scan is therefore local to one node.
- An estimate that sums the shard bytes on the device against the free room on active devices and checks that at least k+m devices are active, ending the stream with `InsufficientDevices` before anything moves unless `partial` is given (SPEC 18.2.2).
- `djbod cluster drain <device> [--partial]`, exiting 2 if anything was skipped, with `--node-id` draining every draining device of a node in turn.

### Changed

- The drain finishes when the version list is exhausted whatever happened to individual versions, so it cannot loop, and the end-of-stream carries `WriteFailed` naming how many versions were skipped (SPEC 18.2.1).

## [110e876] - 2026-09-18

Pull request #22: Milestone 4 (a): record revisions and the re-placement primitive (MoveShard)

### Added

- `MetadataRecord.revision`, 0 when a version is first written and omitted from the JSON then, so every existing record file still parses and verifies unchanged, and covered by the record checksum (SPEC 18.8.1).
- `MetadataRecord::same_body`, saying whether two records describe the same version, key, size, checksum and scheme and differ only in placement.
- `MoveShard` and `djbod move-shard <key> <index> [--to <device>]`, which copy the shard from the source when it is intact and rebuild it from the others otherwise, write the record at revision+1 to the destination and then every other holder, and remove the source's copy (SPEC 18.8.2).
- `ClusterFinding::StaleCopy` from the cluster scrub for a lower-revision copy on a device the record no longer lists, removed by repair and reported in `RepairReport.stale_copies_removed`.

### Changed

- Reads take the highest revision present and require k+m agreeing copies of it from listed devices, ignoring lower revisions.
- `Device::write_record` is idempotent for an equal record and replaces a copy only with a higher revision of the same body; anything else is `RecordExists`.
- `repairable_record` trusts the highest revision vouched for by lower-revision copies of the same body, so a move interrupted after its first `PutMeta` is completed forwards and never backwards.

## [70427eb] - 2026-09-18

Direct commit: Specification: drain is a single pass and cannot loop

### Changed

- `SPEC.md` states that a drain is a single pass and cannot loop.

## [6b0a321] - 2026-09-18

Pull request #21: Specification: milestone 4 design for review

### Added

- SPEC 18.8.1, the record `revision`: 0 at first write and incremented by every re-placement, so the version id identifies the body and the revision identifies its placement. A read of an object fails for the few `PutMeta` round trips of its re-placement window, which is fail-stop as designed but a new way for a read to fail transiently under ordinary administration.
- SPEC 18.8.2, re-placement of one shard from device to device, with every step idempotent and the crash windows stated, and 18.2.1, drain as one command that marks draining, re-places every shard and marks removed.
- SPEC 6.2.6.3, forced removal of a dead node: show the cost first, require the id typed back, apply without the dead node's acknowledgement, then rebuild. A returning removed node refuses to serve and its devices refuse reinitialisation without an explicit wipe flag.
- SPEC 20.2.2 `djbod-recover`, listing and extracting from mounted disks with no cluster and never writing to a device, and 18.9 re-encode as a migration on the primitive.

## [3732738] - 2026-09-18

Pull request #20: Repair rewrites missing record copies when at least k agreeing copies remain

### Added

- Repair rewrites the record copies a device has lost, closing the one case the cluster scrub could find but not fix. It trusts the record when the copies that exist agree, each comes from a device the record lists, and there are at least k of them, and reports the devices it wrote to in a new `record_copies_rewritten` field (SPEC 18.4.2). Two disagreeing copies, or fewer than k, are refused with `RecordsInconsistent`.

### Changed

- Shards are rewritten before record copies, so a crash between the two leaves a shard without a record, which the scrub reports and a later repair completes. Reads keep the strict rule of SPEC 9.4.4, all k+m copies present and agreeing; repair is the one operation allowed to proceed with fewer.

## [d2532ae] - 2026-09-18

Pull request #19: Milestone 3 (d): the cluster-wide scrub and the write-collision guard

### Added

- `LocalScrub`, which runs the local scrub engine over a node's own devices on a blocking thread and streams each finding and then a summary per device, so no data crosses the network for detection.
- `Scrub` in three phases: fan `LocalScrub` out to every node and relay every event as it arrives, with a node that cannot be scrubbed becoming a `NodeFailed` event; cross-node checks for `RecordsInconsistent`, `ShardMissingOnHolder` and `HolderUnavailable`, which catch a device that lost both record and shard for a version; and, with `repair`, one `RepairObject` per damaged key issued from the coordinator.
- `djbod scrub` with `--rate-mib`, `--repair` and `--json`, exiting 0 when clean and 2 when anything was found or the scrub was incomplete.
- The write-collision guard: a node refuses a second `PutShard` for a shard already being written on that device, releasing the slot when the first write finishes or is dropped, and temporary file names carry a process- and call-unique token so two writers can never share one (SPEC 20.1.2.1).

### Changed

- `SPEC.md` records 20.1.2 as built and 20.1.2.1 as decided, closes open question 21.1, and marks milestone 3 complete.

## [5be4b18] - 2026-09-18

Pull request #16: Add the local scrub engine and an offline scrub check

### Added

- `djbod_core::scrub`, which reads one device directly with no network and no node: every record parsed, checksummed, validated and checked against the directory it is in; every shard file opened and every block read against its checksum; and the within-device cross-checks for a record whose shard is absent, a shard with no record, a record that does not list this device, and stale temporaries (SPEC 20.1).
- A rate limiter capping bytes read per second. The scrub is safe while the node runs, because files are immutable once renamed and a file that vanishes mid-scrub is a concurrent delete rather than damage.
- `djbod-node scrub`, which runs the engine offline over one machine's devices, with `--device`, `--rate-mib` and `--json`, exiting 0 when clean and 2 when anything was found.
- SPEC 20.1.2 describing both layers and 20.1.2.1 stating the guard needed before repairs can be issued from more than one place.

## [cc1c114] - 2026-09-18

Direct commit: Specification: membership items decided; milestone 3 status

### Changed

- `SPEC.md` marks the membership items decided and records the milestone 3 status.

## [3f3fe57] - 2026-09-18

Pull request #18: Milestone 3: joining, document changes without a master, sync, startup adoption

### Added

- The `membership` module, written as a client over the native protocol so it runs from anywhere: `fetch_all`, `propose`, `sync`, `join`, `add_devices` and `adopt_from_peers`.
- `propose` reports a refusal by the first-listed node as `Superseded`, to be refetched and retried, and a later failure as `Partial` naming the nodes that applied.
- `djbod-node join` and `add-device`, `djbod cluster show` and `djbod cluster sync`, with `run` consulting bootstrap peers before opening.

### Changed

- `ApplyClusterConfig` accepts any higher version of its cluster's document, not only version plus one, since versions are produced serially through the first-listed node and a straggler two versions behind must be able to jump. SPEC 6.2.6 is corrected where it said exactly plus one.

### Fixed

- A peer that refuses the coordinator's `Hello` over a version mismatch is reported as `DocumentVersionMismatch` with its own message rather than as unreachable.
- A client whose upload the coordinator refuses before the body is consumed reads the refusal the coordinator sent, instead of reporting the broken pipe its own write hit.

## [a3cde8e] - 2026-09-18

Pull request #17: Specification: milestone 3 design for review

### Added

- SPEC 6.2.6, changing the cluster document without a master: check that every node holds version N, build N+1, and apply in document order stopping at the first refusal, so two concurrent proposers cannot both enter a document with one version number. The first-listed node has no run-time role; it is an ordering rule derived from the document.
- SPEC 6.2.6.1 and 6.2.6.2: nodes only move forward and versions are produced serially, so a node may adopt any higher version it is shown, and a straggler after a partial apply fails requests with `DocumentVersionMismatch` until `djbod cluster sync` catches it up.
- SPEC 6.2.6.3: a permanently dead node blocks document changes until milestone 4 adds forced removal, recorded as a known version 1 limitation.
- SPEC 18.1.1 join, 18.1.2 startup adoption and 18.1.3 adding a device to an existing node, with placeholders for `Scrub` and `LocalScrub` in 19.1.3 and the milestone 3 build order in C.4.

## [dbfd6ac] - 2026-09-18

Pull request #15: Log field names in plain text; keep level colours

### Fixed

- Log field names are written plainly as `name=value` instead of in the terminal's italics, through a custom field formatter that is replaceable independently of the event formatter, which still colours the level and dims the target.

## [aaec982] - 2026-09-18

Pull request #14: Add RepairObject: rebuild damaged or missing shards from the intact ones

### Added

- `RepairObject` and `djbod repair <key>`, which rebuild damaged or missing shards from the intact ones (SPEC 18.4.1).
- A first pass that reads every stripe of every readable shard with all k+m indices requested, so any block failing its checksum surfaces as a fault, and refuses to write anything when more than m shards are damaged in any stripe or when the whole-object checksum disagrees.
- A second pass that opens `PutShard` to each damaged shard's own device, re-encodes each stripe and streams the damaged shards' blocks, leaving placement unchanged. Two passes rather than one because a shard's condition is known only after its last block has been read, and buffering a whole shard would break the bounded-memory rule of SPEC 3.6.
- A report listing every shard's condition and whether it was rewritten. Rewriting to a different device when the original is gone waits for the drain and re-placement machinery of milestone 4, and `SPEC.md` says so.

## [aad6066] - 2026-09-18

Pull request #12: Warn once per shared filesystem, listing the devices on it

### Fixed

- Devices sharing a filesystem produce one warning naming them all, instead of a warning for every pair at `init-cluster` and again at `run`. The refusal without `allow_shared_filesystem` is unchanged.

## [a5ce980] - 2026-09-18

Direct commit: Specification: defer human-readable device labels

### Changed

- `SPEC.md` defers human-readable device labels.

## [79206fc] - 2026-09-18

Direct commit: Getting started: say where node.toml lives

### Changed

- The getting-started guide says where `node.toml` lives.

## [9d903cb] - 2026-09-18

Direct commit: Add a getting-started guide for a single-machine cluster

### Added

- `docs/getting-started.md`, a walkthrough of a single-machine cluster, with the `README.md` shortened to point at it.

## [221185f] - 2026-09-18

Pull request #11: Add the djbod command-line client

### Added

- `djbod`, a binary driving a node from a shell with `status`, `put`, `get`, `head`, `list`, `delete` and `cluster-config`, taking `--node` and `--cluster` from `DJBOD_NODE` and `DJBOD_CLUSTER`, and `--json` for machine-readable output.
- Streaming both ways, so memory is bounded by one chunk: a `get` to a named file that fails part way removes the partial file and says so, since the whole-object check arrives in the final stream frame (SPEC 3.6, 11.7).
- Errors printing the code, the message and every identifying field the node supplied: node, device, key, version, shard and stripe (SPEC 16.2).
- A single-machine walkthrough in the `README.md`, with milestone 2 recorded complete in SPEC C.4.2.

## [f33b077] - 2026-09-18

Pull request #10: Add the coordinator: client-facing operations for a cluster of one node

### Added

- The `coordinator` module, serving every client-facing operation by fanning node-to-node operations out over every node in the cluster document, this node included over loopback, so one node and twenty take the same code path (SPEC 4.1).
- Lookup, which broadcasts `LocalLookup` and then checks that a version has k+m equal copies, each from a device the record lists and naming the requested key, with the newest version winning (SPEC 13, 9.1.6, 9.4.4).
- `PutObject`, which places by most free bytes over k+m distinct active devices with room, streams the body into stripes, encodes and fans out with stripe numbers while folding in the whole-object checksum, writes the records, and then deletes older versions (SPEC 10).
- `GetObject`, which opens every data shard before the record reaches the client, decodes each stripe and fails the stream naming device, shard index and stripe on anything but `Intact` (SPEC 11.4, 11.7).
- `ListKeys`, taking the newest version per key across nodes with `start_after`, `limit` and `truncated` (SPEC 15.1).
- `ulid::VersionGenerator`, monotonic within a millisecond (SPEC 9.2.3), and `advertise` in the node configuration for the address recorded in the document.

## [c201faa] - 2026-09-18

Direct commit: Specification: open question on where size limits live; advertise address in 6.1.1

### Added

- An open question in `SPEC.md` about where the size limits live, and the advertised address in SPEC 6.1.1.

## [152da35] - 2026-09-18

Pull request #9: Add the node process serving the node-to-node operations

### Added

- `djbod-node`, a binary with `init-cluster` and `run` that creates a cluster from its own devices and serves every node-to-node operation of SPEC 19.1.3 over TCP.
- `config`: the per-node TOML file with node id, listen address, state directory, device paths, bootstrap peers, temporary-file maximum age and `allow_shared_filesystem` (SPEC 6.1.1).
- `node`: `Node::init_cluster`, `Node::open` and `apply_document`, which enforces the same cluster and version plus one and saves before switching (SPEC 6.2.6).
- `local_ops`: `PutShard` reserving and answering `READY` then checking each frame's stripe number, `GetShard` streaming blocks with their stored checksums without verifying them, and `LocalList`.
- A connection span carrying peer, kind and node id, a request span carrying id, operation and key, a panic hook, and `--log-format json` (SPEC 20.4).

### Changed

- The same-filesystem check refuses two devices on one `st_dev`, and `allow_shared_filesystem` turns that refusal into a logged warning for tests and single-machine experiments (SPEC 5.3).

## [a411e25] - 2026-09-18

Direct commit: Specification: logging conventions (20.4)

### Added

- The logging conventions in `SPEC.md` (SPEC 20.4).

## [01d627d] - 2026-09-18

Direct commit: README: note why the default port is 5263

### Added

- A note in the `README.md` saying why the default port is 5263.

## [9719a99] - 2026-09-18

Direct commit: Specification: node configuration is TOML; default port 5263

### Changed

- `SPEC.md` decides that node configuration is TOML and that the default port is 5263.

## [ce5b96d] - 2026-09-18

Pull request #8: Wrap the metadata record with a checksum of its canonical form

### Added

- A checksum over the record's canonical form, so a record on disk is protected as a block is: XXH3-64 over JSON with keys sorted bytewise, no whitespace and absent optional fields omitted, modelled on RFC 8785 for the value types a record uses. Before this, a corrupt copy showed only as disagreement between the k+m copies, which cannot say which copy is wrong and cannot be found by a local scrub.
- `canonical_bytes()` and `checksum()` as public methods for the scrubber and for repair.

### Changed

- `MetadataRecord::to_json` computes and wraps the checksum and `from_json` unwraps, verifies, then validates, so the file stays readable when reformatted or when its keys are reordered while any change to a value is caught.

## [703fff2] - 2026-09-18

Pull request #7: Add the protocol crate and the cluster document type

### Added

- `djbod-proto`, a runtime-agnostic crate turning messages into bytes and back.
- `frame`: the 12-byte little-endian header, rejecting an unknown message type, non-zero flags and a payload length above 64 MiB + 4096 from the header alone, before any payload is read or allocated; `Frame::decode` reports `Incomplete { have, needed }`.
- `codec`: message bodies in Concise Binary Object Representation (CBOR) through `ciborium` and serde.
- `message`: `Request` and `Response` covering every operation of SPEC 19.1.3, `ErrorDetail` with the fields SPEC 16.2 requires, `DataFrame` costing exactly 16 bytes over its block, and `StreamEnd`.
- `djbod-core::cluster`: `ClusterDocument` with node and device entries, device states, the `device` independence level and the sanity checks of SPEC 6.2.4.

### Changed

- `SPEC.md` writes out framing (19.1.2) and the handshake (19.1.5) as decided, and decides CBOR and tokio.

### Removed

- The cluster secret and `HelloProof`, after review: the plain `Hello` carrying protocol version, peer kind, node id, cluster id and document version catches every misconfiguration the secret was meant to catch, the keyed proof was incomplete against a deliberate actor on the local network, and clients were unauthenticated in any case. SPEC 19.1.6 now fixes the intended design as per-node certificates with fingerprints in the cluster document.

## [77fe2e4] - 2026-09-18

Direct commit: Specification: note listing at scale as an open question (15.2.1)

### Added

- Listing at scale as an open question in `SPEC.md` (SPEC 15.2.1).

## [54049f5] - 2026-09-17

Pull request #6: Add the device layer

### Added

- `Device::initialise`, which requires a directory empty apart from `lost+found`, writes `DISTRIBUTED-JBOD-DEVICE.json` atomically with the system name, a plain-English notice, the format version, a fresh device UUID, the cluster id and a timestamp, and creates `objects/default` (SPEC 5.2, 5.2.1).
- `Device::open`, which tells an uninitialised directory, a `ForeignDirectory` holding `objects/` but no identity file, a corrupt identity file and a device from another cluster apart (SPEC 5.4).
- `begin_shard`, which creates the key directory and a `.tmp` file, reserves the exact final length with `fallocate` and writes the header, with `finish` fsyncing and renaming into place and `abort` or a drop removing the temporary (SPEC 9.3.3, 10.6).
- `write_record`, `read_records`, `read_record`, `open_shard`, `delete_version`, `walk_records` and `cleanup_temporaries`, the last removing `.tmp` files older than a cutoff (SPEC 10.11, 14.2).
- `free_space(headroom)` from `statvfs` available bytes less a fraction of the total (SPEC 5.5).

### Changed

- `SPEC.md` names the identity file and its notice, states the empty-directory rule, and records tokio as the async runtime with the alternatives considered.

## [feec79a] - 2026-09-17

Direct commit: Specification: content length required, fallocate reservation, and temporary file cleanup decided

### Changed

- `SPEC.md` decides that a content length is required, that space is reserved with `fallocate`, and how temporary files are cleaned up.

## [7d7a023] - 2026-09-17

Direct commit: Specification: record milestone 1 as complete

### Changed

- `SPEC.md` records milestone 1 as complete.

## [d142139] - 2026-09-17

Pull request #5: Add the metadata record, on-disk layout names, and ULID text form

### Added

- `record`: `MetadataRecord`, the JSON document describing one version, written as indented JSON so a disk can be searched with ordinary tools, with validation of the system name, the format version, the key hash against the key, the scheme, the block size and the shard list (SPEC 9.4, 9.1.6). `DeviceId` is a universally unique identifier (UUID) newtype.
- `layout`: the directory `objects/<bucket>/ab/cd/<hash>/` and the file names `<version>.meta.json` and `<version>.<index>.shard`, with parsers for both (SPEC 9.1).
- The 26-character Crockford base32 text form of a universally unique lexicographically sortable identifier (ULID), checked to sort in the same order as the bytes so sorting file names sorts by creation time (SPEC 9.2.3).
- Serde implementations for `KeyHash`, `BlockChecksum` and `VersionId`.

### Changed

- The record's shard list is `{ index, device }` with no node UUID, because a device's owning node is in the cluster document and can change when a disk moves between machines; recorded as SPEC 9.4.2.1 for review.

## [6dc4077] - 2026-09-17

Pull request #4: Add key hash, version id, and shard file modules

### Added

- `keyhash`: SHA-256 of the key's raw bytes with its hex rendering and the `ab/cd/<full>` directory components, pinned to published SHA-256 vectors (SPEC 9.1.2, 9.1.3).
- `version`: `VersionId`, the 16-byte version identifier as a value.
- `shardfile`: the reader and writer for shard file format version 1, with a 4 KiB header, blocks at `4096 + i * B`, a footer holding the checksum table, and a 16-byte trailer under the invariant `F + L + 16 == file length` (SPEC 9.3.2).
- A whole-object XXH3-64 checksum in the footer, so the recovery tool has the value without the metadata record (SPEC 8.3.6).
- A geometry cross-check on `finish` and on `open`, so too few stripes or a wrongly padded final stripe cannot produce a valid-looking file.

### Changed

- `ShardFileReader::open` verifies the trailer invariant, the footer checksum, the header checksum and magic, and that header, footer and trailer agree on where the blocks are, but leaves block checksums to the stripe decoder, which keeps opening cheap.
- `ShardFileWriter` recomputes each appended block's checksum and refuses a block whose supplied checksum disagrees (SPEC 17.6), and allows only the final block to be short.

## [c81be85] - 2026-09-17

Direct commit: Specification: shard file format v1 with header, footer, and trailer

### Added

- Shard file format version 1 in `SPEC.md`, with its header, footer and trailer.

## [078c922] - 2026-09-17

Direct commit: Specification: one file per shard decided; defer shard file size cap and client-as-coordinator

### Changed

- `SPEC.md` decides one file per shard, and defers the shard file size cap and the client acting as its own coordinator.

## [9c2a517] - 2026-09-17

Direct commit: Specification: SHA-256 decided for the key hash; spell out collision handling

### Changed

- `SPEC.md` decides SHA-256 for the key hash and spells out what happens on a collision.

## [09b7aec] - 2026-09-17

Pull request #3: Rename ReceivedBlock to ShardBlock; encode_stripe returns shard blocks

### Changed

- `ReceivedBlock` is renamed `ShardBlock`, one block of one shard holding its shard index, its bytes and their checksum, and its `stored_checksum` field becomes `checksum`. The old name described the moment of arrival, but the same unit is what the encoder produces, what a device stores and what the decoder verifies.
- `encode_stripe` returns `Vec<ShardBlock>` in shard index order, so its output feeds `decode_stripe` directly and the tests need no conversion helper.

### Removed

- The `EncodedStripe` struct, with its parallel block and checksum vectors and a `data_len` the caller already knew.

## [b78395c] - 2026-09-17

Pull request #2: decode_stripe returns a three-variant DecodedStripe

### Added

- `DecodedStripe::Intact`, `Repaired` and `Unrecoverable`, so the read path can match `Intact` alone and the repair job can take both the recovered data and the list of shards to rewrite. Before this, a damaged stripe came back as an `Ok` with a non-empty fault list, and the fail-stop rule of SPEC 11.4 depended on remembering to check it.
- A test, `the_three_outcomes_of_decoding`, showing one stripe decoded three ways and that `Err` is for misuse alone.

### Changed

- `StripeError` is reserved for misuse: an empty stripe, or indices out of range, duplicated or not requested.

### Removed

- `StripeError::Unrecoverable` and the old `DecodedStripe` struct.

## [8c4b032] - 2026-09-17

Pull request #1: decode_stripe takes the list of requested shard indices

### Changed

- `decode_stripe` judges faults against the list of shard indices the caller asked for: a requested index with no block is missing, and an index that was never requested is not mentioned. The read path fetches only the k data blocks (SPEC 11.3), so without this every successful read would report the parity as missing.
- A received block that was not requested is a protocol error, as is a duplicate or out-of-range entry in the requested list.

## [1cba456] - 2026-09-17

Direct commit: Rename Coder to ReedSolomonCode

### Changed

- `Coder` is renamed `ReedSolomonCode`, naming it for what it is: the Reed-Solomon code for one scheme, wrapping the library's `ReedSolomon` object with shard indices, input validation and the m = 0 case.

## [7a077dc] - 2026-09-17

Direct commit: Add a step-by-step 6+2 walkthrough test

### Added

- A walkthrough test that builds six data blocks, computes parity, checksums all eight shards, flips one bit in shard 2, finds it by checksum, reconstructs it from the other seven, and confirms the result against the stored checksum.
- A second test that does the same through the stripe layer and shows that faults are reported against the whole scheme, including blocks that were never requested.

## [6df8c58] - 2026-09-17

Direct commit: Add checksum, erasure, and stripe modules to djbod-core

### Added

- `checksum`: the XXH3-64 block checksum, fixed by format version 1 (SPEC 8.3.2, now decided).
- `erasure`: a validated `Scheme`, a `ShardIndex`, and a `Coder` that is the only caller of the positional `reed-solomon-erasure` interface and handles m = 0 itself.
- `stripe`: `encode_stripe` splits a stripe into checksummed blocks with a padded final stripe, and `decode_stripe` verifies every received block, treats a mismatch, a wrong length and an absence alike as an erasure, reconstructs when k blocks are usable, and reports every fault by shard index.
- Tests over every scheme and damage pattern up to m, m + 1 failures, swapped blocks, a corrupt checksum table entry, protocol errors and fault ordering.

### Changed

- The workspace is formatted with `rustfmt`, and clippy's `needless_range_loop` is allowed, since explicit index loops are the project style.

## [a0d6813] - 2026-09-17

Direct commit: Build shard vectors with loops instead of map and extend

### Changed

- The erasure tests build shard vectors with explicit loops instead of `map` and `extend`.

## [bd53239] - 2026-09-17

Direct commit: Remove the codec helper; use plain expect messages

### Removed

- The `codec` helper from the erasure tests, in favour of plain `expect` messages.

## [329460c] - 2026-09-17

Direct commit: Name the scheme in erasure test failure messages

### Changed

- The erasure tests replace `expect("valid scheme")` and bare unwraps with messages that state k, m and, where it matters, the shard length, through a `codec(k, m)` helper.

## [c419bdb] - 2026-09-17

Direct commit: Rename pseudo_random to xorshift64_bytes and write it as a plain loop

### Changed

- `pseudo_random` is renamed `xorshift64_bytes` and written as a plain loop, since a closure that ignores its argument and only mutates captured state reads worse than the loop it stands in for.

### Removed

- The `target-rust-analyzer` build directory added by mistake in `ceccfca`, now ignored by `.gitignore`.

## [ceccfca] - 2026-09-17

Direct commit: Rename pseudo_random to xorshift64_bytes and write it as a plain loop

### Added

- A `target-rust-analyzer` build directory, committed by mistake. The rename the message describes is not in this commit; it lands in `c419bdb`, which also removes the directory.

## [7e3fbce] - 2026-09-16

Direct commit: Add Cargo workspace and in-memory tests of the erasure coding library

### Added

- The Cargo workspace and the `djbod-core` crate.
- Tests of `reed-solomon-erasure` 6.0.0 against the assumptions of SPEC 8: systematic encoding, recovery from every erasure pattern up to m, refusal of m+1 erasures, trust of unmarked corrupt shards, byte-wise independence, parity rows that stay stable as m grows (SPEC 8.1.5), k=1 as replication, m=0 rejected, and the 256-shard limit.
- An ignored test that measures throughput, and the findings recorded in `SPEC.md`.

## [042504c] - 2026-09-16

Direct commit: Record Rust as the implementation language and add the implementation plan

### Added

- Appendix C of `SPEC.md`: Rust as the implementation language, the crate layout, the candidate dependencies, four milestones and the testing stance.
- A pointer from the `README.md` to the specification.

## [fbec7af] - 2026-09-16

Direct commit: Specification draft 3: cluster secret, default bucket directory, PUT replaces

### Changed

- The cluster secret is in version 1, checked by a handshake on every node-to-node connection.
- The default bucket is a top-level directory under `objects/`, and the key alone is hashed.
- A `PUT` to a key that already exists replaces it.

## [b4c7f4b] - 2026-09-16

Direct commit: Revise design specification to draft 2 after review

### Added

- The re-encode procedure for changing k or m, and a per-operation specification of the native protocol.

### Changed

- Buckets, versioning, failure domains and the S3 layer are deferred; placement is most-free-first; one format serves objects of every size.
- Content length, the space reservation, shard file granularity, the key hash choice and the cluster secret are reopened as questions, each with a recommendation.

## [f3dc556] - 2026-09-16

Direct commit: Add design specification, draft 1

### Added

- `SPEC.md`, draft 1: symmetric nodes with no master, recorded placement by free space, broadcast lookup, fail-stop semantics, systematic Reed-Solomon coding with per-block checksums, replicated plain-text metadata, and the on-disk layout.
- A marking on every item as decided, proposed, open or deferred, so a reader can tell settled design from a question still open.

## [b1db55e] - 2026-09-16

Direct commit: Initial commit

### Added

- The repository, with a `README.md` naming the project.
