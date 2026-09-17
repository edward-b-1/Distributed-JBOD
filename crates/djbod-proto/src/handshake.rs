//! The connection handshake (SPEC 19.1.5).
//!
//! Both sides send a `Hello` carrying a fresh random nonce, the cluster id,
//! their peer kind, and their cluster document version. A node peer then
//! sends a `HelloProof`: an HMAC-SHA256, keyed by the cluster secret, over
//! a role label and both nonces and the cluster id. Each side verifies the
//! other's proof. The secret never crosses the wire, a captured proof
//! cannot be replayed against a different nonce, and the role label stops
//! a proof being reflected back to the side that produced it.
//!
//! Clients are not authenticated in v1 (19.1.6); they send `Hello` with
//! `PeerKind::Client` and no proof.
//!
//! Nonce generation is the caller's job, so this module has no dependency
//! on a random source and every test is deterministic.

use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use uuid::Uuid;

use djbod_core::cluster::NodeId;

pub const NONCE_LEN: usize = 32;
pub const PROOF_LEN: usize = 32;
pub const PROTOCOL_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PeerKind {
    Node,
    Client,
}

/// Which end of the connection is proving itself. The initiator opened the
/// connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Initiator,
    Responder,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hello {
    pub protocol_version: u32,
    pub kind: PeerKind,
    /// Present for nodes, absent for clients.
    pub node_id: Option<NodeId>,
    pub cluster_id: Uuid,
    /// The cluster document version this peer holds; 0 for clients.
    pub document_version: u64,
    #[serde(with = "serde_bytes_32")]
    pub nonce: [u8; NONCE_LEN],
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HelloProof {
    #[serde(with = "serde_bytes_32")]
    pub proof: [u8; PROOF_LEN],
}

/// Compute the proof a peer in `role` sends, given both nonces.
pub fn compute_proof(
    secret: &[u8],
    role: Role,
    initiator_nonce: &[u8; NONCE_LEN],
    responder_nonce: &[u8; NONCE_LEN],
    cluster_id: Uuid,
) -> [u8; PROOF_LEN] {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret).expect("HMAC accepts any key length");
    mac.update(b"distributed-jbod handshake v1");
    mac.update(match role {
        Role::Initiator => b"initiator",
        Role::Responder => b"responder",
    });
    mac.update(initiator_nonce);
    mac.update(responder_nonce);
    mac.update(cluster_id.as_bytes());
    let result = mac.finalize().into_bytes();
    let mut proof = [0u8; PROOF_LEN];
    proof.copy_from_slice(&result);
    proof
}

/// Verify a proof received from a peer in `role`. Constant-time.
pub fn verify_proof(
    secret: &[u8],
    role: Role,
    initiator_nonce: &[u8; NONCE_LEN],
    responder_nonce: &[u8; NONCE_LEN],
    cluster_id: Uuid,
    proof: &[u8; PROOF_LEN],
) -> bool {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret).expect("HMAC accepts any key length");
    mac.update(b"distributed-jbod handshake v1");
    mac.update(match role {
        Role::Initiator => b"initiator",
        Role::Responder => b"responder",
    });
    mac.update(initiator_nonce);
    mac.update(responder_nonce);
    mac.update(cluster_id.as_bytes());
    mac.verify_slice(proof).is_ok()
}

/// Serde support for fixed 32-byte arrays as CBOR byte strings.
mod serde_bytes_32 {
    use serde::de::{Error as _, SeqAccess, Visitor};
    use serde::{Deserializer, Serializer};
    use std::fmt;

    pub fn serialize<S: Serializer>(bytes: &[u8; 32], serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_bytes(bytes)
    }

    struct Bytes32Visitor;

    impl<'de> Visitor<'de> for Bytes32Visitor {
        type Value = [u8; 32];

        fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
            f.write_str("32 bytes")
        }

        fn visit_bytes<E: serde::de::Error>(self, v: &[u8]) -> Result<[u8; 32], E> {
            if v.len() != 32 {
                return Err(E::custom(format!("expected 32 bytes, got {}", v.len())));
            }
            let mut out = [0u8; 32];
            out.copy_from_slice(v);
            Ok(out)
        }

        fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<[u8; 32], A::Error> {
            let mut out = [0u8; 32];
            for slot in out.iter_mut() {
                *slot = seq
                    .next_element()?
                    .ok_or_else(|| A::Error::custom("expected 32 bytes, got fewer"))?;
            }
            if seq.next_element::<u8>()?.is_some() {
                return Err(A::Error::custom("expected 32 bytes, got more"));
            }
            Ok(out)
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<[u8; 32], D::Error> {
        deserializer.deserialize_bytes(Bytes32Visitor)
    }
}
