//! The frame: a 12-byte header and a payload (SPEC 19.1.2).
//!
//! ```text
//! 0   2   message type    u16
//! 2   2   flags           u16, 0 in protocol version 1
//! 4   4   request id      u32, correlates responses and streams with requests
//! 8   4   payload length  u32
//! 12      payload
//! ```
//!
//! Little-endian, like the shard file.

use thiserror::Error;

pub const HEADER_LEN: usize = 12;

/// The largest payload a peer will accept. The largest data frame is one
/// block of the largest permitted block size (SPEC 6.2.4) plus its 16-byte
/// prefix; control payloads are far smaller. A length above this is a
/// protocol violation, not an allocation.
pub const MAX_PAYLOAD_LEN: u32 = 64 * 1024 * 1024 + 4096;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u16)]
pub enum MessageType {
    /// Handshake, first message from each side.
    Hello = 1,
    /// Handshake, second message from each side.
    HelloProof = 2,
    Request = 3,
    Response = 4,
    /// One chunk of a stream: 16-byte prefix and raw bytes.
    Data = 5,
    /// Terminates a stream, carrying its status.
    EndOfStream = 6,
}

impl MessageType {
    pub fn from_u16(value: u16) -> Option<MessageType> {
        match value {
            1 => Some(MessageType::Hello),
            2 => Some(MessageType::HelloProof),
            3 => Some(MessageType::Request),
            4 => Some(MessageType::Response),
            5 => Some(MessageType::Data),
            6 => Some(MessageType::EndOfStream),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameHeader {
    pub message_type: MessageType,
    pub flags: u16,
    pub request_id: u32,
    pub payload_len: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub header: FrameHeader,
    pub payload: Vec<u8>,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum FrameError {
    /// Not enough bytes yet; `needed` is the total the frame requires.
    #[error("frame incomplete: have {have} bytes, need {needed}")]
    Incomplete { have: usize, needed: usize },
    #[error("unknown message type {0}")]
    UnknownMessageType(u16),
    #[error("payload length {0} exceeds the maximum of {MAX_PAYLOAD_LEN}")]
    PayloadTooLarge(u32),
    #[error("flags {0:#06x} are not zero; unknown protocol extension")]
    UnknownFlags(u16),
}

impl FrameHeader {
    pub fn encode(&self) -> [u8; HEADER_LEN] {
        let mut out = [0u8; HEADER_LEN];
        out[0..2].copy_from_slice(&(self.message_type as u16).to_le_bytes());
        out[2..4].copy_from_slice(&self.flags.to_le_bytes());
        out[4..8].copy_from_slice(&self.request_id.to_le_bytes());
        out[8..12].copy_from_slice(&self.payload_len.to_le_bytes());
        out
    }

    /// Parse a header. Rejects unknown types, non-zero flags, and
    /// oversized lengths before any payload is read.
    pub fn decode(bytes: &[u8]) -> Result<FrameHeader, FrameError> {
        if bytes.len() < HEADER_LEN {
            return Err(FrameError::Incomplete {
                have: bytes.len(),
                needed: HEADER_LEN,
            });
        }
        let raw_type = u16::from_le_bytes([bytes[0], bytes[1]]);
        let message_type =
            MessageType::from_u16(raw_type).ok_or(FrameError::UnknownMessageType(raw_type))?;
        let flags = u16::from_le_bytes([bytes[2], bytes[3]]);
        if flags != 0 {
            return Err(FrameError::UnknownFlags(flags));
        }
        let request_id = u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]);
        let payload_len = u32::from_le_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]);
        if payload_len > MAX_PAYLOAD_LEN {
            return Err(FrameError::PayloadTooLarge(payload_len));
        }
        Ok(FrameHeader {
            message_type,
            flags,
            request_id,
            payload_len,
        })
    }
}

impl Frame {
    pub fn new(message_type: MessageType, request_id: u32, payload: Vec<u8>) -> Frame {
        Frame {
            header: FrameHeader {
                message_type,
                flags: 0,
                request_id,
                payload_len: payload.len() as u32,
            },
            payload,
        }
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(HEADER_LEN + self.payload.len());
        out.extend_from_slice(&self.header.encode());
        out.extend_from_slice(&self.payload);
        out
    }

    /// Decode one frame from the front of `bytes`. Returns the frame and
    /// how many bytes it consumed, or `Incomplete` with the total needed so
    /// a reader knows how much more to fetch.
    pub fn decode(bytes: &[u8]) -> Result<(Frame, usize), FrameError> {
        let header = FrameHeader::decode(bytes)?;
        let total = HEADER_LEN + header.payload_len as usize;
        if bytes.len() < total {
            return Err(FrameError::Incomplete {
                have: bytes.len(),
                needed: total,
            });
        }
        let payload = bytes[HEADER_LEN..total].to_vec();
        Ok((Frame { header, payload }, total))
    }
}
