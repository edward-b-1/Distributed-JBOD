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
