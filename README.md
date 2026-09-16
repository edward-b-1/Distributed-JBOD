# Distributed-JBOD

A distributed object store that aggregates the mismatched disks of several
commodity machines into one durable, bitrot-protected pool. Every node runs
the same process; there is no master.

The design is in [SPEC.md](SPEC.md). Implementation is in Rust; see
Appendix C of the specification for the crate layout and milestones.
