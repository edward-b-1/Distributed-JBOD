# Distributed-JBOD

A distributed object store that aggregates the mismatched disks of several
commodity machines into one durable, bitrot-protected pool. Every node runs
the same process; there is no master.

The design is in [SPEC.md](SPEC.md). Implementation is in Rust; see
Appendix C of the specification for the crate layout and milestones.

## Default port

Nodes listen on TCP port **5263** by default. It spells JBOD on a telephone
keypad (J=5, B=2, O=6, D=3), and IANA lists it as unassigned. The first
candidate, 7400, turned out to be the DDS/RTPS discovery port used by ROS 2,
so it was dropped. See SPEC.md item 6.1.1.
