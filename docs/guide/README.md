# Distributed-JBOD user guide

This guide is for the person who runs the machines and stores the data.
It covers how to build and configure a cluster, three ways to lay the
machines out, every command, and the actions to take when a disk or a
machine fails.

The [README](../../README.md) is the introduction. [SPEC.md](../../SPEC.md)
is the design document for people changing the software. Procedures in
this guide were checked against the implementation at commit `79dac71`
(build `0.1.0+79dac71ec`), including runs on one machine with directories
standing in for disks. Where a command's result depends on free space or
on which disks an object landed on, the guide says what was observed and
what to look for on your own cluster.

## Read this first

A Distributed-JBOD cluster does not repair itself. A bad block is reported
when something reads it. A disk that disappears, or a machine that is
switched off, stops the operations that needed it. Putting the cluster
back is a command you run, after you have seen what it will cost.

Two facts decide most of the procedures later in this guide:

- Every node listed in the cluster has to answer. While one node is
  unreachable, `djbod status`, `djbod get`, and `djbod put` fail and name
  that node, including for objects that have no data on it. `djbod
  cluster show` still works and shows which node is down. Bring the node
  back, or retire it with `djbod cluster remove-node --force` if it is
  never coming back.
- A disk the node can no longer read stays listed as `active` until you
  change that. `djbod status` prints `active, unavailable`. Reads of
  objects that kept a record on that disk fail until you retire the disk
  with `djbod cluster remove-device --force` and then rebuild with
  `djbod scrub --repair`. A new write can go around the disk; `djbod put`
  stores the object and exits 2.

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
