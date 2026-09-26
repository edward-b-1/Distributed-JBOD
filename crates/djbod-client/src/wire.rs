//! Frames over tokio streams.

use std::time::Duration;

use thiserror::Error;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use djbod_proto::frame::{Frame, FrameError, FrameHeader, MessageType, HEADER_LEN};
use djbod_proto::message::{Message, MessageError};

#[derive(Debug, Error)]
pub enum WireError {
    #[error("connection closed")]
    Closed,
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Frame(#[from] FrameError),
    #[error(transparent)]
    Message(#[from] MessageError),
    #[error("no frame arrived for {} seconds; the stream was abandoned (SPEC 10.12)", .0.as_secs())]
    Idle(Duration),
    /// A well-framed request whose payload this build cannot decode, with
    /// the request id to answer on.
    #[error("request {id} cannot be decoded: {reason}")]
    UndecodableRequest { id: u32, reason: String },
}

/// Read one message, or fail with `Idle` if none arrives within `idle`.
/// For the receiving side of a block stream: a sender that stops mid-way
/// must not leave the receiver waiting forever.
pub async fn read_message_within<R: AsyncRead + Unpin>(
    reader: &mut R,
    idle: Duration,
) -> Result<Message, WireError> {
    match tokio::time::timeout(idle, read_message(reader)).await {
        Ok(result) => result,
        Err(_) => Err(WireError::Idle(idle)),
    }
}

/// Read one frame. `Closed` if the peer hung up cleanly between frames.
pub async fn read_frame<R: AsyncRead + Unpin>(reader: &mut R) -> Result<Frame, WireError> {
    let mut header_bytes = [0u8; HEADER_LEN];
    match reader.read_exact(&mut header_bytes).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Err(WireError::Closed),
        Err(e) => return Err(WireError::Io(e)),
    }
    let header = FrameHeader::decode(&header_bytes)?;
    let mut payload = vec![0u8; header.payload_len as usize];
    reader.read_exact(&mut payload).await?;
    Ok(Frame { header, payload })
}

pub async fn read_message<R: AsyncRead + Unpin>(reader: &mut R) -> Result<Message, WireError> {
    let frame = read_frame(reader).await?;
    match Message::from_frame(&frame) {
        Ok(message) => Ok(message),
        Err(MessageError::Codec(e)) if frame.header.message_type == MessageType::Request => {
            Err(WireError::UndecodableRequest {
                id: frame.header.request_id,
                reason: e.to_string(),
            })
        }
        Err(e) => Err(e.into()),
    }
}

pub async fn write_message<W: AsyncWrite + Unpin>(
    writer: &mut W,
    message: &Message,
) -> Result<(), WireError> {
    let bytes = message.encode()?;
    writer.write_all(&bytes).await?;
    // A plain TCP stream has nothing to flush. A TLS stream does: rustls
    // may accept the plaintext into its own buffer while the socket would
    // block, and nothing sends those encrypted bytes until the next write
    // or a flush. Without this, the last frame of a conversation (an
    // EndOfStream after many blocks) could sit unsent while both sides
    // waited on each other.
    writer.flush().await?;
    Ok(())
}
