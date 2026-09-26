# Distributed-JBOD user guide

This guide is for the person who runs the machines and stores the data.
It covers how to build and configure a cluster, three ways to lay the
machines out, every command, and the actions to take when a disk or a
machine fails.

The [README](../../README.md) is the introduction. [SPEC.md](../../SPEC.md)
is the design document for people changing the software. Procedures in
this guide were checked against the implementation, including runs on one
machine with directories standing in for disks, and then read again
against main at `1b8e434` (version 0.2.5), where reads, listings, and
`status` learned to go around a disk or a machine they cannot reach.
Where a command's result depends on free space or on which disks an
object landed on, the guide says what was observed and what to look for
on your own cluster.

## Read this first

A Distributed-JBOD cluster does not repair itself. A bad block is reported
when something reads it. A disk that disappears, or a machine that is
switched off, is named on the commands that had to work around it, and
putting the data back onto other disks is a command you run after you
have seen what it will cost.

Two facts decide most of the procedures later in this guide:

- The node your client connects to is the coordinator for that request.
  It fans the work out to the other machines, encodes and decodes, and
  streams the object. You choose it. On a cluster whose machines differ
  in CPU, link speed, or region, point `--node` at a machine that can
  do that work well. [Concepts](concepts.md#which-node-coordinates) is
  the full account. The web UI's `--bootstrap-node` is the same choice,
  and once it has read the cluster document it will try the other
  listed nodes if your first choices do not answer.
- A disk the node can no longer read stays `active` until you change
  that. `djbod status` prints `active, unavailable`. A `get` that still
  has `k` record copies and at most `m` unreadable shards returns the
  bytes and exits 2, naming what it went without. `repair` does not move
  those shards onto other disks while the dead disk is still a member.
  Retire it with `djbod cluster remove-device --force` and rebuild with
  `djbod scrub --repair`. A new write skips that disk. It stores the
  object and exits 2 when `k + m` other devices are usable, and it
  stores nothing when they are not. A whole machine that is off is the
  same kind of gap for a read that still has those copies, and a harder
  stop for writes: `put`, `delete`, and `repair` refuse with
  `NodeUnreachable` until that machine answers or you remove it.
  `status` still prints, and names the machine.

[Concepts](concepts.md) defines the words those two paragraphs use.
[Setup](setup.md) builds a cluster on one machine. [Deployment](deployment.md)
is the same work on real disks. [Day to day](day-to-day.md) is storing and
fetching objects, and the web UI. [TLS](tls.md) is optional and separate.
[Commands](commands.md) is the reference. [Scenarios](scenarios.md) is the
runbook.

## The programs

| Program | Role |
|---|---|
| `djbod-node` | One process per machine. Creates the cluster, joins a machine, adds a disk, and serves. |
| `djbod` | The client and the administration tool. Every object operation and every membership change. |
| `djbod-ui` | A browser page for the same administration, bound to localhost unless you say otherwise. |
| `djbod-recover` | Reads device directories with no node running and writes an object back to a file. |
| `scripts/djbod-pki.sh` | Creates the certificate authority and the node and client certificates used by [TLS](tls.md). |

From a program, the same client operations are the `djbod-client` Rust
crate and the `djbod` Python package. The Python package's own readme is
`crates/djbod-python/README.md`.

## What this guide leaves to SPEC.md

The byte layout of a shard file, the erasure-coding mathematics, and the
frame protocol are in [SPEC.md](../../SPEC.md). You can operate a cluster
without them. You do need the on-disk picture in [Setup](setup.md#what-is-on-a-device):
each disk holds a JSON record next to a shard file, and the record names
the key in plain text.
