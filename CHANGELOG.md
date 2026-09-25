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
