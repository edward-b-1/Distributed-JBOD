# Names that read as the things they count

Status: audit of `main` at `40b0938`, 24 September 2026. This is a catalog. It renames nothing.

Open work is tracked in #202.

The review of #185 asked that a count of unreadable devices not be called `unavailable`, because that name reads as the devices themselves, or as a structure describing them. That rename landed as `unavailable_count` in `crates/djbod-cli/src/main.rs` (line 611), in `2fda3b3`, together with `create_key_directory` and the cross-node merge's `unread` set. `ObjectWrite.unavailable` (`crates/djbod-proto/src/message.rs:720`) is `Vec<UnavailableDevice>` and really is that list. It is the shape the count had been imitating.

A name is listed below when a reader can take it for the things themselves — a list, a record, or a yes/no — and the value is only how many there are. The sharp cases are the ones where the same word is already a collection somewhere else. Line numbers are from `40b0938`.

The search covered the Rust crates, the Python bindings (they surface the serde names), the status page script, and the tests. Measures that already say they are measures are collected at the end, as the spelling to copy.

## 1. The closest remaining case

`Node::unavailable_devices` (`crates/djbod-node/src/node.rs:192`) returns `Vec<DeviceId>`, the devices the node could not open.

The scrub command in the CLI counts the same idea under the same words:

```rust
let mut unavailable_devices = 0usize;
```

`crates/djbod-cli/src/main.rs:804`. It is incremented once per unavailable device (line 816) and printed as `{unavailable_devices} device(s) unavailable` (line 919). A matching name is `unavailable_device_count`.

## 2. Public counts with collection names

`DeviceContents` and `DrainEvent::Estimate` are serialized. The field names are the CBOR keys, the JSON of `djbod contents --json` and of the drain `estimate` event, and the keys of the dict Python's `device_contents` returns. Tests already assert `after["versions"]` and `after["keys"]`. SPEC 18.2.3 describes the contents figures as "the number of versions, of distinct keys, of blocks, and the shard bytes" and does not fix the spellings. Renaming these four fields changes the wire.

| Where | Name | What it holds | A name that matches |
|---|---|---|---|
| `crates/djbod-proto/src/message.rs:316` | `DeviceContents.versions` | `u64`, `records.len()` | `version_count` |
| `message.rs:318` | `DeviceContents.keys` | `u64`, the size of a set | `key_count` |
| `message.rs:320` | `DeviceContents.blocks` | `u64`, blocks summed from the records | `block_count` |
| `message.rs:626` | `DrainEvent::Estimate.versions` | `u64`, `records.len()` | `version_count` |

On `DeviceContents` the sibling field is already `shard_bytes`. The function that fills the struct (`crates/djbod-node/src/coordinator.rs:492`) keeps `keys` as a `BTreeSet<&str>` and then stores `keys.len()` in the field `keys`. The local `blocks` in that function (line 493) is the `u64` the field receives. Elsewhere `versions` is a `Vec` (`crates/djbod-core/src/device.rs:491`, and `versions_of` at `coordinator.rs:349`), `keys` is `Vec<KeyEntry>` on `ListPage`, and `blocks` is the block buffers in the stripe code.

On the drain estimate, `shard_bytes`, `active_devices`, and `required_devices` already read as measurements. `versions` is the field that does not. (`active_devices` and `required_devices` are plural and are counts, but every field of that variant is a measurement, so they are easier to read than `versions`.)

Three library names have the same shape. They are Rust fields, not JSON keys. `VersionReference` is not `Serialize`. The two errors are shown through `Display`, which already says "version(s)" and "active device(s)".

| Where | Name | What it holds | A name that matches |
|---|---|---|---|
| `crates/djbod-client/src/admin.rs:777` | `VersionReference.shards` | `usize`. The doc comment says "How many of the version's shards are on those devices." | `shard_count` |
| `admin.rs:106` | `StillReferenced.versions` | `usize`, `references.len()`. `examples` on the same variant is the sample of keys. | `version_count` |
| `admin.rs:138` | `TooFewActiveDevices.active` | `usize` | `active_device_count` |

`MetadataRecord.shards` and `RepairReport.shards` (`message.rs:393`) are the shard lists. The forced-removal text prints the count as `r.shards`. `active` is also the `Vec<&DeviceStatus>` in the drain handler (`coordinator.rs:2597`).

## 3. The same word is a collection in one place and a count in another

**`findings`.** `CrossCheckOutcome.findings` (`coordinator.rs:3499`) is a `usize`. `ScrubSummary.findings` (`crates/djbod-core/src/scrub.rs:126`) is `Vec<Finding>`. `check_group` builds `let mut findings = Vec::new()` (`coordinator.rs:3660`). The scrub handler already uses `finding_count` for its own counter (`coordinator.rs:3182`) and then adds `outcome.findings` into it.

The CLI repeats the collision. `main.rs:801` is `let mut findings = 0usize`, passed into `scrub_exit_code` whose parameter is also `findings: usize` (`main.rs:431`), and the same match prints `summary.findings.len()`. `repairs` beside it (`main.rs:802`) is the same kind of counter. The status page does the same pair: `let findings = 0, repairs = 0` at `crates/djbod-ui/ui.html:1213`, and `e.summary.findings.length` at line 1244. Matching names are `finding_count` and `repair_count`. The counter already called `repair_failures` can stay.

**`active`.** `Node::open` (`crates/djbod-node/src/node.rs:348`) sets `active` from `.count()` of devices in the `Active` state, then logs that number as `active_devices`. The drain path's `active` (`coordinator.rs:2597`) is the `Vec<&DeviceStatus>`. A matching name for the count is `active_device_count`.

**`received`.** In `stream_body_to_writers` (`coordinator.rs:1456`) `received` is a `u64` of body bytes, compared with `record.size`. In `read_stripe_blocks` (`coordinator.rs:807`), the object read (`coordinator.rs:942`), and `read_repair_stripe` (`coordinator.rs:3020`), `received` is `Vec<ShardBlock>`. A matching name for the byte total is `received_bytes`.

**`unrecoverable`.** `djbod-recover` (`crates/djbod-recover/src/main.rs:244`) uses it as a `usize` of versions that cannot be rebuilt. `ForcedRemovalPlan::unrecoverable` (`admin.rs:1005`) returns `Vec<&VersionReference>`, and the CLI calls `.is_empty()` and `.len()` on that (`main.rs:1675`). A matching name for the recover counter is `unrecoverable_count`.

**`present`.** In `djbod-recover`'s list (`recover/src/main.rs:248`) `present` is how many shard files were found for the version, printed as `{present}/{total}`. `check_group` takes `present: &BTreeMap<DeviceId, bool>` (`coordinator.rs:3657`). A matching name for the recover tally is `present_count`.

**`skipped`, next to `moved` and `deleted`.** The CLI drain (`main.rs:1746`) keeps `moved` and `deleted` as `usize` counters and `skipped` as `Vec<(String, String)>`, the keys and the reasons. The node's drain handler counts skipped versions as `let mut skipped = 0usize` (`coordinator.rs:2651`). The status page (`ui.html:1278`) uses `moved`, `skipped`, and `deleted` as three counters. Matching names for the counters are `moved_count`, `deleted_count`, and `skipped_count`, with the CLI's vector left as the list of skipped keys.

**`rewritten`.** The repair command prints `let rewritten = report.shards.iter().filter(|s| s.rewritten).count()` (`main.rs:891`). The same `.count()` in the repair-report printer is already bound to `count` (`main.rs:1012`). The repair implementation collects `let rewritten: Vec<ShardIndex>` (`coordinator.rs:2035`). A matching name for the CLI counter is `rewritten_count`.

## 4. Further locals of the same shape

Each of these is a count, from `.count()`, `.len()`, or `+= 1`, under a name that reads as the things or as a yes/no.

| Where | Name | What it holds | A name that matches |
|---|---|---|---|
| `main.rs:563` | `empty` | how many content rows have `versions == 0` | `empty_count` |
| `crates/djbod-ui/src/lib.rs:808` | `with_room` | how many active devices can hold the shard | `devices_with_room`, already the JSON key at line 820 |
| `coordinator.rs:1739` | `vouching` | how many record copies agree enough to trust | `vouching_copies` |
| `coordinator.rs:3181` | `failed_nodes` | how many local scrubs failed. The next line is `finding_count`. | `failed_node_count` |
| `main.rs:1708` | `failed` | how many forced-removal repairs failed | `failed_count` |
| `main.rs:1550` | `failures` | how many re-encodes failed. The function returns this number. | `failure_count` |
| `crates/djbod-core/src/shardfile.rs:463` | `actual_blocks` | `self.checksums.len() as u64`, compared with `block_count`. The error variant `GeometryMismatch.actual_blocks` (`shardfile.rs:189`) sits beside `expected_blocks`. | `actual_block_count` |
| `ui/src/lib.rs:1014` | `Progress.unreported` | bytes accumulated since the last progress line | `unreported_bytes` |
| `coordinator.rs:939` and `ui/src/lib.rs:901` | `delivered` | bytes of the object body written so far. The mismatch error already says `delivered {delivered} bytes`. | `delivered_bytes` |

`count_versions_behind` and `reencode_all` also keep `behind`, `examined`, and `reencoded` as bare counters (`main.rs:1501` and `1548`). The function names carry the meaning, so these are the same habit at lower urgency. `failures` in that second function is the row above.

## 5. A quantity name holding a sentence

`coordinator.rs:2623`: `shortfall` is `Option<String>`. `Some` is the sentence printed when a drain cannot place every version ("N active device(s), but every version needs …", or a byte shortfall written out in words). `None` means the drain has room. The name reads as the number of devices or bytes short. The binding is then used as `reason` (`if let Some(reason) = shortfall`). A matching name is `shortfall_reason`.

## 6. Nearby mismatches

These are a different shape. The name still points at an entity, and the value is something else.

`Node.versions` (`node.rs:99`) and `Node::versions()` (`node.rs:164`) hold a `VersionGenerator`, the clock that mints version ids. Every other `versions` in the tree is the stored version records or a count of them. A matching name is `version_generator`.

`Client.next_node` (`crates/djbod-client/src/client.rs:190`) is a `usize` index into `options.nodes`. The comment says where the next reconnect starts. A matching name is `next_node_index`.

`Node::connections_accepted` (`node.rs:431`) returns `(u64, u64)`. The comment says the pair is plain, then TLS. The two counters are indistinguishable from the type. A named pair, `plain` and `tls`, would carry that.

## 7. Tests

`crates/djbod-node/tests/node.rs:594` binds `leftovers` to `Vec<String>`, the names left in a directory. Lines 645 and 872 bind `leftovers` to `read_dir(...).count()`, a number of entries, asserted equal to 0. A matching name for the counts is `leftover_count`. The coordinator test binds the same expression to `count`.

## 8. Names that already match the value

Worth copying: `unavailable_count`, `finding_count`, `repair_failures`, `records_checked`, `shards_checked`, `blocks_checked`, `versions_checked`, `versions_unchecked`, `shard_bytes`, `block_count` on the shard footer, and `ObjectWrite.unavailable` for the list of devices the write went around.
