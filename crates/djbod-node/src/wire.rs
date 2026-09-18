//! Frames over tokio streams.

use thiserror::Error;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use djbod_proto::frame::{Frame, FrameError, FrameHeader, HEADER_LEN};
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
    Ok(Message::from_frame(&frame)?)
}

pub async fn write_message<W: AsyncWrite + Unpin>(
    writer: &mut W,
    message: &Message,
) -> Result<(), WireError> {
    let bytes = message.encode()?;
    writer.write_all(&bytes).await?;
    Ok(())
}
