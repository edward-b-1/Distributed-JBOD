# Changelog

All notable changes to Distributed-JBOD are recorded in this file.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and the project follows [Semantic Versioning](https://semver.org/spec/v2.0.0.html).
The workspace `version` in `Cargo.toml` is the single source of the version
number and every crate inherits it. A build identifies itself as
`<version>+<git commit>`, which `djbod --version`, `djbod identity`,
`djbod status` and `djbod cluster show` print.

0.2.0 is the first tagged release. Everything before it was unreleased
development: the workspace carried version 0.1.0 from the first commit on
16 September 2026, nothing was tagged, and no build was published. Tags and
version increments start from 0.2.0. Entries name the pull request that made
the change, and the issue where one was reported; entries from the early
direct commits on `main` name the commit instead.

## [Unreleased]

## [0.2.0] - 2026-09-25

### Added

**Core format and erasure coding**

- The Cargo workspace and the first in-memory tests of the erasure coding library (7e3fbce).
- `djbod-core`'s `checksum` (XXH3-64), `erasure` (`Scheme`, `ShardIndex`, `ReedSolomonCode`) and `stripe` modules, with a step-by-step 6+2 walkthrough test (6df8c58, 7a077dc, 1cba456).
- `decode_stripe` judges faults against the list of shard indices the caller asked for, so a read that fetches only the k data blocks does not report the parity as missing (SPEC 11.3) (#1).
- `decode_stripe` returns `DecodedStripe::{Intact, Repaired, Unrecoverable}`, so a caller can tell an untouched stripe from one rebuilt from parity (#2).
- `ShardBlock`, one block of one shard with its index, its bytes and their checksum; `encode_stripe` returns shard blocks in index order (#3).
- `keyhash` (SHA-256 of the key with the `ab/cd/<hash>` directory components), `version` (`VersionId`, and its 26-character Crockford base32 universally unique lexicographically sortable identifier (ULID) text form), and `shardfile`, the reader and writer for shard file format version 1 with its header, footer and trailer (SPEC 9.1, 9.2.3, 9.3.2) (#4, #5).
- `MetadataRecord`, the JSON document describing one version, with validation of the key hash, the scheme, the block size and the shard list, and `layout`, the on-disk directory and file names; `DeviceId` is a universally unique identifier (UUID) newtype (SPEC 9.1, 9.4) (#5).
- A checksum over the record's canonical form, so a record on disk is protected as a block is (SPEC 8.3) (#8).
- The device layer: `Device::initialise` writes `DISTRIBUTED-JBOD-DEVICE.json` atomically into an empty directory, and `Device::open` checks it and tells an uninitialised directory, a foreign directory and a device from another cluster apart (SPEC 5.2, 5.4) (#6).
- `djbod-proto`: the 12-byte frame header rejecting unknown types and oversized payloads before any allocation, message bodies in Concise Binary Object Representation (CBOR), and the cluster document type (SPEC 10) (#7).
- `MetadataRecord.revision`, 0 at first write and incremented by every re-placement, with the amended read rule and `MetadataRecord::same_body` (SPEC 18.8.1) (#22).
- The specification decisions taken directly on `main`: SHA-256 for the key hash with collision handling, one file per shard, shard file format v1, a required content length with a `fallocate` reservation and temporary-file cleanup, node configuration in TOML on default port 5263, the logging conventions of 20.4, and drain as a single pass that cannot loop (9c2a517, 078c922, c81be85, feec79a, 9719a99, a411e25, 70427eb, c201faa, 77fe2e4, cc1c114, a5ce980, 7d7a023).

**Node and cluster document**

- `djbod-node`: the per-node TOML configuration, device and cluster document state, and a server answering every node-to-node operation of SPEC 19.1.3 over TCP, with connection and request spans (#9).
- The coordinator, serving `Status`, `PutObject`, `GetObject`, `HeadObject`, `DeleteObject` and `ListKeys` by fanning node-to-node operations out over loopback, so one node and twenty take the same code path (SPEC 4.1, 13, 15.1) (#10).
- Cluster membership without a master: `propose` checks that every node holds the current version and applies the next in document order, stopping at the first refusal, with `join`, startup adoption, `sync`, and `djbod cluster show` (SPEC 6.2.6, 18.1) (#18).
- Device labels and node labels in the cluster document, 1 to 128 bytes, free of whitespace, not shaped like a UUID and unique, set with `djbod cluster set-label` and `djbod cluster set-node-label`; every command that names a device or a node accepts a label in place of the UUID (SPEC 6.2.5.1) (#41, #71).
- A human-readable cluster name beside the cluster id, set by `djbod-node init-cluster --name` or `djbod cluster set-name` (#81).
- `djbod cluster set-address` changes a node's address after it has joined, and startup adoption proposes the node's own advertised address when the document disagrees (SPEC 18.1.2.1) (#77).
- `max_key_bytes`, `max_object_bytes` and `max_user_metadata_bytes` in the cluster document, with `djbod-node init-cluster --max-key-bytes --max-object-bytes` and `djbod cluster set-limits`; bounded records and paged listings follow from them (SPEC 6.2.2, 21.3) (#27).
- Every node setting from a command-line argument, an environment variable or the configuration file, in that order, across `init-cluster`, `join`, `add-device`, `run` and `scrub` (SPEC 20.6) (#38).

**Client, CLI and libraries**

- `djbod`, the command-line client, driving a node from a shell with `put`, `get`, `head`, `delete`, `list`, `status` and the `cluster` subcommands (#11).
- The `djbod-client` crate, holding the wire, connection and streaming object code that had lived inside `djbod-node` (#110).
- The Rust client API: `ClientOptions::new` takes several node addresses tried in order, one connection is kept and a failure moves the next request to the next address, only requests that are safe to repeat are retried, and the cluster id is learned from the first node that answers when it is not given (SPEC 19.1.5.1) (#111).
- `djbod` built on the client library, with `--node` and `DJBOD_NODE` taking a comma-separated list of addresses and no command building raw requests any more (#114).
- Administration in the client library as `djbod_client::admin`: every change to the cluster document and the procedures built on them (SPEC 6.2.6) (#115).
- `Client::move_shard`, `Client::scrub` and `Client::drain`, the last three operations the library lacked; `scrub` and `drain` return an `EventRun` that yields events until the stream ends (#120).
- `djbod get-cluster-id` and `djbod identity` ask a node who it is, through a handshake in which a client sends the nil UUID as the cluster id and is answered with a `Hello` and nothing else (SPEC 19.1.5.1) (#105).
- `DeviceContents` and `djbod contents` count the versions, keys, blocks and shard bytes on a device from its records, without reading a shard (SPEC 19.1.3) (#119).
- `djbod status` reports the build of the node that answered and of every node, with a `NODE BUILD` column on each device row (SPEC 19.1.5) (#179).

**Administration: drain, repair, removal**

- `RepairObject` and `djbod repair <key>` rebuild damaged or missing shards from the intact ones, after reading every stripe of every readable shard and refusing to write anything when more than m shards are damaged in any stripe (SPEC 18.4.1) (#14).
- Repair rewrites the record copies a device has lost when the copies that remain agree, come from devices the record lists, and number at least k, reporting them as `record_copies_rewritten` (SPEC 9.4.4) (#20).
- `MoveShard` and `djbod move-shard <key> <index> [--to <device>]` re-place one shard, copying it from the source when it is intact and rebuilding it from the others otherwise (SPEC 18.8.2) (#22).
- `djbod cluster set-state` and `djbod cluster drain`, the single pass of SPEC 18.2.1 with the out-of-room estimate of 18.2.2 (#23).
- `djbod cluster remove-device`, `remove-node` and `remove-node --force`, with the reference scan of SPEC 18.5, the forced-removal plan of 6.2.6.3, and `--wipe-removed-device` on `join`, `add-device` and `init-cluster` (#24).
- `djbod cluster remove-device --force` retires a device the cluster cannot read: it reports the device's state, says what marking it removed means, warns when fewer than k+m active devices would remain, and asks for the device id to be typed back unless `--yes` (SPEC 18.2.1.1) (#204, issue #170).
- `djbod cluster set-scheme` and `djbod cluster reencode`, changing the global scheme in the document and then migrating existing objects as a streamed GET into a PUT per version (SPEC 18.9) (#26).

**Scrub and failure handling**

- The local scrub engine in `djbod-core`, which reads one device directly with no network and checks every record and every shard file, and the offline `djbod-node scrub` that runs it on one machine (SPEC 20.1) (#16).
- The cluster-wide `djbod scrub`: `LocalScrub` streams each finding from the node that holds the disks, the coordinator relays events as they arrive, and repairs are issued from one place behind the write-collision guard (SPEC 20.1.2, 20.1.2.1) (#19).
- Four exit codes for `djbod scrub`, one per outcome, in place of the single code 2 for both damage and an unfinished run (#164, issue #156).
- `CrossCheckStopped`, naming the node that ended the cross-node phase, the reason, and how many keys were checked and how many were not (#158, issue #153).
- `shard_present` on each streamed record, gathered with one `stat` as the page is built, so the cross-node phase can judge a version from the merged group alone (#162, issue #152).

**TLS (Transport Layer Security)**

- `djbod_node::transport`: material loaded from PEM files, a plain or TLS stream behind one read and write type, and an accept that looks at the first byte to tell a handshake from a plain frame (SPEC 19.1.6) (#35).
- `transport` in the cluster document with the modes `plain`, `tls-optional` and `tls`, set by `djbod cluster set-transport`, mutual TLS between nodes, and a node that refuses to start or to adopt a document under a TLS transport without material (#35).
- `djbod --tls-ca`, `--tls-cert` and `--tls-key`, with `DJBOD_TLS_CA`, `DJBOD_TLS_CERT` and `DJBOD_TLS_KEY`: no authority means plain, an authority alone means an encrypted connection with no client certificate, and all three mean mutual TLS (SPEC 19.1.6.2) (#37).
- `Connector::from_client_options`, the one place that turns the three client TLS settings into a connector for both `djbod` and `djbod-ui` (#45).
- `scripts/djbod-pki.sh`, issuing a certificate authority, node certificates and client certificates with `openssl`; djbod itself generates no keys and signs nothing (SPEC 19.1.6.1) (#40).

**Web UI**

- `djbod-ui`, a crate serving one embedded page and a JSON API over a local HTTP port, where every call is one node operation or one membership procedure, `PutObject` and `GetObject` stream through without being held, and `Scrub` and `Drain` are relayed as newline-delimited JSON events (SPEC 20.3.1) (#36).
- An upload checked against the devices' free space before its bytes are sent, with a progress meter, a rate, a time left and a cancel button, and the node's refusal delivered to the browser instead of a network failure (SPEC 10.4) (#42, #43).
- A Verify button that reads an object through the node without saving it, so every block and the whole object are checked, and names the device, shard and stripe of any damage (SPEC 11.7) (#48).
- An in-memory note per key of the last read the node stopped for damage, shown on the object panel and marked in the list until a read, verify or repair of that key succeeds (#51, #53).
- Device and node labels shown first with the UUID in parentheses wherever the page names one, and a Label button on both tables (#52, #54, #76).
- Device endpoints accepting a label as well as a UUID, resolved as every `djbod` command resolves one (#60).
- The cluster's transport in the header and in an Overview tile, with whether this server's own connection to the node uses TLS (#59).
- Each node's build and the UI's own, with the Nodes tile saying how many builds are in use and a banner when they differ (#79, #82).
- A health card at the top of the Overview, answering whether every node holds the document's version, naming the nodes that do not, and offering the sync (SPEC 6.2.6) (#83).
- A Nodes tab holding one nodes table with every check and each node's devices, leaving the Overview as the summary (#89, #92).
- Navigation as a left rail with live counts, collapsible to icons and remembered per browser, with a head row carrying the title and the toggle (#64, #90, #103).
- A dark theme switch in the header, remembered per browser and applied before the first paint, falling back to the system preference (#109).
- A progress bar for the scrub, advancing by device as each one is reported, with the bytes read and the rate (#72).
- The key table scrolling in its own box with its header held in place, paged 100 keys at a time with Previous and Next over the node's own cursor (SPEC 15.2.1) (#106, #107).
- The Nodes and Devices tiles on the Overview open the Nodes tab (#91).
- Status marks drawn in CSS as a dot, a square, a triangle and a ring, so shape carries the meaning as well as colour (#96).
- The address that was tried and the reason, in plain words, when the UI cannot reach its node (#206, #208, issue #177).
- `--bootstrap-node` and `DJBOD_BOOTSTRAP_NODE`, a list of addresses tried in order, with fallback to the other nodes named in the cluster document (#207, issue #176).
- The application icon in the browser tab, the page header and the README (#80).
- Upload controls as the first row with the prefix filter and its Clear button beneath, a blue List button, and destructive buttons filled red (#70, #74, #104).
- An explanation in the Download button's tooltip of why a download that meets damage is cut short, pointing at Repair (#44).

**Recovery tool**

- The `djbod-recover` crate, depending on `djbod-core` alone: `list` walks a device's tree directly, even one that has lost its identity file, and reports every version with its present shards and whether k sound shard files remain; `extract` writes an object back out from the shards on the disks it is given (SPEC 20.2, 20.2.2) (#25).

**Python package**

- The `djbod` Python package, `djbod_client::blocking::Client` wrapped with PyO3 and built by maturin as an `abi3` extension module, so one wheel serves Python 3.10 and later, with `scripts/python-tests.sh` (#112).

**Documentation and licence**

- The design specification `SPEC.md`, drafts 1 to 3 (f3dc556, b4c7f4b, fbec7af), and Rust recorded as the implementation language with the implementation plan (042504c).
- A getting-started guide for a single-machine cluster, saying where `node.toml` lives (9d903cb, 79206fc).
- The TLS walkthrough in the getting-started guide: one certificate authority with `openssl`, node and client certificates, installing the files, moving a cluster from `plain` to `tls-optional` to `tls`, joining under TLS, and rotating the authority with a two-root bundle (#39).
- The specification records each milestone's design before it is built: milestone 3 (#17), milestone 4 (#21), the TLS design and the configuration-source rule (#34), the web UI's browser link and its checks (#65), and the cross-node scrub as a merge of per-device record streams with the one-connection-per-collection rule of 15.2.3 (#160).
- Scrub history recorded as an open question, since a scrub's findings live only in the stream sent to the client that asked for it (SPEC 20.1.4) (#49).
- `docs/proposals/damage-marks.md`, weighing where the store should remember damaged shards and recommending a per-node ledger, with the note that the UI's stopgap is found by polling and can be late (#50, #56).
- The README rewritten for the person who will run the system: the problem it solves, a quick start, what you get, the tools, security, and a typical three-machine deployment (#63, #66, #68, #88, #118, 01d627d, 6131634).
- A README section on why not MinIO, Garage, SeaweedFS or Ceph, and what none of them offer (#87).
- A README section on how failures show up and where the line is drawn, including the statement that there is no alerting subsystem and none is planned (#188).
- `LICENSE` with the text of the GNU Affero General Public License (AGPL) version 3, `THIRD-PARTY-NOTICES`, and `scripts/third-party-notices.py` that generates it (#86).
- A logo and brand kit under `docs/brand`, applied as the header lockup, the favicon, the application icon and the README image, with one-line lockups in both spellings of the name (#94, #95, #99, #100).
- Six alternative logo concepts and the colour exploration sheets kept under `docs/brand`, and the four design comparisons behind the UI's choices under `docs/design` (#97, #98, #150).

### Changed

- Reads reconstruct a damaged block from parity instead of failing: the coordinator opens the parity shards, decodes the stripe from any k good blocks, verifies it, delivers it, writes nothing to any device, and reports what it rebuilt in the terminating `StreamEnd` (SPEC 11.4) (#197, issue #196).
- A read treats a shard it cannot open at all as erased from the first stripe, so it succeeds while at most m shards are out and reports the ranges of stripes it rebuilt; more than m is refused before any block of the body (SPEC 11.4) (#200, issue #199).
- A node starts without a device whose configured path is missing or holds neither identity file nor objects tree, serving its other devices and reporting the missing one as unavailable, because a disk failure is a device failure and not a node failure (SPEC 5.6) (#186, issue #173).
- Writes place around devices the cluster cannot read: a write makes its directories with a plain `mkdir` and treats a missing parent as unavailable, so no write ever recreates a device's tree on whatever filesystem sits at its path (SPEC 5.6) (#185, #186).
- An unavailable device makes a scrub incomplete rather than damaged, and a node that cannot be asked is unchecked rather than a finding; a key that could not be checked is never queued for repair (#158, #185).
- `Status` reports a node it cannot reach with `reachable: false` and the reason, and lists that node's devices from the document as unavailable, instead of failing the whole request on the first unreachable node (SPEC 19.1.3, 5.6) (#211, issue #209).
- A node refuses a cluster document carrying a field its build does not know, on the wire and when reading its own `cluster.json`, rather than dropping the field and saving a version it cannot represent (SPEC 6.2.6.4) (#78).
- Fields that were optional only so that an older build's message could be read are now required, on the wire and in the cluster document: `Hello.build`, `LocalStatus.build`, `NodeStatus.build`, `Status.transport`, `LocalStatus.tls_ready`, and the document's limits and transport. There is no installed base and no compatibility is kept with unreleased builds (SPEC 19.1.5.2, 6.2.2) (#183, #184).
- The licence is AGPL-3.0-only, declared in the workspace `Cargo.toml` and inherited by every crate; it had said MIT while the repository carried no licence text at all (#86).
- `coordinator::list_keys` sends the client's cursor and page size down to every node and takes one page from each, so a page costs one page per node instead of moving every key in the cluster (#32, issue #29).
- `LocalRecords` pages in key hash order and reads only the records it returns, so a device is walked once rather than once per page (#163, issue #121).
- The cross-node scrub phase is a merge of one paged record stream per device, held open for the phase and judged group by group, with no lookup and no per-key probe (SPEC 15.2.2, 15.2.3) (#160, #162, issue #152).
- Membership failures carry the status that says whose fault they are: 502 for a node that could not be reached or a change that got part way round, 404 for something that does not exist, and 409 for a refusal by the store's state (#58).
- Every error type follows the workspace's `<Thing>Error` convention: `AdminError`, `MembershipError`, `ConnectionError` and `ClientError` (#116, #117).
- The specification says device, or node, throughout; the undefined word "holder", used about forty times for both, is gone (#159).
- The web UI's object list pages with Previous and Next instead of a More button that appended keys without end (#106).
- The logo blue matches the UI's accent rather than the cyan chosen before there was anything to match, and the parity slabs are yellow across the kits, with the glowing status-light lockup in the web UI header (#102, #108).
- `djbod-ui` takes `--bootstrap-node`; `--node` and `DJBOD_NODE` remain as the deprecated spelling and print a notice on stderr, with the new spelling winning when both are given (#207).
- The mixed-builds banner in the web UI is shorter and no longer repeats what a node does with a document it cannot represent (#82).

### Fixed

- `PutObject` hung under the TLS transport because `EndOfStream` frames were never flushed: `write_message` now flushes after every write, which covers every frame on every path, and a stream that goes silent is abandoned (SPEC 10.12) (#75).
- `remove-device` and `remove-node` refuse an active device, or a node with any active device, closing the window in which a client write could place a shard between the reference scan and the proposal (#31, issue #28).
- A drain reports a version deleted or replaced during the pass as deleted, rather than as a stale copy and a failure of the run (#33, issue #30).
- Devices sharing a filesystem produce one warning naming them all, instead of a warning for every pair at `init-cluster` and again at `run` (#12).
- Log field names are written plainly as `name=value` instead of in the terminal's italics, with the level colour kept (#15).
- Command-line tables size every column across its header and all displayed rows, so long labels, addresses, build strings and certificate names no longer push later columns away from their headers (#134, issue #132).
- Click-to-copy works over plain HTTP, where `navigator.clipboard` does not exist, and reports "copied" only when a copy succeeded (#47).
- Opening the Objects section no longer scrolls the page to the key table, which the URL fragment had been treating as a scroll target (#67).
- Opening the record JSON no longer widens the object panel and squeezes the key list; the JSON scrolls sideways in its own box (#55).
- The unhyphenated inline lockups carried the hyphenated `<title>`, so their accessible name contradicted the artwork (#101).
- The command-line tests spawn the binaries with every `DJBOD_*` variable removed, so a shell that has sourced a cluster's settings no longer fails `get_cluster_id_needs_no_cluster_id` (#212).

### Removed

- `layout::DEVICE_IDENTITY_FILE`, which still named `device.json`; the identity file is `DISTRIBUTED-JBOD-DEVICE.json` and `device::DEVICE_IDENTITY_FILE` is the one definition (#149, issue #148).
- The `EncodedStripe` struct, with its parallel block and checksum vectors and a `data_len` the caller already knew (#3).
- The document version from the web UI header, since it is the version held by whichever node answered and the Nodes table and the health card already report it per node (#85).
- The "older, unreported" build wording in `status`, `cluster show` and the web UI, `Identity.build` as an optional field in the Rust and Python clients, and the command-line fallback that recovered a cluster id from an old node's refusal (#183).

### Security

- The web UI refuses any method but GET or HEAD that does not carry `Sec-Fetch-Site: same-origin` or `none`, and refuses any request naming a host it does not serve, in one middleware on every route (SPEC 20.3.2) (#57).
- A banner warns while a link is unencrypted: red while the cluster's transport is `plain`, amber while it is `tls-optional`, and red while the page itself is served over plain HTTP from anywhere but the machine the server runs on (#61).
- `TlsMaterial::load` refuses a private key file readable by anyone but its owner (SPEC 19.1.6.2) (#35).
- The specification records that the web UI's browser link is plain HTTP with no authentication and that its same-origin and host checks are the browser's own word and not authentication, with the two routes to encrypting it left as an open question (SPEC 20.3.2, 20.3.3, 21.4) (#65).

[Unreleased]: https://github.com/edward-b-1/Distributed-JBOD/compare/v0.2.0...HEAD
[0.2.0]: https://github.com/edward-b-1/Distributed-JBOD/releases/tag/v0.2.0
