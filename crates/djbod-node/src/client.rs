//! A connection to a node, from the point of view of whoever opened it:
//! the coordinator talking to a holder, the command-line tool, or a test.

use std::net::SocketAddr;

use thiserror::Error;
use tokio::io::BufReader;
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
        let id = self
            .send_request(Request::PutObject {
                key: key.to_string(),
                size: body.len() as u64,
                content_type,
                user_metadata: Default::default(),
            })
            .await?;
        for (sequence, piece) in body.chunks(chunk.max(1)).enumerate() {
            self.send_data(id, body_frame(sequence as u64, piece.to_vec()))
                .await?;
        }
        self.send_end(id, StreamEnd::ok()).await?;
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

    /// Download a whole object into memory, verifying each body chunk in
    /// transit and requiring the stream to end cleanly (which is where
    /// the coordinator reports the whole-object check, SPEC 11.7).
    pub async fn get_object(
        &mut self,
        key: &str,
    ) -> Result<(djbod_core::record::MetadataRecord, Vec<u8>), ClientError> {
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
        let mut body = Vec::with_capacity(record.size as usize);
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
                    body.extend_from_slice(&data.bytes);
                }
                StreamItem::End(end) => {
                    if let Some(error) = end.error {
                        return Err(ClientError::StreamFailed(error));
                    }
                    return Ok((record, body));
                }
            }
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
