//! The native wire protocol, SPEC 19.1.
//!
//! This crate is runtime-agnostic: it turns messages into bytes and bytes
//! into messages. Moving the bytes over a socket is the node's and the
//! client's job. Three layers:
//!
//! - [`frame`]: the 12-byte header and the raw frame.
//! - [`handshake`]: the first exchange on every connection, proving both
//!   sides know the cluster secret without sending it (19.1.5).
//! - [`message`]: every request, response, data frame, and end-of-stream
//!   marker for the operations of 19.1.3, and the [`message::Message`]
//!   enum that ties them to frames.
//!
//! Control payloads are CBOR (19.1.2). Data payloads are a 16-byte prefix
//! and raw block bytes, with no serialization in between.

pub mod codec;
pub mod frame;
pub mod handshake;
pub mod message;

pub use frame::{Frame, FrameError, FrameHeader, MessageType, HEADER_LEN, MAX_PAYLOAD_LEN};
pub use message::{Message, Request, Response};
