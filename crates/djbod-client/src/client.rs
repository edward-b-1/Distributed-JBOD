//! The client a program uses (SPEC 20.8): several node addresses tried in
//! order, the cluster id learned from the first node that answers when it
//! is not given, one connection kept open, and a reconnection to the next
//! node when that connection fails. Every operation is one method; the
//! scrub and drain return an [`EventRun`] that yields their events.

use std::collections::BTreeMap;
use std::net::SocketAddr;

use thiserror::Error;
use tokio::io::{AsyncRead, AsyncWrite};
use uuid::Uuid;

use djbod_core::cluster::{ClusterDocument, NodeId, Transport};
use djbod_core::record::{DeviceId, MetadataRecord};
use djbod_core::version::VersionId;
use djbod_proto::message::{
    DeviceContents, DeviceStatus, DrainEvent, ErrorCode, ErrorDetail, KeyEntry, ListQuery,
    NodeStatus, RepairReport, Request, Response, ScrubEvent, StreamEnd,
};

use crate::connection::{Connection, ConnectionError, DEFAULT_BODY_CHUNK};
use crate::transport::Connector;

/// How to reach the cluster.
#[derive(Clone)]
pub struct ClientOptions {
    /// Node addresses, tried in this order; any node answers any request.
    pub nodes: Vec<SocketAddr>,
    /// The cluster id, or `None` to learn it from the first node that
    /// answers (19.1.5.1). Give it whenever it is known: with it, a node
    /// serving another cluster is refused before anything is done.
    pub cluster: Option<Uuid>,
    /// Plain, or TLS with the client's material (19.1.6.2).
    pub connector: Connector,
    /// Bytes per body frame when streaming an object in either direction.
    pub body_chunk: usize,
}

impl ClientOptions {
    pub fn new(nodes: Vec<SocketAddr>) -> ClientOptions {
        ClientOptions {
            nodes,
            cluster: None,
            connector: Connector::plain(),
            body_chunk: DEFAULT_BODY_CHUNK,
        }
    }

    pub fn cluster(mut self, cluster: Uuid) -> ClientOptions {
        self.cluster = Some(cluster);
        self
    }

    pub fn connector(mut self, connector: Connector) -> ClientOptions {
        self.connector = connector;
        self
    }
}

#[derive(Debug, Error)]
pub enum ClientError {
    #[error("no node addresses were given")]
    NoNodes,
    #[error("no node could be reached: {}", describe_attempts(.0))]
    Unreachable(Vec<(SocketAddr, String)>),
    /// Boxed: a node's `ErrorDetail` is large, and errors are rare.
    #[error(transparent)]
    Connection(Box<ConnectionError>),
    #[error("the node answered {got} to {request}")]
    UnexpectedResponse { request: &'static str, got: String },
}

impl From<ConnectionError> for ClientError {
    fn from(e: ConnectionError) -> ClientError {
        ClientError::Connection(Box::new(e))
    }
}

fn describe_attempts(attempts: &[(SocketAddr, String)]) -> String {
    attempts
        .iter()
        .map(|(address, reason)| format!("{address}: {reason}"))
        .collect::<Vec<_>>()
        .join("; ")
}

impl ClientError {
    /// The node's error, when the node answered with one (SPEC 16.2).
    pub fn detail(&self) -> Option<&ErrorDetail> {
        match self {
            ClientError::Connection(inner) => match inner.as_ref() {
                ConnectionError::Remote(detail) | ConnectionError::StreamFailed(detail) => {
                    Some(detail)
                }
                _ => None,
            },
            _ => None,
        }
    }

    pub fn is_not_found(&self) -> bool {
        self.detail().map(|d| d.code) == Some(ErrorCode::NotFound)
    }

    /// Whether the connection failed, as opposed to the node refusing:
    /// the case in which another node may answer.
    fn is_connection_failure(&self) -> bool {
        matches!(self, ClientError::Connection(inner) if matches!(inner.as_ref(), ConnectionError::Wire(_)))
    }
}

/// What `Status` reports (19.1.3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Status {
    pub cluster_id: Uuid,
    pub cluster_name: Option<String>,
    pub document_version: u64,
    pub coordinator: NodeId,
    /// Every node asked, with its build; empty from a coordinator that
    /// predates builds in `Status`.
    pub nodes: Vec<NodeStatus>,
    pub transport: Transport,
    pub devices: Vec<DeviceStatus>,
}

/// Who the client is connected to, from the node's `Hello` (19.1.5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    pub address: SocketAddr,
    pub cluster_id: Uuid,
    pub cluster_name: Option<String>,
    pub node: Option<NodeId>,
    pub build: Option<String>,
    pub document_version: u64,
}

/// What `MoveShard` reports (19.1.3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MoveShardReport {
    /// The record at its new revision.
    pub record: MetadataRecord,
    /// The device the shard came from.
    pub source: DeviceId,
    /// Whether the source's copy was removed; if not, the scrub reports
    /// it as stale and repair removes it.
    pub source_cleaned: bool,
    /// Whether the shard was rebuilt from the others rather than copied.
    pub rebuilt: bool,
}

/// A scrub or drain in progress: the events as the node sends them, then
/// the end. Reading past the end is an error.
pub struct EventRun<E> {
    connection: Connection,
    id: u32,
    event: std::marker::PhantomData<E>,
}

impl<E: serde::de::DeserializeOwned> EventRun<E> {
    /// The next event, or the stream's end: `Ok(Ok(event))`, or
    /// `Ok(Err(end))` once, where `end.error` says whether the run failed.
    pub async fn next_event(&mut self) -> Result<Result<E, StreamEnd>, ClientError> {
        Ok(self.connection.next_event(self.id).await?)
    }
}

/// One page of a listing (15.2.1): keys in order, and whether more
/// follow after the last one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListPage {
    pub keys: Vec<KeyEntry>,
    pub truncated: bool,
}

impl ListPage {
    /// The `start_after` for the next page, when there is one.
    pub fn next_start_after(&self) -> Option<&str> {
        if self.truncated {
            self.keys.last().map(|k| k.key.as_str())
        } else {
            None
        }
    }
}

pub struct Client {
    options: ClientOptions,
    cluster: Uuid,
    connection: Option<(SocketAddr, Connection)>,
    /// Where to start in `options.nodes` at the next reconnection.
    next_node: usize,
}

impl Client {
    /// Connect to the first node in `options.nodes` that answers. Without
    /// a cluster id, the first node that answers is asked for it.
    pub async fn connect(options: ClientOptions) -> Result<Client, ClientError> {
        if options.nodes.is_empty() {
            return Err(ClientError::NoNodes);
        }
        let mut attempts = Vec::new();
        for (position, &address) in options.nodes.iter().enumerate() {
            let cluster = match options.cluster {
                Some(cluster) => cluster,
                None => match ask_cluster_id(&options.connector, address).await {
                    Ok(cluster) => cluster,
                    Err(e) => {
                        attempts.push((address, e.to_string()));
                        continue;
                    }
                },
            };
            match Connection::connect_with(
                &options.connector,
                address,
                Connection::client_hello(cluster),
            )
            .await
            {
                Ok(connection) => {
                    return Ok(Client {
                        next_node: (position + 1) % options.nodes.len(),
                        options,
                        cluster,
                        connection: Some((address, connection)),
                    })
                }
                Err(e) => attempts.push((address, e.to_string())),
            }
        }
        Err(ClientError::Unreachable(attempts))
    }

    pub fn cluster_id(&self) -> Uuid {
        self.cluster
    }

    /// The node the client is connected to, if it is.
    pub fn node_address(&self) -> Option<SocketAddr> {
        self.connection.as_ref().map(|(address, _)| *address)
    }

    /// The open connection, reconnecting to the next node if the last one
    /// failed.
    async fn connection(&mut self) -> Result<&mut Connection, ClientError> {
        if self.connection.is_none() {
            self.reconnect().await?;
        }
        Ok(&mut self.connection.as_mut().expect("connected").1)
    }

    /// Try every node once, starting after the one that failed.
    async fn reconnect(&mut self) -> Result<(), ClientError> {
        let count = self.options.nodes.len();
        let mut attempts = Vec::new();
        for offset in 0..count {
            let position = (self.next_node + offset) % count;
            let address = self.options.nodes[position];
            match Connection::connect_with(
                &self.options.connector,
                address,
                Connection::client_hello(self.cluster),
            )
            .await
            {
                Ok(connection) => {
                    self.connection = Some((address, connection));
                    self.next_node = (position + 1) % count;
                    return Ok(());
                }
                Err(e) => attempts.push((address, e.to_string())),
            }
        }
        Err(ClientError::Unreachable(attempts))
    }

    /// One request and its response. A request whose connection fails is
    /// sent once more over a fresh connection to another node when
    /// `retry` says it is safe to repeat; a refusal is never retried.
    async fn request(&mut self, request: Request, retry: bool) -> Result<Response, ClientError> {
        let mut attempts_left = if retry { 2 } else { 1 };
        loop {
            attempts_left -= 1;
            let result = self
                .connection()
                .await?
                .request(request.clone())
                .await
                .map_err(ClientError::from);
            match result {
                Ok(Response::Error(detail)) => return Err(ConnectionError::Remote(detail).into()),
                Ok(response) => return Ok(response),
                Err(e) if e.is_connection_failure() => {
                    self.connection = None;
                    if attempts_left == 0 {
                        return Err(e);
                    }
                }
                Err(e) => return Err(e),
            }
        }
    }

    fn unexpected(request: &'static str, got: Response) -> ClientError {
        ClientError::UnexpectedResponse {
            request,
            got: format!("{got:?}"),
        }
    }

    // ---------------------------------------------------------- objects

    /// Store `body` under `key`. Returns the version id.
    pub async fn put(
        &mut self,
        key: &str,
        body: &[u8],
        content_type: Option<String>,
    ) -> Result<VersionId, ClientError> {
        let mut cursor = body;
        self.put_from_reader(
            key,
            body.len() as u64,
            &mut cursor,
            content_type,
            BTreeMap::new(),
        )
        .await
    }

    /// Store `size` bytes read from `source` under `key`, holding one body
    /// chunk in memory at a time. Not retried: a failed upload is
    /// reported, and the caller decides.
    pub async fn put_from_reader<R: AsyncRead + Unpin>(
        &mut self,
        key: &str,
        size: u64,
        source: &mut R,
        content_type: Option<String>,
        user_metadata: BTreeMap<String, String>,
    ) -> Result<VersionId, ClientError> {
        let chunk = self.options.body_chunk;
        let result = self
            .connection()
            .await?
            .put_object_with_metadata(key, size, source, chunk, content_type, user_metadata)
            .await
            .map_err(ClientError::from);
        self.forget_connection_on_failure(&result);
        result
    }

    /// Fetch an object and its record.
    pub async fn get(&mut self, key: &str) -> Result<(MetadataRecord, Vec<u8>), ClientError> {
        let mut body = Vec::new();
        let record = self.get_to_writer(key, &mut body).await?;
        Ok((record, body))
    }

    /// Fetch an object, writing the body to `sink` as it arrives. Every
    /// block is checked against its checksum by the node before it is
    /// sent, and the whole object's checksum at the end; a failure part
    /// way leaves `sink` with what arrived so far, and says so.
    pub async fn get_to_writer<W: AsyncWrite + Unpin>(
        &mut self,
        key: &str,
        sink: &mut W,
    ) -> Result<MetadataRecord, ClientError> {
        let result = self
            .connection()
            .await?
            .get_object_to_writer(key, sink)
            .await
            .map_err(ClientError::from);
        self.forget_connection_on_failure(&result);
        result
    }

    /// The object's record without its body.
    pub async fn head(&mut self, key: &str) -> Result<MetadataRecord, ClientError> {
        match self
            .request(
                Request::HeadObject {
                    key: key.to_string(),
                },
                true,
            )
            .await?
        {
            Response::HeadObject { record } => Ok(record),
            other => Err(Self::unexpected("HeadObject", other)),
        }
    }

    /// Delete an object. Not retried over a fresh connection, since the
    /// first attempt may have succeeded before the connection failed.
    pub async fn delete(&mut self, key: &str) -> Result<(), ClientError> {
        match self
            .request(
                Request::DeleteObject {
                    key: key.to_string(),
                },
                false,
            )
            .await?
        {
            Response::DeleteObject => Ok(()),
            other => Err(Self::unexpected("DeleteObject", other)),
        }
    }

    /// One page of keys (15.2.1).
    pub async fn list(&mut self, query: ListQuery) -> Result<ListPage, ClientError> {
        match self.request(Request::ListKeys(query), true).await? {
            Response::ListKeys { keys, truncated } => Ok(ListPage { keys, truncated }),
            other => Err(Self::unexpected("ListKeys", other)),
        }
    }

    /// Every key under `prefix`, page after page. Holds them all in
    /// memory; for large listings page with [`Client::list`].
    pub async fn list_all(&mut self, prefix: Option<&str>) -> Result<Vec<KeyEntry>, ClientError> {
        let mut keys = Vec::new();
        let mut start_after: Option<String> = None;
        loop {
            let page = self
                .list(ListQuery {
                    prefix: prefix.map(str::to_string),
                    start_after: start_after.clone(),
                    limit: None,
                })
                .await?;
            start_after = page.next_start_after().map(str::to_string);
            keys.extend(page.keys);
            if start_after.is_none() {
                return Ok(keys);
            }
        }
    }

    /// What one device holds (18.2.3), counted from its records without
    /// reading data. Zero versions means the device is empty.
    pub async fn device_contents(
        &mut self,
        device: DeviceId,
    ) -> Result<DeviceContents, ClientError> {
        match self
            .request(Request::DeviceContents { device }, true)
            .await?
        {
            Response::DeviceContents(contents) => Ok(contents),
            other => Err(Self::unexpected("DeviceContents", other)),
        }
    }

    /// Move one shard of an object to another device (18.8.2): `target`
    /// names the device, or `None` chooses as a write would (10.4).
    pub async fn move_shard(
        &mut self,
        key: &str,
        shard_index: u8,
        target: Option<DeviceId>,
    ) -> Result<MoveShardReport, ClientError> {
        match self
            .request(
                Request::MoveShard {
                    key: key.to_string(),
                    shard_index,
                    target,
                },
                false,
            )
            .await?
        {
            Response::MoveShard {
                record,
                source,
                source_cleaned,
                rebuilt,
            } => Ok(MoveShardReport {
                record,
                source,
                source_cleaned,
                rebuilt,
            }),
            other => Err(Self::unexpected("MoveShard", other)),
        }
    }

    /// Start a cluster-wide scrub (20.1.2). The run owns the connection
    /// until its last event; the client reconnects for whatever follows.
    pub async fn scrub(
        &mut self,
        max_bytes_per_second: Option<u64>,
        repair: bool,
    ) -> Result<EventRun<ScrubEvent>, ClientError> {
        self.connection().await?;
        let (_, mut connection) = self.connection.take().expect("connected");
        let id = connection.start_scrub(max_bytes_per_second, repair).await?;
        Ok(EventRun {
            connection,
            id,
            event: std::marker::PhantomData,
        })
    }

    /// Start a drain of one draining device (18.2.1); events as for
    /// [`Client::scrub`].
    pub async fn drain(
        &mut self,
        device: DeviceId,
        partial: bool,
    ) -> Result<EventRun<DrainEvent>, ClientError> {
        self.connection().await?;
        let (_, mut connection) = self.connection.take().expect("connected");
        let id = connection.start_drain(device, partial).await?;
        Ok(EventRun {
            connection,
            id,
            event: std::marker::PhantomData,
        })
    }

    /// Rebuild what is damaged or missing of one object (18.3, 18.4).
    pub async fn repair(&mut self, key: &str) -> Result<RepairReport, ClientError> {
        match self
            .request(
                Request::RepairObject {
                    key: key.to_string(),
                },
                false,
            )
            .await?
        {
            Response::RepairObject(report) => Ok(report),
            other => Err(Self::unexpected("RepairObject", other)),
        }
    }

    // ---------------------------------------------------------- cluster

    pub async fn status(&mut self) -> Result<Status, ClientError> {
        match self.request(Request::Status, true).await? {
            Response::Status {
                cluster_id,
                cluster_name,
                document_version,
                coordinator,
                nodes,
                transport,
                devices,
            } => Ok(Status {
                cluster_id,
                cluster_name,
                document_version,
                coordinator,
                nodes,
                transport,
                devices,
            }),
            other => Err(Self::unexpected("Status", other)),
        }
    }

    /// The cluster document as the connected node holds it.
    pub async fn cluster_document(&mut self) -> Result<ClusterDocument, ClientError> {
        match self.request(Request::GetClusterConfig, true).await? {
            Response::GetClusterConfig { document } => Ok(document),
            other => Err(Self::unexpected("GetClusterConfig", other)),
        }
    }

    /// Who the client is connected to, reconnecting first if it is not.
    pub async fn identity(&mut self) -> Result<Identity, ClientError> {
        self.connection().await?;
        let (address, connection) = self.connection.as_ref().expect("connected");
        let hello = connection.peer_hello();
        Ok(Identity {
            address: *address,
            cluster_id: hello.cluster_id,
            cluster_name: hello.cluster_name.clone(),
            node: hello.node_id,
            build: hello.build.clone(),
            document_version: hello.document_version,
        })
    }

    fn forget_connection_on_failure<T>(&mut self, result: &Result<T, ClientError>) {
        if matches!(result, Err(e) if e.is_connection_failure()) {
            self.connection = None;
        }
    }
}

/// Ask `address` who it is without naming a cluster (19.1.5.1): the
/// node answers with its `Hello` and closes.
pub async fn ask(connector: &Connector, address: SocketAddr) -> Result<Identity, ClientError> {
    let connection =
        Connection::connect_with(connector, address, Connection::client_hello(Uuid::nil())).await?;
    let hello = connection.peer_hello();
    Ok(Identity {
        address,
        cluster_id: hello.cluster_id,
        cluster_name: hello.cluster_name.clone(),
        node: hello.node_id,
        build: hello.build.clone(),
        document_version: hello.document_version,
    })
}

/// Ask `address` which cluster it serves (19.1.5.1).
pub async fn ask_cluster_id(
    connector: &Connector,
    address: SocketAddr,
) -> Result<Uuid, ClientError> {
    Ok(ask(connector, address).await?.cluster_id)
}
