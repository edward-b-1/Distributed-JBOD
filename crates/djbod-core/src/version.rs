//! Version identifiers, SPEC 9.2.
//!
//! Every body written under a key receives a 128-bit, time-ordered
//! identifier (a ULID, 9.2.3), even though v1 keeps at most one version per
//! key. Generation belongs to the coordinator; this module only defines the
//! value as it appears in file names, records, and shard file headers.

/// A 128-bit version identifier.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct VersionId(pub [u8; 16]);

impl std::fmt::Debug for VersionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "VersionId(")?;
        for byte in &self.0 {
            write!(f, "{byte:02x}")?;
        }
        write!(f, ")")
    }
}
