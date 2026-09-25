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
