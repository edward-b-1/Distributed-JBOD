use crate::{
    config::{WorkerAddress, WorkerConfig},
    model::{digest, Reply, Request, FORMAT},
    worker::Worker,
    Stop,
};
use anyhow::{bail, ensure, Context, Result};
use djbod_client::transport::{
    check_key_permissions, read_authorities, read_certificates, read_key, TlsPaths,
};
use rustls::{pki_types::ServerName, server::WebPkiClientVerifier, ClientConfig, ServerConfig};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use std::{
    path::Path,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    task::JoinSet,
};
use tokio_rustls::{TlsAcceptor, TlsConnector};

pub const MAX_FRAME: usize = 8 * 1024 * 1024;
const TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Serialize, Deserialize)]
struct Envelope {
    format: u32,
    request: Request,
}

pub fn certificate_fingerprint(path: &Path) -> Result<String> {
    Ok(digest(
        read_certificates(path)?
            .first()
            .context("certificate file is empty")?
            .as_ref(),
    ))
}

async fn read_frame<T: DeserializeOwned>(stream: &mut (impl AsyncRead + Unpin)) -> Result<T> {
    let length = stream.read_u32().await? as usize;
    ensure!(
        length > 0 && length <= MAX_FRAME,
        "invalid bitrotter frame length"
    );
    let mut bytes = vec![0; length];
    stream.read_exact(&mut bytes).await?;
    Ok(serde_json::from_slice(&bytes)?)
}

async fn write_frame<T: Serialize>(
    stream: &mut (impl AsyncWrite + Unpin),
    value: &T,
) -> Result<()> {
    let bytes = serde_json::to_vec(value)?;
    ensure!(bytes.len() <= MAX_FRAME, "bitrotter request exceeds 8 MiB");
    stream.write_u32(bytes.len() as u32).await?;
    stream.write_all(&bytes).await?;
    stream.flush().await?;
    Ok(())
}

#[derive(Clone)]
pub struct RpcClient {
    tls: Arc<ClientConfig>,
}

impl RpcClient {
    pub fn new(paths: &TlsPaths) -> Result<Self> {
        check_key_permissions(&paths.key)?;
        let tls = ClientConfig::builder()
            .with_root_certificates(read_authorities(&paths.ca)?)
            .with_client_auth_cert(read_certificates(&paths.cert)?, read_key(&paths.key)?)?;
        Ok(Self { tls: Arc::new(tls) })
    }
    pub async fn request(&self, worker: &WorkerAddress, request: Request) -> Result<Reply> {
        let envelope = Envelope {
            format: FORMAT,
            request,
        };
        ensure!(
            serde_json::to_vec(&envelope)?.len() <= MAX_FRAME,
            "plan/request exceeds 8 MiB frame limit"
        );
        let request = async {
            let address = if worker.address.contains(':') {
                worker.address.clone()
            } else {
                format!("{}:{}", worker.address, crate::WORKER_PORT)
            };
            let host = worker.server_name.clone().unwrap_or_else(|| {
                if let Ok(address) = address.parse::<std::net::SocketAddr>() {
                    address.ip().to_string()
                } else {
                    address
                        .rsplit_once(':')
                        .map_or(address.clone(), |(host, _)| host.to_owned())
                }
            });
            let name = ServerName::try_from(host).context("invalid worker TLS server name")?;
            let tcp = TcpStream::connect(&address)
                .await
                .with_context(|| format!("connecting to worker {address}"))?;
            tcp.set_nodelay(true)?;
            let mut tls = TlsConnector::from(self.tls.clone())
                .connect(name, tcp)
                .await?;
            write_frame(&mut tls, &envelope).await?;
            match read_frame::<Reply>(&mut tls).await? {
                Reply::Error(message) => bail!("worker {}: {message}", worker.node),
                reply => Ok(reply),
            }
        };
        tokio::time::timeout(TIMEOUT, request)
            .await
            .context("worker request timed out; outcome may be uncertain")?
    }
}

pub async fn serve(config: WorkerConfig, listener: TcpListener, stop: Stop) -> Result<()> {
    check_key_permissions(&config.tls.key)?;
    let verifier =
        WebPkiClientVerifier::builder(Arc::new(read_authorities(&config.tls.ca)?)).build()?;
    let tls = ServerConfig::builder()
        .with_client_cert_verifier(verifier)
        .with_single_cert(
            read_certificates(&config.tls.cert)?,
            read_key(&config.tls.key)?,
        )?;
    let acceptor = TlsAcceptor::from(Arc::new(tls));
    let worker = Arc::new(Mutex::new(Worker::open(config)?));
    let mut tasks = JoinSet::new();
    loop {
        if tasks.len() >= 64 {
            let _ = tasks.join_next().await;
        }
        tokio::select! {
            _ = stop.cancelled() => break,
            _ = tasks.join_next(), if !tasks.is_empty() => {},
            incoming = listener.accept() => {
                let (tcp, _) = incoming?;
                let acceptor = acceptor.clone();
                let worker = worker.clone();
                tasks.spawn(async move {
                    let operation = async {
                        tcp.set_nodelay(true)?;
                        let mut tls = acceptor.accept(tcp).await?;
                        let cert = tls.get_ref().1.peer_certificates()
                            .and_then(|certs| certs.first()).context("client certificate is required")?;
                        let controller = digest(cert.as_ref());
                        let envelope: Envelope = read_frame(&mut tls).await?;
                        ensure!(envelope.format == FORMAT, "unsupported worker protocol");
                        let result = tokio::task::spawn_blocking(move || -> Result<Reply> {
                            worker.lock().map_err(|_| anyhow::anyhow!("worker lock poisoned"))?
                                .handle(&controller, envelope.request)
                        }).await?;
                        let reply = result.unwrap_or_else(|error| Reply::Error(format!("{error:#}")));
                        write_frame(&mut tls, &reply).await?;
                        Ok::<_, anyhow::Error>(())
                    };
                    // If the connection times out after an intent was written,
                    // the blocking mutation still completes and records its result.
                    let _ = tokio::time::timeout(TIMEOUT, operation).await;
                });
            }
        }
    }
    while tasks.join_next().await.is_some() {}
    Ok(())
}
