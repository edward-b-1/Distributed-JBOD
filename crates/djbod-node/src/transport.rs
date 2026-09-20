//! The node's side of the byte stream beneath the frames (SPEC 19.1.6):
//! its own TLS material, which both accepts and connects, and accepting
//! an incoming connection as plain or TLS. The client half, `Connector`,
//! `Stream`, the path types and the PEM readers, is `djbod_client::transport`
//! and re-exported here.

use std::io;
use std::sync::Arc;

use rustls::server::WebPkiClientVerifier;
use rustls::{ClientConfig, ServerConfig};
use tokio::net::TcpStream;
use tokio_rustls::{TlsAcceptor, TlsStream};

use djbod_core::cluster::Transport;

pub use djbod_client::transport::{
    check_key_permissions, read_authorities, read_certificates, read_key, ClientTlsPaths,
    Connector, Stream, TlsError, TlsPaths,
};

/// Loaded material: how to accept and how to connect (SPEC 19.1.6.3).
pub struct TlsMaterial {
    client: Arc<ClientConfig>,
    /// Accepts only peers presenting a certificate the authority issued.
    server_authenticated: Arc<ServerConfig>,
    /// Accepts a peer with or without a certificate; one presented must
    /// verify (the `tls-optional` mode, 19.1.6.4).
    server_any: Arc<ServerConfig>,
}

impl TlsMaterial {
    pub fn load(paths: &TlsPaths) -> Result<TlsMaterial, TlsError> {
        check_key_permissions(&paths.key)?;
        let certs = read_certificates(&paths.cert)?;
        if certs.is_empty() {
            return Err(TlsError::NoCertificate {
                path: paths.cert.clone(),
            });
        }
        let key = read_key(&paths.key)?;
        let roots = Arc::new(read_authorities(&paths.ca)?);
        let client = ClientConfig::builder()
            .with_root_certificates(roots.clone())
            .with_client_auth_cert(certs.clone(), key.clone_key())
            .map_err(|e| TlsError::Rustls(e.to_string()))?;
        let authenticated = WebPkiClientVerifier::builder(roots.clone())
            .build()
            .map_err(|e| TlsError::Rustls(e.to_string()))?;
        let any = WebPkiClientVerifier::builder(roots)
            .allow_unauthenticated()
            .build()
            .map_err(|e| TlsError::Rustls(e.to_string()))?;
        let server_authenticated = ServerConfig::builder()
            .with_client_cert_verifier(authenticated)
            .with_single_cert(certs.clone(), key.clone_key())
            .map_err(|e| TlsError::Rustls(e.to_string()))?;
        let server_any = ServerConfig::builder()
            .with_client_cert_verifier(any)
            .with_single_cert(certs, key)
            .map_err(|e| TlsError::Rustls(e.to_string()))?;
        Ok(TlsMaterial {
            client: Arc::new(client),
            server_authenticated: Arc::new(server_authenticated),
            server_any: Arc::new(server_any),
        })
    }

    /// How this side connects out: TLS with its own certificate.
    pub fn connector(&self) -> Connector {
        Connector::with_tls(self.client.clone())
    }
}

/// The first byte of a TLS connection is the handshake record type. A
/// frame header begins with the message type as a little-endian u16, and
/// no message type is 0x16 (19.1.2), so one byte tells the two apart.
const TLS_HANDSHAKE_RECORD: u8 = 0x16;

/// What the listener decided about an incoming connection.
pub enum Accepted {
    Stream(Stream),
    /// A plain connection arrived while the cluster's transport is `tls`.
    /// The stream is handed back so the refusal can be written on it.
    PlainRefused(TcpStream),
}

/// Look at the first byte and complete the appropriate handshake
/// (SPEC 19.1.6.4). TLS is accepted whenever material is loaded; plain
/// is accepted unless the transport is `tls`.
pub async fn accept(
    tcp: TcpStream,
    material: Option<&TlsMaterial>,
    transport: Transport,
) -> io::Result<Accepted> {
    tcp.set_nodelay(true)?;
    let mut first = [0u8; 1];
    let peeked = tcp.peek(&mut first).await?;
    let looks_like_tls = peeked == 1 && first[0] == TLS_HANDSHAKE_RECORD;
    match (looks_like_tls, material) {
        (true, Some(material)) => {
            let config = match transport {
                Transport::Tls => material.server_authenticated.clone(),
                Transport::Plain | Transport::TlsOptional => material.server_any.clone(),
            };
            let tls = TlsAcceptor::from(config).accept(tcp).await?;
            Ok(Accepted::Stream(Stream::Tls(Box::new(TlsStream::Server(
                tls,
            )))))
        }
        (true, None) => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "peer began a TLS handshake but this node has no TLS material",
        )),
        (false, _) if transport == Transport::Tls => Ok(Accepted::PlainRefused(tcp)),
        (false, _) => Ok(Accepted::Stream(Stream::Plain(tcp))),
    }
}
