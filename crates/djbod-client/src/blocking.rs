//! The same client without `async`, for programs and language bindings
//! that call it from ordinary threads. Each method runs the asynchronous
//! one to completion on a runtime the client owns.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::SocketAddr;
use std::pin::Pin;
use std::task::{Context, Poll};

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::runtime::Runtime;
use uuid::Uuid;

use djbod_core::cluster::ClusterDocument;
use djbod_core::record::{DeviceId, MetadataRecord};
use djbod_core::version::VersionId;
use djbod_proto::message::{DeviceContents, KeyEntry, ListQuery, RepairReport};

pub use crate::client::{ClientError, ClientOptions, Identity, ListPage, Status};

pub struct Client {
    runtime: Runtime,
    inner: crate::client::Client,
}

impl Client {
    pub fn connect(options: ClientOptions) -> Result<Client, ClientError> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("a tokio runtime");
        let inner = runtime.block_on(crate::client::Client::connect(options))?;
        Ok(Client { runtime, inner })
    }

    pub fn cluster_id(&self) -> Uuid {
        self.inner.cluster_id()
    }

    pub fn node_address(&self) -> Option<SocketAddr> {
        self.inner.node_address()
    }

    pub fn put(
        &mut self,
        key: &str,
        body: &[u8],
        content_type: Option<String>,
    ) -> Result<VersionId, ClientError> {
        self.runtime
            .block_on(self.inner.put(key, body, content_type))
    }

    /// Store `size` bytes read from `source`. The reader is driven from
    /// the runtime's thread a chunk at a time.
    pub fn put_from_reader<R: Read + Unpin>(
        &mut self,
        key: &str,
        size: u64,
        source: R,
        content_type: Option<String>,
        user_metadata: BTreeMap<String, String>,
    ) -> Result<VersionId, ClientError> {
        let mut source = BlockingReader(source);
        self.runtime.block_on(self.inner.put_from_reader(
            key,
            size,
            &mut source,
            content_type,
            user_metadata,
        ))
    }

    pub fn get(&mut self, key: &str) -> Result<(MetadataRecord, Vec<u8>), ClientError> {
        self.runtime.block_on(self.inner.get(key))
    }

    /// Fetch an object, writing the body to `sink` as it arrives.
    pub fn get_to_writer<W: Write + Unpin>(
        &mut self,
        key: &str,
        sink: W,
    ) -> Result<MetadataRecord, ClientError> {
        let mut sink = BlockingWriter(sink);
        self.runtime
            .block_on(self.inner.get_to_writer(key, &mut sink))
    }

    pub fn head(&mut self, key: &str) -> Result<MetadataRecord, ClientError> {
        self.runtime.block_on(self.inner.head(key))
    }

    pub fn delete(&mut self, key: &str) -> Result<(), ClientError> {
        self.runtime.block_on(self.inner.delete(key))
    }

    pub fn list(&mut self, query: ListQuery) -> Result<ListPage, ClientError> {
        self.runtime.block_on(self.inner.list(query))
    }

    pub fn list_all(&mut self, prefix: Option<&str>) -> Result<Vec<KeyEntry>, ClientError> {
        self.runtime.block_on(self.inner.list_all(prefix))
    }

    pub fn device_contents(&mut self, device: DeviceId) -> Result<DeviceContents, ClientError> {
        self.runtime.block_on(self.inner.device_contents(device))
    }

    pub fn repair(&mut self, key: &str) -> Result<RepairReport, ClientError> {
        self.runtime.block_on(self.inner.repair(key))
    }

    pub fn status(&mut self) -> Result<Status, ClientError> {
        self.runtime.block_on(self.inner.status())
    }

    pub fn cluster_document(&mut self) -> Result<ClusterDocument, ClientError> {
        self.runtime.block_on(self.inner.cluster_document())
    }

    pub fn identity(&mut self) -> Result<Identity, ClientError> {
        self.runtime.block_on(self.inner.identity())
    }
}

/// A synchronous reader offered as an asynchronous one. Each read blocks
/// the runtime's thread, which is this client's own and has nothing else
/// to do while an upload is in progress.
struct BlockingReader<R>(R);

impl<R: Read + Unpin> AsyncRead for BlockingReader<R> {
    fn poll_read(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let unfilled = buf.initialize_unfilled();
        let read = self.get_mut().0.read(unfilled)?;
        buf.advance(read);
        Poll::Ready(Ok(()))
    }
}

/// The counterpart for downloads.
struct BlockingWriter<W>(W);

impl<W: Write + Unpin> AsyncWrite for BlockingWriter<W> {
    fn poll_write(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Poll::Ready(self.get_mut().0.write(buf))
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Poll::Ready(self.get_mut().0.flush())
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        self.poll_flush(cx)
    }
}
