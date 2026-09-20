//! The byte stream beneath the frames: plain TCP or TLS (SPEC 19.1.6),
//! as a client opens it. Accepting connections, and the node's own
//! material, live in the node crate on top of the pieces here.
//!
//! TLS material is PEM files named by path (19.1.6.2): a certificate
//! chain, its private key, and the certificate authority bundle. Nothing
//! here generates or signs anything.

use std::fs::File;
use std::io::{self, BufReader as StdBufReader};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName};
use rustls::{ClientConfig, RootCertStore};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpStream;
use tokio_rustls::{TlsConnector, TlsStream};

/// Where a node's or client's TLS material lives (SPEC 19.1.6.2).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TlsPaths {
    /// PEM certificate, or chain, presented to peers.
    pub cert: PathBuf,
    /// PEM private key; must be readable only by its owner.
    pub key: PathBuf,
    /// PEM certificate authority, or a bundle of several.
    pub ca: PathBuf,
}

#[derive(Debug, Error)]
pub enum TlsError {
    #[error("cannot read {path}: {source}")]
    Io { path: PathBuf, source: io::Error },
    #[error("{path} holds no PEM certificate")]
    NoCertificate { path: PathBuf },
    #[error("{path} holds no PEM private key")]
    NoKey { path: PathBuf },
    #[error(
        "{path} is readable by others (mode {mode:o}); a private key must be readable only by its owner (SPEC 19.1.6.2)"
    )]
    KeyReadable { path: PathBuf, mode: u32 },
    #[error("{path} holds no usable certificate authority")]
    NoAuthority { path: PathBuf },
    #[error("TLS configuration rejected: {0}")]
    Rustls(String),
    #[error(
        "a client certificate and key must be given together, and with the certificate authority (--tls-ca, --tls-cert, --tls-key)"
    )]
    IncompleteClientIdentity,
}

/// What a client needs (SPEC 19.1.6.2): the authority to verify servers
/// against, and optionally its own certificate and key. Without them the
/// client is encrypted but anonymous, which a `tls-optional` cluster
/// accepts and a `tls` cluster refuses (19.1.6.4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientTlsPaths {
    pub ca: PathBuf,
    pub identity: Option<(PathBuf, PathBuf)>,
}

impl Connector {
    /// The connector a client program builds from its three optional
    /// settings (SPEC 19.1.6.2), the same way for `djbod` and `djbod-ui`:
    /// plain when no authority is given; TLS presenting no certificate
    /// when only the authority is; TLS with the client's identity when
    /// all three are. A certificate without its key, or either without
    /// the authority, is refused.
    pub fn from_client_options(
        ca: Option<&Path>,
        cert: Option<&Path>,
        key: Option<&Path>,
    ) -> Result<Connector, TlsError> {
        let identity = match (cert, key) {
            (None, None) => None,
            (Some(cert), Some(key)) => Some((cert.to_path_buf(), key.to_path_buf())),
            _ => return Err(TlsError::IncompleteClientIdentity),
        };
        match ca {
            None if identity.is_none() => Ok(Connector::plain()),
            None => Err(TlsError::IncompleteClientIdentity),
            Some(ca) => Connector::from_client_paths(&ClientTlsPaths {
                ca: ca.to_path_buf(),
                identity,
            }),
        }
    }

    /// A client connector from paths. With an identity the client
    /// presents its certificate; without one it presents none.
    pub fn from_client_paths(paths: &ClientTlsPaths) -> Result<Connector, TlsError> {
        let roots = Arc::new(read_authorities(&paths.ca)?);
        let builder = ClientConfig::builder().with_root_certificates(roots);
        let client = match &paths.identity {
            Some((cert, key)) => {
                check_key_permissions(key)?;
                let certs = read_certificates(cert)?;
                if certs.is_empty() {
                    return Err(TlsError::NoCertificate { path: cert.clone() });
                }
                builder
                    .with_client_auth_cert(certs, read_key(key)?)
                    .map_err(|e| TlsError::Rustls(e.to_string()))?
            }
            None => builder.with_no_client_auth(),
        };
        Ok(Connector::with_tls(Arc::new(client)))
    }
}

/// The certificate authority bundle at `path` as a root store; an empty
/// bundle is an error.
pub fn read_authorities(path: &Path) -> Result<RootCertStore, TlsError> {
    let mut roots = RootCertStore::empty();
    for authority in read_certificates(path)? {
        roots
            .add(authority)
            .map_err(|e| TlsError::Rustls(e.to_string()))?;
    }
    if roots.is_empty() {
        return Err(TlsError::NoAuthority {
            path: path.to_path_buf(),
        });
    }
    Ok(roots)
}

fn io_error(path: &Path, source: io::Error) -> TlsError {
    TlsError::Io {
        path: path.to_path_buf(),
        source,
    }
}

pub fn check_key_permissions(path: &Path) -> Result<(), TlsError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(path)
            .map_err(|e| io_error(path, e))?
            .permissions()
            .mode()
            & 0o777;
        if mode & 0o077 != 0 {
            return Err(TlsError::KeyReadable {
                path: path.to_path_buf(),
                mode,
            });
        }
    }
    Ok(())
}

pub fn read_certificates(path: &Path) -> Result<Vec<CertificateDer<'static>>, TlsError> {
    let file = File::open(path).map_err(|e| io_error(path, e))?;
    rustls_pemfile::certs(&mut StdBufReader::new(file))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| io_error(path, e))
}

pub fn read_key(path: &Path) -> Result<PrivateKeyDer<'static>, TlsError> {
    let file = File::open(path).map_err(|e| io_error(path, e))?;
    rustls_pemfile::private_key(&mut StdBufReader::new(file))
        .map_err(|e| io_error(path, e))?
        .ok_or_else(|| TlsError::NoKey {
            path: path.to_path_buf(),
        })
}

/// How to open an outgoing connection: plain, or TLS with a client
/// configuration.
#[derive(Clone, Default)]
pub struct Connector {
    tls: Option<Arc<ClientConfig>>,
}

impl Connector {
    pub fn plain() -> Connector {
        Connector { tls: None }
    }

    /// TLS with a ready client configuration; the node builds one from
    /// its own material.
    pub fn with_tls(config: Arc<ClientConfig>) -> Connector {
        Connector { tls: Some(config) }
    }

    pub fn is_tls(&self) -> bool {
        self.tls.is_some()
    }

    /// Connect to `addr`. Under TLS the server's certificate is verified
    /// against the authority and against the address dialled, which the
    /// certificate must carry as an IP subject alternative name
    /// (SPEC 19.1.6.1); addresses in the cluster document are IP:port.
    pub async fn connect(&self, addr: SocketAddr) -> io::Result<Stream> {
        let tcp = TcpStream::connect(addr).await?;
        tcp.set_nodelay(true)?;
        match &self.tls {
            None => Ok(Stream::Plain(tcp)),
            Some(config) => {
                let name = ServerName::IpAddress(rustls::pki_types::IpAddr::from(addr.ip()));
                let tls = TlsConnector::from(config.clone())
                    .connect(name, tcp)
                    .await?;
                Ok(Stream::Tls(Box::new(TlsStream::Client(tls))))
            }
        }
    }
}

/// An accepted or opened connection's byte stream.
pub enum Stream {
    Plain(TcpStream),
    Tls(Box<TlsStream<TcpStream>>),
}

impl Stream {
    pub fn is_tls(&self) -> bool {
        matches!(self, Stream::Tls(_))
    }
}

impl AsyncRead for Stream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Stream::Plain(s) => Pin::new(s).poll_read(cx, buf),
            Stream::Tls(s) => Pin::new(s.as_mut()).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for Stream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        match self.get_mut() {
            Stream::Plain(s) => Pin::new(s).poll_write(cx, buf),
            Stream::Tls(s) => Pin::new(s.as_mut()).poll_write(cx, buf),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Stream::Plain(s) => Pin::new(s).poll_flush(cx),
            Stream::Tls(s) => Pin::new(s.as_mut()).poll_flush(cx),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Stream::Plain(s) => Pin::new(s).poll_shutdown(cx),
            Stream::Tls(s) => Pin::new(s.as_mut()).poll_shutdown(cx),
        }
    }
}
