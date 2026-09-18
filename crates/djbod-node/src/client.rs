//! A connection to a node, from the point of view of whoever opened it:
//! the coordinator talking to a holder, the command-line tool, or a test.

use std::net::SocketAddr;

use thiserror::Error;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::TcpStream;
use uuid::Uuid;

use djbod_core::checksum::checksum_block;
use djbod_core::erasure::ShardIndex;
use djbod_core::stripe::ShardBlock;
use djbod_proto::handshake::{Hello, HelloError, PeerKind, PROTOCOL_VERSION};
use djbod_proto::message::{DataFrame, ErrorDetail, Message, Request, Response, StreamEnd};

use crate::wire::{read_message, write_message, WireError};

#[derive(Debug, Error)]
pub enum ClientError {
    #[error(transparent)]
    Wire(#[from] WireError),
    #[error("peer refused the connection: {0}")]
    Hello(#[from] HelloError),
    #[error("peer sent {got} when {expected} was expected")]
    UnexpectedMessage { expected: &'static str, got: String },
    #[error("peer answered request {expected} with a message for request {got}")]
    WrongRequestId { expected: u32, got: u32 },
    #[error("node reported an error: {0:?}")]
    Remote(ErrorDetail),
    #[error("stream ended with an error: {0:?}")]
    StreamFailed(ErrorDetail),
}

/// One item of an incoming stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamItem {
    Data(DataFrame),
    End(StreamEnd),
}

pub struct Connection {
    reader: BufReader<OwnedReadHalf>,
    writer: OwnedWriteHalf,
    peer_hello: Hello,
    next_request_id: u32,
}

impl Connection {
    /// Connect, send our `Hello`, and read the peer's. The peer's `Hello`
    /// is checked against the cluster id we expect; a node peer's document
    /// version is not checked here, since a client has none to compare.
    pub async fn connect(addr: SocketAddr, our_hello: Hello) -> Result<Connection, ClientError> {
        let stream = TcpStream::connect(addr).await.map_err(WireError::Io)?;
        stream.set_nodelay(true).map_err(WireError::Io)?;
        let (read_half, write_half) = stream.into_split();
        let mut connection = Connection {
            reader: BufReader::new(read_half),
            writer: write_half,
            peer_hello: our_hello.clone(),
            next_request_id: 1,
        };
        write_message(&mut connection.writer, &Message::Hello(our_hello.clone())).await?;
        match read_message(&mut connection.reader).await? {
            Message::Hello(hello) => {
                if hello.cluster_id != our_hello.cluster_id {
                    return Err(ClientError::Hello(HelloError::ClusterId {
                        peer: hello.cluster_id,
                        ours: our_hello.cluster_id,
                    }));
                }
                connection.peer_hello = hello;
            }
            Message::Response {
                response: Response::Error(detail),
                ..
            } => return Err(ClientError::Remote(detail)),
            other => {
                return Err(ClientError::UnexpectedMessage {
                    expected: "Hello",
                    got: describe(&other),
                })
            }
        }
        Ok(connection)
    }

    pub fn peer_hello(&self) -> &Hello {
        &self.peer_hello
    }

    /// The `Hello` a client (not a node) sends.
    pub fn client_hello(cluster_id: Uuid) -> Hello {
        Hello {
            protocol_version: PROTOCOL_VERSION,
            kind: PeerKind::Client,
            node_id: None,
            cluster_id,
            document_version: 0,
        }
    }

    fn allocate_request_id(&mut self) -> u32 {
        let id = self.next_request_id;
        self.next_request_id = self.next_request_id.wrapping_add(1).max(1);
        id
    }

    /// Send a request and return its id, for operations that stream.
    pub async fn send_request(&mut self, request: Request) -> Result<u32, ClientError> {
        let id = self.allocate_request_id();
        write_message(&mut self.writer, &Message::Request { id, request }).await?;
        Ok(id)
    }

    /// Read the response to request `id`. A `Response::Error` becomes
    /// `ClientError::Remote`.
    pub async fn read_response(&mut self, id: u32) -> Result<Response, ClientError> {
        match read_message(&mut self.reader).await? {
            Message::Response { id: got, response } => {
                if got != id {
                    return Err(ClientError::WrongRequestId { expected: id, got });
                }
                match response {
                    Response::Error(detail) => Err(ClientError::Remote(detail)),
                    other => Ok(other),
                }
            }
            other => Err(ClientError::UnexpectedMessage {
                expected: "Response",
                got: describe(&other),
            }),
        }
    }

    /// Send a request and read its single response.
    pub async fn request(&mut self, request: Request) -> Result<Response, ClientError> {
        let id = self.send_request(request).await?;
        self.read_response(id).await
    }

    pub async fn send_data(&mut self, id: u32, data: DataFrame) -> Result<(), ClientError> {
        write_message(&mut self.writer, &Message::Data { id, data }).await?;
        Ok(())
    }

    pub async fn send_end(&mut self, id: u32, end: StreamEnd) -> Result<(), ClientError> {
        write_message(&mut self.writer, &Message::EndOfStream { id, end }).await?;
        Ok(())
    }

    /// Read the next item of the stream for request `id`.
    pub async fn read_stream_item(&mut self, id: u32) -> Result<StreamItem, ClientError> {
        match read_message(&mut self.reader).await? {
            Message::Data { id: got, data } => {
                if got != id {
                    return Err(ClientError::WrongRequestId { expected: id, got });
                }
                Ok(StreamItem::Data(data))
            }
            Message::EndOfStream { id: got, end } => {
                if got != id {
                    return Err(ClientError::WrongRequestId { expected: id, got });
                }
                Ok(StreamItem::End(end))
            }
            other => Err(ClientError::UnexpectedMessage {
                expected: "Data or EndOfStream",
                got: describe(&other),
            }),
        }
    }

    /// The whole `PutShard` conversation: request, wait for READY, stream
    /// `blocks` in order as stripes 0.., end with the object size and
    /// checksum, wait for DONE.
    pub async fn put_shard(
        &mut self,
        request: Request,
        blocks: &[ShardBlock],
        object_size: u64,
        object_checksum: djbod_core::checksum::BlockChecksum,
    ) -> Result<(), ClientError> {
        let id = self.send_request(request).await?;
        match self.read_response(id).await? {
            Response::PutShardReady => {}
            other => {
                return Err(ClientError::UnexpectedMessage {
                    expected: "PutShardReady",
                    got: format!("{other:?}"),
                })
            }
        }
        for (stripe, block) in blocks.iter().enumerate() {
            self.send_data(
                id,
                DataFrame {
                    sequence: stripe as u64,
                    checksum: block.checksum,
                    bytes: block.bytes.clone(),
                },
            )
            .await?;
        }
        self.send_end(
            id,
            StreamEnd {
                error: None,
                object_size: Some(object_size),
                object_checksum: Some(object_checksum),
            },
        )
        .await?;
        match self.read_response(id).await? {
            Response::PutShardDone => Ok(()),
            other => Err(ClientError::UnexpectedMessage {
                expected: "PutShardDone",
                got: format!("{other:?}"),
            }),
        }
    }

    /// The whole `GetShard` conversation. Returns the blocks in stripe
    /// order with the checksums the holder stored for them; they are not
    /// verified here.
    pub async fn get_shard(
        &mut self,
        request: Request,
        shard_index: ShardIndex,
    ) -> Result<Vec<ShardBlock>, ClientError> {
        let id = self.send_request(request).await?;
        let block_count = match self.read_response(id).await? {
            Response::GetShard { block_count } => block_count,
            other => {
                return Err(ClientError::UnexpectedMessage {
                    expected: "GetShard",
                    got: format!("{other:?}"),
                })
            }
        };
        let mut blocks = Vec::with_capacity(block_count as usize);
        loop {
            match self.read_stream_item(id).await? {
                StreamItem::Data(data) => blocks.push(ShardBlock {
                    index: shard_index,
                    bytes: data.bytes,
                    checksum: data.checksum,
                }),
                StreamItem::End(end) => {
                    if let Some(error) = end.error {
                        return Err(ClientError::StreamFailed(error));
                    }
                    return Ok(blocks);
                }
            }
        }
    }
}

/// Body chunk size used by the streaming helpers: one frame per chunk.
pub const DEFAULT_BODY_CHUNK: usize = 1 << 20;

impl Connection {
    /// Upload a whole object held in memory, in body chunks of `chunk`
    /// bytes. Returns the version id.
    pub async fn put_object(
        &mut self,
        key: &str,
        body: &[u8],
        chunk: usize,
        content_type: Option<String>,
    ) -> Result<djbod_core::version::VersionId, ClientError> {
        let mut cursor = body;
        self.put_object_from_reader(key, body.len() as u64, &mut cursor, chunk, content_type)
            .await
    }

    /// Upload `size` bytes read from `source`, in body chunks of `chunk`
    /// bytes, holding at most one chunk in memory (SPEC 3.6). Returns the
    /// version id. Reading fewer than `size` bytes from `source` is an
    /// error the coordinator reports, since the declared size is binding.
    pub async fn put_object_from_reader<R: AsyncRead + Unpin>(
        &mut self,
        key: &str,
        size: u64,
        source: &mut R,
        chunk: usize,
        content_type: Option<String>,
    ) -> Result<djbod_core::version::VersionId, ClientError> {
        let id = self
            .send_request(Request::PutObject {
                key: key.to_string(),
                size,
                content_type,
                user_metadata: Default::default(),
            })
            .await?;
        let chunk = chunk.max(1);
        let mut remaining = size;
        let mut sequence: u64 = 0;
        let mut buffer = vec![0u8; chunk];
        while remaining > 0 {
            let want = (remaining as usize).min(chunk);
            let read = source
                .read(&mut buffer[..want])
                .await
                .map_err(WireError::Io)?;
            if read == 0 {
                // Source ended early. Tell the coordinator so it aborts the
                // holders, then report what it says.
                self.send_end(
                    id,
                    StreamEnd::failed(ErrorDetail::new(
                        djbod_proto::message::ErrorCode::WriteFailed,
                        format!("source ended with {remaining} of {size} bytes unsent"),
                    )),
                )
                .await?;
                break;
            }
            // The coordinator may refuse before the body is consumed and
            // close the connection while we are still writing. Then the
            // write fails, but its reason is already waiting to be read.
            if let Err(e) = self
                .send_data(id, body_frame(sequence, buffer[..read].to_vec()))
                .await
            {
                return Err(self.refusal_behind_write_error(e).await);
            }
            sequence += 1;
            remaining -= read as u64;
        }
        if remaining == 0 {
            if let Err(e) = self.send_end(id, StreamEnd::ok()).await {
                return Err(self.refusal_behind_write_error(e).await);
            }
        }
        // Success is a Response. A refusal after the body started flowing
        // arrives as an EndOfStream carrying the error, after which the
        // coordinator closes the connection.
        match read_message(&mut self.reader).await? {
            Message::Response {
                id: got,
                response: Response::PutObject { version },
            } if got == id => Ok(version),
            Message::Response {
                response: Response::Error(detail),
                ..
            } => Err(ClientError::Remote(detail)),
            Message::EndOfStream { end, .. } => {
                Err(ClientError::StreamFailed(end.error.unwrap_or_else(|| {
                    ErrorDetail::new(
                        djbod_proto::message::ErrorCode::ProtocolViolation,
                        "upload ended without a version",
                    )
                })))
            }
            other => Err(ClientError::UnexpectedMessage {
                expected: "PutObject",
                got: describe(&other),
            }),
        }
    }

    /// Download a whole object into memory. See `get_object_to_writer`.
    pub async fn get_object(
        &mut self,
        key: &str,
    ) -> Result<(djbod_core::record::MetadataRecord, Vec<u8>), ClientError> {
        let mut body = Vec::new();
        let record = self.get_object_to_writer(key, &mut body).await?;
        Ok((record, body))
    }

    /// Download an object, writing its body to `sink` as it arrives while
    /// verifying each chunk in transit, and requiring the stream to end
    /// cleanly, which is where the coordinator reports the whole-object
    /// check (SPEC 11.7). Bytes may already have been written to `sink`
    /// when an error is returned; the caller must discard them.
    pub async fn get_object_to_writer<W: AsyncWrite + Unpin>(
        &mut self,
        key: &str,
        sink: &mut W,
    ) -> Result<djbod_core::record::MetadataRecord, ClientError> {
        let id = self
            .send_request(Request::GetObject {
                key: key.to_string(),
            })
            .await?;
        let record = match self.read_response(id).await? {
            Response::GetObject { record } => record,
            other => {
                return Err(ClientError::UnexpectedMessage {
                    expected: "GetObject",
                    got: format!("{other:?}"),
                })
            }
        };
        let mut expected_sequence = 0;
        loop {
            match self.read_stream_item(id).await? {
                StreamItem::Data(data) => {
                    if data.sequence != expected_sequence
                        || checksum_block(&data.bytes) != data.checksum
                    {
                        return Err(ClientError::StreamFailed(ErrorDetail::new(
                            djbod_proto::message::ErrorCode::ProtocolViolation,
                            format!(
                                "body chunk {} out of order or corrupt in transit",
                                data.sequence
                            ),
                        )));
                    }
                    expected_sequence += 1;
                    sink.write_all(&data.bytes).await.map_err(WireError::Io)?;
                }
                StreamItem::End(end) => {
                    if let Some(error) = end.error {
                        return Err(ClientError::StreamFailed(error));
                    }
                    sink.flush().await.map_err(WireError::Io)?;
                    return Ok(record);
                }
            }
        }
    }
}

impl Connection {
    /// After a write failed mid-upload, read what the coordinator sent
    /// before closing, if anything, so the caller sees the refusal rather
    /// than a broken pipe.
    async fn refusal_behind_write_error(&mut self, write_error: ClientError) -> ClientError {
        match read_message(&mut self.reader).await {
            Ok(Message::EndOfStream { end, .. }) => {
                ClientError::StreamFailed(end.error.unwrap_or_else(|| {
                    ErrorDetail::new(
                        djbod_proto::message::ErrorCode::ProtocolViolation,
                        "upload ended without a version",
                    )
                }))
            }
            Ok(Message::Response {
                response: Response::Error(detail),
                ..
            }) => ClientError::Remote(detail),
            _ => write_error,
        }
    }
}

impl Connection {
    /// Start a cluster-wide scrub. Returns the request id; then call
    /// `next_scrub_event` until it yields the stream's end.
    pub async fn start_scrub(
        &mut self,
        max_bytes_per_second: Option<u64>,
        repair: bool,
    ) -> Result<u32, ClientError> {
        let id = self
            .send_request(Request::Scrub {
                max_bytes_per_second,
                repair,
            })
            .await?;
        match self.read_response(id).await? {
            Response::ScrubStarted => Ok(id),
            other => Err(ClientError::UnexpectedMessage {
                expected: "ScrubStarted",
                got: format!("{other:?}"),
            }),
        }
    }

    /// The next scrub event, or the stream's end.
    pub async fn next_scrub_event(
        &mut self,
        id: u32,
    ) -> Result<Result<djbod_proto::message::ScrubEvent, StreamEnd>, ClientError> {
        match self.read_stream_item(id).await? {
            StreamItem::Data(data) => {
                if checksum_block(&data.bytes) != data.checksum {
                    return Err(ClientError::StreamFailed(ErrorDetail::new(
                        djbod_proto::message::ErrorCode::ProtocolViolation,
                        "scrub event corrupt in transit",
                    )));
                }
                let event = djbod_proto::codec::decode_cbor(&data.bytes).map_err(|e| {
                    ClientError::StreamFailed(ErrorDetail::new(
                        djbod_proto::message::ErrorCode::ProtocolViolation,
                        e.to_string(),
                    ))
                })?;
                Ok(Ok(event))
            }
            StreamItem::End(end) => Ok(Err(end)),
        }
    }
}

/// A checksummed data frame for a chunk of object body.
pub fn body_frame(sequence: u64, bytes: Vec<u8>) -> DataFrame {
    DataFrame {
        sequence,
        checksum: checksum_block(&bytes),
        bytes,
    }
}

impl std::fmt::Debug for Connection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Connection")
            .field("peer_hello", &self.peer_hello)
            .field("next_request_id", &self.next_request_id)
            .finish_non_exhaustive()
    }
}

fn describe(message: &Message) -> String {
    match message {
        Message::Hello(_) => "Hello".to_string(),
        Message::Request { .. } => "Request".to_string(),
        Message::Response { .. } => "Response".to_string(),
        Message::Data { .. } => "Data".to_string(),
        Message::EndOfStream { .. } => "EndOfStream".to_string(),
    }
}
