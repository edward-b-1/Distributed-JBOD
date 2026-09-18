//! Accepting connections and dispatching requests (SPEC 4.4, 19.1.5).

use std::sync::Arc;

use tokio::io::BufReader;
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{TcpListener, TcpStream};
use tracing::Instrument;

use djbod_proto::handshake::{Hello, PeerKind, PROTOCOL_VERSION};
use djbod_proto::message::{ErrorCode, ErrorDetail, Message, Response};

use crate::coordinator;
use crate::local_ops;
use crate::node::Node;
use crate::wire::{read_message, write_message, WireError};

/// Why a connection was closed. Every connection ends with one of these;
/// only `PeerClosed` is silent.
#[derive(Debug)]
pub enum ConnectionEnd {
    PeerClosed,
    Wire(WireError),
    ProtocolViolation(String),
    HelloRefused(String),
}

impl From<WireError> for ConnectionEnd {
    fn from(e: WireError) -> ConnectionEnd {
        match e {
            WireError::Closed => ConnectionEnd::PeerClosed,
            other => ConnectionEnd::Wire(other),
        }
    }
}

pub type Reader = BufReader<OwnedReadHalf>;
pub type Writer = OwnedWriteHalf;

/// Accept connections forever, one task each.
pub async fn serve(node: Arc<Node>, listener: TcpListener) {
    loop {
        match listener.accept().await {
            Ok((stream, peer)) => {
                let node = node.clone();
                // The connection span (SPEC 20.4.2): every event on this
                // connection carries the peer address; kind and node id
                // are recorded once the Hello arrives.
                let span = tracing::info_span!(
                    "connection",
                    %peer,
                    kind = tracing::field::Empty,
                    node = tracing::field::Empty
                );
                tokio::spawn(
                    async move {
                        let end = handle_connection(node, stream).await;
                        match end {
                            ConnectionEnd::PeerClosed => tracing::debug!("connection closed"),
                            other => tracing::info!(?other, "connection ended"),
                        }
                    }
                    .instrument(span),
                );
            }
            Err(e) => {
                tracing::error!(error = %e, "accept failed");
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
        }
    }
}

pub fn our_hello(node: &Node) -> Hello {
    Hello {
        protocol_version: PROTOCOL_VERSION,
        kind: PeerKind::Node,
        node_id: Some(node.id()),
        cluster_id: node.cluster_id(),
        document_version: node.document_version(),
    }
}

async fn handle_connection(node: Arc<Node>, stream: TcpStream) -> ConnectionEnd {
    if let Err(e) = stream.set_nodelay(true) {
        return ConnectionEnd::Wire(WireError::Io(e));
    }
    let (read_half, write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);
    let mut writer = write_half;

    // The peer speaks first (19.1.5).
    let hello = match read_message(&mut reader).await {
        Ok(Message::Hello(hello)) => hello,
        Ok(_) => {
            return ConnectionEnd::ProtocolViolation("first message was not Hello".to_string())
        }
        Err(e) => return e.into(),
    };
    if let Err(refusal) = hello.check_against(node.cluster_id(), node.document_version()) {
        let code = match refusal {
            djbod_proto::handshake::HelloError::DocumentVersion { .. } => {
                ErrorCode::DocumentVersionMismatch
            }
            _ => ErrorCode::ProtocolViolation,
        };
        let detail = ErrorDetail {
            node: Some(node.id()),
            ..ErrorDetail::new(code, refusal.to_string())
        };
        let _ = write_message(
            &mut writer,
            &Message::Response {
                id: 0,
                response: Response::Error(detail),
            },
        )
        .await;
        return ConnectionEnd::HelloRefused(refusal.to_string());
    }
    if let Err(e) = write_message(&mut writer, &Message::Hello(our_hello(&node))).await {
        return e.into();
    }
    let span = tracing::Span::current();
    span.record("kind", tracing::field::debug(hello.kind));
    if let Some(node_id) = hello.node_id {
        span.record("node", tracing::field::display(node_id.0));
    }
    tracing::debug!("connection accepted");

    loop {
        let (id, request) = match read_message(&mut reader).await {
            Ok(Message::Request { id, request }) => (id, request),
            Ok(other) => {
                return ConnectionEnd::ProtocolViolation(format!(
                    "expected a Request between operations, got {other:?}"
                ))
            }
            Err(e) => return e.into(),
        };
        // The request span (SPEC 20.4.2): id, operation, and the key for
        // object operations.
        let span = tracing::info_span!(
            "request",
            id,
            op = operation_name(&request),
            key = request_key(&request)
        );
        let result = async {
            if coordinator::is_client_operation(&request) {
                coordinator::handle(
                    &node,
                    node.versions(),
                    id,
                    request,
                    &mut reader,
                    &mut writer,
                )
                .await
            } else {
                local_ops::handle(&node, id, request, &mut reader, &mut writer).await
            }
        }
        .instrument(span)
        .await;
        if let Err(end) = result {
            return end;
        }
    }
}

fn operation_name(request: &djbod_proto::message::Request) -> &'static str {
    use djbod_proto::message::Request::*;
    match request {
        Status => "Status",
        PutObject { .. } => "PutObject",
        GetObject { .. } => "GetObject",
        HeadObject { .. } => "HeadObject",
        DeleteObject { .. } => "DeleteObject",
        ListKeys(_) => "ListKeys",
        LocalStatus => "LocalStatus",
        LocalLookup { .. } => "LocalLookup",
        LocalList(_) => "LocalList",
        PutShard { .. } => "PutShard",
        GetShard { .. } => "GetShard",
        PutMeta { .. } => "PutMeta",
        GetMeta { .. } => "GetMeta",
        DeleteVersion { .. } => "DeleteVersion",
        AbortShard { .. } => "AbortShard",
        GetClusterConfig => "GetClusterConfig",
        ApplyClusterConfig { .. } => "ApplyClusterConfig",
    }
}

fn request_key(request: &djbod_proto::message::Request) -> Option<&str> {
    use djbod_proto::message::Request::*;
    match request {
        PutObject { key, .. } | GetObject { key } | HeadObject { key } | DeleteObject { key } => {
            Some(key.as_str())
        }
        _ => None,
    }
}
