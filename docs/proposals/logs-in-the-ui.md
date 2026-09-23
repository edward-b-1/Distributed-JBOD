# Proposal: recent logs over the protocol, and a Logs tab

Status: proposal, 19 September 2026, parked for later. Nothing here is in
SPEC.md; section 6 lists what would change there if it is adopted.

## 1. The want

An administrator at the web UI, or at `djbod` on a laptop, wants to see
what has happened on the cluster recently: which node changed the
document, which drain moved what, which request failed and why. Today
that means logging into each machine and reading its journal.

## 2. The obstacle

Nodes log through `tracing` to standard error (SPEC 20.4.3), and the
platform keeps the logs: systemd's journal, a container runtime, a
collector fed by `--log-format json`. That is the right arrangement for a
daemon, and the process writes no log files of its own. But the UI and
the client reach a node only through the native protocol, and usually
from another machine, so they have no way to a node's log. Reading a
remote journal would be platform-specific, need its own credentials, and
be the one place the UI bypassed the protocol, which 20.3.1 forbids: every
UI action is a native operation.

So the node has to offer its recent log itself, over the protocol.

## 3. The design

### 3.1 A ring buffer in every node

A second `tracing` layer, installed beside the stderr layer at startup,
appends every event that passes the level filter to an in-memory ring
buffer of the last N entries (proposed default 10 000, configurable).
An entry holds:

- a sequence number, increasing for the life of the process;
- the timestamp;
- the level and the target (the module that emitted it);
- the message;
- the event's own fields, and the fields of the spans it was emitted in
  (20.4.2), which is how an entry carries the peer address, request id,
  operation, and key without the emitting code changing.

Nothing about what is logged or where else it goes changes. The buffer is
lost when the process exits; the platform's copy is the durable one, and
this is a window onto the recent past, not an archive. Memory is a few
hundred bytes per entry, so a few megabytes per node at the default.

### 3.2 Two operations

`LocalLogs` (node to node): request the entries after a sequence number,
optionally at or above a level, up to a page. Response: the entries, the
node's current highest sequence number, and whether more follow. Paged
by bytes like the other listings (15.2.2). A node that restarted has
sequence numbers starting again; it also returns a process start id (its
startup time is enough) so a client can tell a restart from a gap.

`Logs` (client): the coordinator sends `LocalLogs` to every node in the
document with a per-node cursor supplied by the client, merges the
entries by timestamp, tags each with its node id, and answers with the
merged page and the new per-node cursors. A node that cannot be reached
is reported in the response and the others' entries are still returned:
unlike a read, a log view is more useful partial than absent, and this is
a diagnostic, not a data operation, so fail-stop (16.1) does not apply.

`TailLogs` (client, optional in the first version): the streamed form,
on the pattern of `Scrub`, delivering entries as they arrive until the
client closes. Polling `Logs` every two seconds gives the same experience
with less machinery; the streamed form saves the polling.

### 3.3 The tab and the command

The UI gains a Logs tab: the merged entries newest first, a filter by
node and by level, a text search over message and key, a follow toggle
that polls, and a click on a key that jumps to the object. Every field of
an entry is shown as data, never rendered as HTML.

Because every UI action is also a command (20.3.1), `djbod logs` comes
with it: `--node`, `--level`, `--since <cursor>`, `--follow`, `--json`.
It is useful on its own when the nodes are elsewhere.

### 3.4 What the log shows at the default level

At `info` (20.4.4) the buffer holds lifecycle events: connections ending,
document changes, drains and moves, and every `warn` and `error`. That is
"what happened on this cluster in the last hour", which is the question
the tab answers. Per-request flow is at `debug` and stays there. A later
addition would let the buffer's level differ from stderr's, or be changed
at run time per node, so a node can be watched closely from the UI
without restarting it.

## 4. Costs and cautions

- Memory: N entries per node, a few megabytes at the default.
- One layer in the node; two (or three) protocol operations; a merge in
  the coordinator; a command; a tab. About one milestone step.
- Ordering across nodes is by each node's clock, so the merged view has
  the same clock-skew caveat as version ids.
- Entries contain object keys and client addresses. With TLS the
  operation is available to any authenticated client, like every other
  operation, since there is no authorisation yet (3.12). Worth a sentence
  in the spec when adopted.
- A very chatty node (`RUST_LOG=trace`) overwrites its buffer in seconds;
  the tab shows what is there and says how far back it reaches.

## 5. Alternatives considered

- **Read the platform's logs.** `journalctl -o json` on each machine.
  Rejected: platform-specific, remote access needs its own credentials,
  and it bypasses the protocol.
- **A log file per node served over the protocol.** More history than a
  ring buffer, but 20.4.3 deliberately writes no log files and does no
  rotation. Could be offered as an option later; the ring buffer needs no
  such change.
- **A central collector.** The right answer for a large installation,
  and `--log-format json` already feeds one. Out of scope for a home
  cluster that wants to work with nothing else installed.

## 6. Changes to SPEC.md if adopted

- 20.4: a new item for the ring-buffer layer, its size, and its level.
- 19.1.3: `LocalLogs`, `Logs`, and `TailLogs` if built.
- 20.3.1: the Logs tab.
- 3.12 or 19.1.6: that logs are readable by any authenticated client.
- C.4: a milestone step.

## 7. Open questions

1. Buffer size: entries or bytes? Entries are simple; bytes bound the
   memory regardless of message length.
2. Should `Logs` include the UI's own server log, or only the nodes'? The
   UI is a client and its log is on the machine the administrator is
   already using; probably not.
3. Whether to build `TailLogs` in the first version or poll.
