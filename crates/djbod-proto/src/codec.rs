//! CBOR encoding of control payloads (SPEC 19.1.2).

use serde::de::DeserializeOwned;
use serde::Serialize;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum CodecError {
    #[error("CBOR encode failed: {0}")]
    Encode(String),
    #[error("CBOR decode failed: {0}")]
    Decode(String),
}

pub fn encode_cbor<T: Serialize>(value: &T) -> Result<Vec<u8>, CodecError> {
    let mut bytes = Vec::new();
    ciborium::ser::into_writer(value, &mut bytes).map_err(|e| CodecError::Encode(e.to_string()))?;
    Ok(bytes)
}

pub fn decode_cbor<T: DeserializeOwned>(bytes: &[u8]) -> Result<T, CodecError> {
    ciborium::de::from_reader(bytes).map_err(|e| CodecError::Decode(e.to_string()))
}
