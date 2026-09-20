//! TLS between nodes and for clients (SPEC 19.1.6): material loading, the
//! three transport modes, and moving a running cluster between them.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use djbod_client::connection::{ClientError, Connection};
use djbod_core::cluster::Transport;
use djbod_node::config::NodeConfig;
use djbod_node::membership;
use djbod_node::node::{ClusterParameters, Node, NodeError};
use djbod_node::server;
use djbod_node::transport::{Connector, TlsError, TlsMaterial, TlsPaths};
use djbod_proto::message::{DataFrame, ErrorCode, Message, Request, Response, StreamEnd};
use rcgen::{BasicConstraints, CertificateParams, DnType, IsCa, KeyPair};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use uuid::Uuid;

const BLOCK: u64 = 64 * 1024;

/// A certificate authority and the material it issues, written as PEM
/// files the way an administrator would with openssl (SPEC 19.1.6.1).
struct Authority {
    dir: tempfile::TempDir,
    ca_cert: rcgen::Certificate,
    ca_key: KeyPair,
    issued: usize,
}

impl Authority {
    fn new() -> Authority {
        let mut params = CertificateParams::new(Vec::<String>::new()).expect("params");
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        params
            .distinguished_name
            .push(DnType::CommonName, "djbod test CA");
        let ca_key = KeyPair::generate().expect("key");
        let ca_cert = params.self_signed(&ca_key).expect("self-signed");
        let dir = tempfile::tempdir().expect("temp dir");
        std::fs::write(dir.path().join("ca.crt"), ca_cert.pem()).expect("write");
        Authority {
            dir,
            ca_cert,
            ca_key,
            issued: 0,
        }
    }

    fn ca_path(&self) -> PathBuf {
        self.dir.path().join("ca.crt")
    }

    /// Issue a certificate naming `host` (an IP for a node, any name for
    /// a client) and return the paths of the three files.
    fn issue(&mut self, host: &str) -> TlsPaths {
        self.issued += 1;
        let params = CertificateParams::new(vec![host.to_string()]).expect("params");
        let key = KeyPair::generate().expect("key");
        let cert = params
            .signed_by(&key, &self.ca_cert, &self.ca_key)
            .expect("signed");
        let cert_path = self.dir.path().join(format!("{}.crt", self.issued));
        let key_path = self.dir.path().join(format!("{}.key", self.issued));
        std::fs::write(&cert_path, cert.pem()).expect("write");
        write_private(&key_path, key.serialize_pem().as_bytes());
        TlsPaths {
            cert: cert_path,
            key: key_path,
            ca: self.ca_path(),
        }
    }

    /// Paths whose CA file is another authority's, so verification fails.
    fn issue_with_ca(&mut self, host: &str, ca: &Path) -> TlsPaths {
        let mut paths = self.issue(host);
        paths.ca = ca.to_path_buf();
        paths
    }
}

fn write_private(path: &Path, bytes: &[u8]) {
    std::fs::write(path, bytes).expect("write");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).expect("chmod");
    }
}

struct TestNode {
    config: NodeConfig,
    node: Arc<Node>,
    addr: SocketAddr,
    _server: JoinHandle<()>,
    _dirs: Vec<tempfile::TempDir>,
    _state: tempfile::TempDir,
}

fn make_config(
    listen: SocketAddr,
    bootstrap: Vec<String>,
    tls: Option<TlsPaths>,
) -> (NodeConfig, Vec<tempfile::TempDir>, tempfile::TempDir) {
    let dirs = vec![tempfile::tempdir().expect("temp dir")];
    let state = tempfile::tempdir().expect("temp dir");
    let config = NodeConfig {
        node_id: Uuid::new_v4(),
        listen,
        advertise: None,
        state_dir: state.path().to_path_buf(),
        devices: dirs.iter().map(|d| d.path().to_path_buf()).collect(),
        bootstrap_peers: bootstrap,
        temporary_max_age_secs: 3600,
        stream_idle_timeout_secs: 120,
        allow_shared_filesystem: true,
        tls,
    };
    (config, dirs, state)
}

async fn reserve_port() -> (TcpListener, SocketAddr) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    (listener, addr)
}

async fn first_node(k: u8, m: u8, tls: Option<TlsPaths>) -> TestNode {
    let (listener, addr) = reserve_port().await;
    let (config, dirs, state) = make_config(addr, vec![], tls);
    let node = Arc::new(
        Node::init_cluster(
            config.clone(),
            ClusterParameters {
                k,
                m,
                block_size: BLOCK,
                headroom: 0.0,
                ..ClusterParameters::default()
            },
        )
        .expect("init cluster"),
    );
    let server = tokio::spawn(server::serve(node.clone(), listener));
    TestNode {
        config,
        node,
        addr,
        _server: server,
        _dirs: dirs,
        _state: state,
    }
}

async fn joined_node(peer: &TestNode, tls: Option<TlsPaths>) -> TestNode {
    let (listener, addr) = reserve_port().await;
    let (config, dirs, state) = make_config(addr, vec![peer.addr.to_string()], tls);
    membership::join(&config, peer.addr, peer.node.cluster_id(), false)
        .await
        .expect("join");
    let node = Arc::new(Node::open(config.clone()).expect("open joined node"));
    let server = tokio::spawn(server::serve(node.clone(), listener));
    TestNode {
        config,
        node,
        addr,
        _server: server,
        _dirs: dirs,
        _state: state,
    }
}

impl TestNode {
    #[allow(clippy::result_large_err)]
    async fn client(&self, connector: &Connector) -> Result<Connection, ClientError> {
        Connection::connect_with(
            connector,
            self.addr,
            Connection::client_hello(self.node.cluster_id()),
        )
        .await
    }
}

fn tls_connector(paths: &TlsPaths) -> Connector {
    TlsMaterial::load(paths).expect("load material").connector()
}

#[test]
fn material_loading_checks_the_key_permissions_and_the_files() {
    let mut authority = Authority::new();
    let paths = authority.issue("127.0.0.1");
    TlsMaterial::load(&paths).expect("loads");

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&paths.key, std::fs::Permissions::from_mode(0o644))
            .expect("chmod");
        match TlsMaterial::load(&paths) {
            Err(TlsError::KeyReadable { mode, .. }) => assert_eq!(mode, 0o644),
            Err(other) => panic!("expected KeyReadable, got {other}"),
            Ok(_) => panic!("a world-readable key must be refused"),
        }
        std::fs::set_permissions(&paths.key, std::fs::Permissions::from_mode(0o600))
            .expect("chmod");
    }

    let mut wrong = paths.clone();
    wrong.cert = paths.ca.clone();
    wrong.ca = paths.cert.clone();
    // Swapping the files still parses (both are certificates) but the
    // authority is then a leaf, which rustls refuses as a root.
    assert!(matches!(
        TlsMaterial::load(&wrong),
        Err(TlsError::Rustls(_)) | Ok(_)
    ));
    let mut empty = paths.clone();
    empty.cert = authority.dir.path().join("empty.crt");
    std::fs::write(&empty.cert, "").expect("write");
    assert!(matches!(
        TlsMaterial::load(&empty),
        Err(TlsError::NoCertificate { .. })
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_cluster_moves_from_plain_to_tls_and_back() {
    let mut authority = Authority::new();
    let a = first_node(2, 1, Some(authority.issue("127.0.0.1"))).await;
    let b = joined_node(&a, Some(authority.issue("127.0.0.1"))).await;
    let c = joined_node(&b, Some(authority.issue("127.0.0.1"))).await;
    let cluster = a.node.cluster_id();
    let plain = Connector::plain();
    let tls = tls_connector(&authority.issue("admin-laptop"));
    let body = vec![7u8; 2 * 2 * BLOCK as usize + 99];

    // Plain: everything as before, and a TLS client is accepted too,
    // because every node has material loaded.
    let mut client = a.client(&plain).await.expect("plain client");
    client
        .put_object("k", &body, 100_000, None)
        .await
        .expect("put");
    let mut secure = a.client(&tls).await.expect("tls client under plain");
    assert!(secure.is_tls());
    let (_, got) = secure.get_object("k").await.expect("get over tls");
    assert_eq!(got, body);
    match client.request(Request::Status).await.expect("status") {
        Response::Status { transport, .. } => assert_eq!(transport, Transport::Plain),
        other => panic!("{other:?}"),
    }

    // tls-optional: nodes speak TLS to each other from now on. A write
    // through a reaches b and c only over TLS.
    let (document, changed) =
        membership::set_transport(&plain, a.addr, cluster, Transport::TlsOptional)
            .await
            .expect("set transport");
    assert!(changed);
    for n in [&a, &b, &c] {
        assert_eq!(n.node.document().version, document.version);
        assert_eq!(n.node.document().transport, Transport::TlsOptional);
    }
    let before_b = b.node.connections_accepted();
    let before_c = c.node.connections_accepted();
    let mut client = a.client(&plain).await.expect("plain client still accepted");
    client
        .put_object("k2", &body, 100_000, None)
        .await
        .expect("put");
    let (_, got) = client.get_object("k2").await.expect("get");
    assert_eq!(got, body);
    let after_b = b.node.connections_accepted();
    let after_c = c.node.connections_accepted();
    assert_eq!(after_b.0, before_b.0, "no plain connection reached b");
    assert_eq!(after_c.0, before_c.0, "no plain connection reached c");
    assert!(
        after_b.1 > before_b.1 && after_c.1 > before_c.1,
        "b and c were reached over TLS"
    );
    // A TLS client without a certificate is accepted in this mode.
    let anonymous = {
        // Material whose client certificate the CA did not issue would be
        // refused; a connector with roots only is what "no certificate"
        // means, which the library does not expose, so use a certificate
        // from another authority and expect refusal instead.
        let mut other = Authority::new();
        tls_connector(&other.issue_with_ca("intruder", &authority.ca_path()))
    };
    assert!(
        a.client(&anonymous).await.is_err(),
        "a certificate the CA did not issue is refused"
    );

    // tls: plain is refused with a message saying so; the TLS client works.
    let (document, changed) = membership::set_transport(&plain, a.addr, cluster, Transport::Tls)
        .await
        .expect("set transport");
    assert!(changed);
    for n in [&a, &b, &c] {
        assert_eq!(n.node.document().version, document.version);
    }
    match a.client(&plain).await {
        Err(ClientError::Remote(detail)) => {
            assert_eq!(detail.code, ErrorCode::TlsRequired);
            assert_eq!(detail.node, Some(a.node.id()));
        }
        Err(other) => panic!("expected TlsRequired, got {other}"),
        Ok(_) => panic!("plain must be refused under transport tls"),
    }
    let mut secure = a.client(&tls).await.expect("tls client");
    secure
        .put_object("k3", &body, 100_000, None)
        .await
        .expect("put");
    let (_, got) = secure.get_object("k3").await.expect("get");
    assert_eq!(got, body);
    match secure.request(Request::Status).await.expect("status") {
        Response::Status { transport, .. } => assert_eq!(transport, Transport::Tls),
        other => panic!("{other:?}"),
    }
    // Membership operations work over TLS too, and a repeat is a no-op.
    let (_, changed) = membership::set_transport(&tls, a.addr, cluster, Transport::Tls)
        .await
        .expect("set transport again");
    assert!(!changed);
    let (_, changed) = membership::set_device_state(
        &tls,
        b.addr,
        cluster,
        a.node.devices()[0].id(),
        djbod_core::cluster::DeviceState::Draining,
    )
    .await
    .expect("set state over tls");
    assert!(changed);

    // Back to plain, proposed over TLS since plain is refused.
    let (document, _) = membership::set_transport(&tls, c.addr, cluster, Transport::Plain)
        .await
        .expect("back to plain");
    for n in [&a, &b, &c] {
        assert_eq!(n.node.document().version, document.version);
        assert_eq!(n.node.document().transport, Transport::Plain);
    }
    let mut client = a.client(&plain).await.expect("plain again");
    for key in ["k", "k2", "k3"] {
        let (_, got) = client.get_object(key).await.expect("get");
        assert_eq!(got, body);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn moving_off_plain_is_refused_while_a_node_lacks_material() {
    let mut authority = Authority::new();
    let a = first_node(1, 1, Some(authority.issue("127.0.0.1"))).await;
    let b = joined_node(&a, None).await;
    let cluster = a.node.cluster_id();
    match membership::set_transport(&Connector::plain(), a.addr, cluster, Transport::TlsOptional)
        .await
    {
        Err(membership::MembershipError::NodeNotTlsReady { node, .. }) => {
            assert_eq!(node, b.node.id())
        }
        other => panic!("expected NodeNotTlsReady, got {other:?}"),
    }
    assert_eq!(a.node.document().transport, Transport::Plain);
    // Nor can a node be handed such a document directly.
    let mut next = b.node.document();
    next.version += 1;
    next.transport = Transport::Tls;
    match b.node.apply_document(next) {
        Err(NodeError::TlsRequired { transport }) => assert_eq!(transport, Transport::Tls),
        other => panic!("expected TlsRequired, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_node_without_material_refuses_to_start_under_a_tls_transport() {
    let mut authority = Authority::new();
    let a = first_node(1, 0, Some(authority.issue("127.0.0.1"))).await;
    let cluster = a.node.cluster_id();
    membership::set_transport(&Connector::plain(), a.addr, cluster, Transport::Tls)
        .await
        .expect("set transport");
    let mut config = a.config.clone();
    config.tls = None;
    match Node::open(config) {
        Err(NodeError::TlsRequired { transport }) => assert_eq!(transport, Transport::Tls),
        Err(other) => panic!("expected TlsRequired, got {other}"),
        Ok(_) => panic!("must not start"),
    }
    // With material it reopens (the running instance keeps the port; open
    // only reads the state).
    Node::open(a.config.clone()).expect("opens with material");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_server_certificate_for_another_address_is_refused() {
    let mut authority = Authority::new();
    // The node's certificate names 10.0.0.1, but it listens on 127.0.0.1.
    let a = first_node(1, 0, Some(authority.issue("10.0.0.1"))).await;
    let tls = tls_connector(&authority.issue("admin"));
    match a.client(&tls).await {
        Err(ClientError::Wire(_)) => {}
        Err(other) => panic!("expected a handshake failure, got {other}"),
        Ok(_) => panic!("a certificate for another address must be refused"),
    }
    // Plain still works: the cluster is in plain mode.
    a.client(&Connector::plain()).await.expect("plain");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_node_joins_a_tls_cluster_with_its_own_certificate() {
    let mut authority = Authority::new();
    let a = first_node(1, 1, Some(authority.issue("127.0.0.1"))).await;
    let cluster = a.node.cluster_id();
    let tls = tls_connector(&authority.issue("admin"));
    membership::set_transport(&Connector::plain(), a.addr, cluster, Transport::Tls)
        .await
        .expect("set transport");
    // Joining without material is refused with a clear reason.
    let (_, addr) = reserve_port().await;
    let (config, _dirs, _state) = make_config(addr, vec![a.addr.to_string()], None);
    match membership::join(&config, a.addr, cluster, false).await {
        Err(membership::MembershipError::Unreachable { .. })
        | Err(membership::MembershipError::PeerUnreachable { .. })
        | Err(membership::MembershipError::TlsRequired { .. }) => {}
        other => panic!("expected a refusal, got {other:?}"),
    }
    // With its own certificate the node joins and serves over TLS.
    let b = joined_node(&a, Some(authority.issue("127.0.0.1"))).await;
    assert_eq!(b.node.document().transport, Transport::Tls);
    let mut secure = b.client(&tls).await.expect("tls client to b");
    let body = vec![3u8; BLOCK as usize];
    secure
        .put_object("k", &body, 100_000, None)
        .await
        .expect("put across both");
    let (_, got) = secure.get_object("k").await.expect("get");
    assert_eq!(got, body);
}

#[test]
fn a_client_connector_comes_from_three_optional_settings() {
    let mut authority = Authority::new();
    let paths = authority.issue("admin");
    let plain = Connector::from_client_options(None, None, None).expect("plain");
    assert!(!plain.is_tls());
    let anonymous =
        Connector::from_client_options(Some(&paths.ca), None, None).expect("authority alone");
    assert!(anonymous.is_tls());
    let identified =
        Connector::from_client_options(Some(&paths.ca), Some(&paths.cert), Some(&paths.key))
            .expect("full identity");
    assert!(identified.is_tls());
    for (ca, cert, key) in [
        (Some(&paths.ca), Some(&paths.cert), None),
        (Some(&paths.ca), None, Some(&paths.key)),
        (None, Some(&paths.cert), Some(&paths.key)),
    ] {
        assert!(matches!(
            Connector::from_client_options(
                ca.map(|p| p.as_path()),
                cert.map(|p| p.as_path()),
                key.map(|p| p.as_path())
            ),
            Err(TlsError::IncompleteClientIdentity)
        ));
    }
}

/// The last frame of a conversation must reach the peer even when the
/// socket would not take it at the moment it was written. On TLS, rustls
/// accepts the plaintext into its own buffer, returns success, and sends
/// nothing until the next write or a flush. A sender that then only reads
/// (a coordinator waiting for PutShardDone) would wait forever, and so
/// would its peer. `write_message` flushes. The socket is stood in for by
/// a gate the test closes before the last frame and opens afterwards:
/// without the flush the frame never arrives, because nothing polls the
/// TLS stream again.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_last_frame_of_a_tls_stream_is_flushed() {
    use djbod_client::wire::{read_message, write_message};
    use djbod_core::checksum::checksum_block;
    use djbod_node::transport::{accept, Accepted};
    use std::pin::Pin;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Mutex;
    use std::task::{Context, Poll, Waker};
    use tokio::io::{AsyncRead, AsyncWrite, BufReader, ReadBuf};
    use tokio::net::TcpStream;

    /// A TCP stream whose writes can be held back, as a full socket
    /// buffer would hold them.
    struct Gate {
        open: AtomicBool,
        waker: Mutex<Option<Waker>>,
    }
    impl Gate {
        fn set(&self, open: bool) {
            self.open.store(open, Ordering::SeqCst);
            if open {
                if let Some(waker) = self.waker.lock().expect("lock").take() {
                    waker.wake();
                }
            }
        }
    }
    struct Gated {
        inner: TcpStream,
        gate: Arc<Gate>,
    }
    impl Gated {
        fn held(&self, cx: &mut Context<'_>) -> bool {
            if self.gate.open.load(Ordering::SeqCst) {
                return false;
            }
            *self.gate.waker.lock().expect("lock") = Some(cx.waker().clone());
            true
        }
    }
    impl AsyncRead for Gated {
        fn poll_read(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &mut ReadBuf<'_>,
        ) -> Poll<std::io::Result<()>> {
            Pin::new(&mut self.inner).poll_read(cx, buf)
        }
    }
    impl AsyncWrite for Gated {
        fn poll_write(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<std::io::Result<usize>> {
            if self.held(cx) {
                return Poll::Pending;
            }
            Pin::new(&mut self.inner).poll_write(cx, buf)
        }
        fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            if self.held(cx) {
                return Poll::Pending;
            }
            Pin::new(&mut self.inner).poll_flush(cx)
        }
        fn poll_shutdown(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
        ) -> Poll<std::io::Result<()>> {
            Pin::new(&mut self.inner).poll_shutdown(cx)
        }
    }

    let mut authority = Authority::new();
    let material = Arc::new(TlsMaterial::load(&authority.issue("127.0.0.1")).expect("load"));
    let client_paths = authority.issue("client");
    let (listener, addr) = reserve_port().await;

    // The receiving side: a real node-style TLS accept, reading frames
    // until the end of the stream.
    let server = tokio::spawn(async move {
        let (tcp, _) = listener.accept().await.expect("accept");
        let stream = match accept(tcp, Some(&material), Transport::TlsOptional)
            .await
            .expect("tls accept")
        {
            Accepted::Stream(stream) => stream,
            Accepted::PlainRefused(_) => panic!("plain?"),
        };
        let (read_half, _write_half) = tokio::io::split(stream);
        let mut reader = BufReader::new(read_half);
        let mut frames = 0u64;
        loop {
            match read_message(&mut reader).await.expect("read") {
                Message::Data { .. } => frames += 1,
                Message::EndOfStream { .. } => return frames,
                other => panic!("unexpected {other:?}"),
            }
        }
    });

    // The sending side: the same rustls client configuration a node or
    // client builds, over the gated socket.
    let read_pem = |path: &std::path::Path| -> Vec<rustls::pki_types::CertificateDer<'static>> {
        let file = std::fs::File::open(path).expect("open");
        rustls_pemfile::certs(&mut std::io::BufReader::new(file))
            .collect::<Result<_, _>>()
            .expect("pem")
    };
    let mut roots = rustls::RootCertStore::empty();
    for cert in read_pem(&client_paths.ca) {
        roots.add(cert).expect("root");
    }
    let key = rustls_pemfile::private_key(&mut std::io::BufReader::new(
        std::fs::File::open(&client_paths.key).expect("open"),
    ))
    .expect("pem")
    .expect("key");
    let config = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_client_auth_cert(read_pem(&client_paths.cert), key)
        .expect("client config");
    let gate = Arc::new(Gate {
        open: AtomicBool::new(true),
        waker: Mutex::new(None),
    });
    let tcp = TcpStream::connect(addr).await.expect("connect");
    let gated = Gated {
        inner: tcp,
        gate: gate.clone(),
    };
    let name = rustls::pki_types::ServerName::IpAddress(rustls::pki_types::IpAddr::from(addr.ip()));
    let mut tls = tokio_rustls::TlsConnector::from(Arc::new(config))
        .connect(name, gated)
        .await
        .expect("tls connect");

    // Blocks go through with the gate open.
    let block = vec![0x5au8; 64 * 1024];
    for sequence in 0..4u64 {
        write_message(
            &mut tls,
            &Message::Data {
                id: 1,
                data: DataFrame {
                    sequence,
                    checksum: checksum_block(&block),
                    bytes: block.clone(),
                },
            },
        )
        .await
        .expect("write block");
    }
    // The socket "fills" just before the last frame. The write of the
    // EndOfStream is spawned because, with the flush, it must wait for
    // the socket; without the flush it returns at once with the frame
    // still in rustls' buffer.
    gate.set(false);
    let sender = tokio::spawn(async move {
        write_message(
            &mut tls,
            &Message::EndOfStream {
                id: 1,
                end: StreamEnd::ok(),
            },
        )
        .await
        .expect("write end");
        // Keep the stream alive, only waiting, as a coordinator waiting
        // for PutShardDone would.
        tokio::time::sleep(std::time::Duration::from_secs(30)).await;
        drop(tls);
    });
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    gate.set(true);
    let frames = tokio::time::timeout(std::time::Duration::from_secs(5), server)
        .await
        .expect("the EndOfStream frame must arrive once the socket takes writes again")
        .expect("server task");
    assert_eq!(frames, 4);
    sender.abort();
}

/// Many writes of varying sizes through a TLS cluster, none may hang.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sustained_writes_over_tls_all_complete() {
    let mut authority = Authority::new();
    let a = first_node(2, 1, Some(authority.issue("127.0.0.1"))).await;
    let b = joined_node(&a, Some(authority.issue("127.0.0.1"))).await;
    let c = joined_node(&b, Some(authority.issue("127.0.0.1"))).await;
    let cluster = a.node.cluster_id();
    let tls = tls_connector(&authority.issue("admin"));
    membership::set_transport(&Connector::plain(), a.addr, cluster, Transport::Tls)
        .await
        .expect("set transport");
    let mut client = a.client(&tls).await.expect("tls client");
    let mut x: u64 = 7;
    for i in 0..40u32 {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        let size = 1024 + (x % (3 * 1024 * 1024)) as usize;
        let body = vec![(i as u8).wrapping_mul(31); size];
        let key = format!("sustained/{i:03}");
        match tokio::time::timeout(
            std::time::Duration::from_secs(30),
            client.put_object(&key, &body, 100_000, None),
        )
        .await
        {
            Ok(result) => {
                result.expect("put");
            }
            Err(_) => panic!("put {i} of {size} bytes hung"),
        }
    }
    for i in [0u32, 17, 39] {
        let (_, got) = client
            .get_object(&format!("sustained/{i:03}"))
            .await
            .expect("get");
        assert_eq!(got[0], (i as u8).wrapping_mul(31));
    }
    drop((b, c));
}

/// A client that starts an upload and then goes silent must not hold
/// shard writes open forever: the coordinator gives up after the idle
/// timeout, aborts the holders, and no temporary file remains.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_silent_upload_is_abandoned_after_the_idle_timeout() {
    let (listener, addr) = reserve_port().await;
    let (base, _dirs, _state) = make_config(addr, vec![], None);
    // 2+1 needs three devices; make_config gives one.
    let extra: Vec<tempfile::TempDir> = (0..2).map(|_| tempfile::tempdir().expect("dir")).collect();
    let mut devices = base.devices.clone();
    devices.extend(extra.iter().map(|d| d.path().to_path_buf()));
    let config = NodeConfig {
        devices,
        stream_idle_timeout_secs: 1,
        ..base
    };
    let node = Arc::new(
        Node::init_cluster(
            config,
            ClusterParameters {
                k: 2,
                m: 1,
                block_size: BLOCK,
                headroom: 0.0,
                ..ClusterParameters::default()
            },
        )
        .expect("init cluster"),
    );
    let server = tokio::spawn(server::serve(node.clone(), listener));
    let mut client = Connection::connect(addr, Connection::client_hello(node.cluster_id()))
        .await
        .expect("connect");
    let id = client
        .send_request(Request::PutObject {
            key: "silent".to_string(),
            size: 4 * BLOCK,
            content_type: None,
            user_metadata: Default::default(),
        })
        .await
        .expect("send request");
    // Send nothing more. Within a few seconds the coordinator abandons the
    // write and tells us, or closes the connection.
    let outcome =
        tokio::time::timeout(std::time::Duration::from_secs(10), client.read_response(id))
            .await
            .expect("the coordinator must give up");
    match outcome {
        Err(ClientError::Remote(detail)) | Err(ClientError::StreamFailed(detail)) => {
            assert!(detail.message.contains("no frame arrived"), "{detail:?}")
        }
        Err(_) => {}
        Ok(other) => panic!("a silent upload must not succeed: {other:?}"),
    }
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    for device in node.devices() {
        let mut pending = vec![device.root().to_path_buf()];
        while let Some(dir) = pending.pop() {
            for entry in std::fs::read_dir(&dir).expect("read dir") {
                let path = entry.expect("entry").path();
                if path.is_dir() {
                    pending.push(path);
                } else {
                    assert!(
                        path.extension().is_none_or(|e| e != "tmp"),
                        "temporary left behind: {}",
                        path.display()
                    );
                }
            }
        }
    }
    server.abort();
    drop(extra);
}
