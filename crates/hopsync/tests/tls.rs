//! The web UI over real HTTPS: what a browser on another machine meets at `https://ip:port`.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use hopsync::tls::{Source, TlsListener, fingerprint_of, load};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{WebPkiSupportedAlgorithms, aws_lc_rs};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{ClientConfig, DigitallySignedStruct, SignatureScheme};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// Accepts any certificate and keeps it, as a browser does once the user accepts the warning.
#[derive(Debug)]
struct Keep {
    seen: Mutex<Option<Vec<u8>>>,
    algorithms: WebPkiSupportedAlgorithms,
}

impl ServerCertVerifier for Keep {
    fn verify_server_cert(
        &self,
        end: &CertificateDer<'_>,
        _: &[CertificateDer<'_>],
        _: &ServerName<'_>,
        _: &[u8],
        _: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        *self.seen.lock().unwrap() = Some(end.to_vec());
        Ok(ServerCertVerified::assertion())
    }
    fn verify_tls12_signature(
        &self,
        m: &[u8],
        c: &CertificateDer<'_>,
        d: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(m, c, d, &self.algorithms)
    }
    fn verify_tls13_signature(
        &self,
        m: &[u8],
        c: &CertificateDer<'_>,
        d: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(m, c, d, &self.algorithms)
    }
    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.algorithms.supported_schemes()
    }
}

fn client() -> (tokio_rustls::TlsConnector, Arc<Keep>) {
    let provider = aws_lc_rs::default_provider();
    let keep = Arc::new(Keep {
        seen: Mutex::new(None),
        algorithms: provider.signature_verification_algorithms,
    });
    let config = ClientConfig::builder_with_provider(Arc::new(provider))
        .with_safe_default_protocol_versions()
        .unwrap()
        .dangerous()
        .with_custom_certificate_verifier(keep.clone())
        .with_no_client_auth();
    (tokio_rustls::TlsConnector::from(Arc::new(config)), keep)
}

fn generated(dir: &std::path::Path, name: &str) -> Source {
    Source::Generated {
        data_dir: dir.to_owned(),
        names: vec![name.to_owned()],
    }
}

/// A server on `127.0.0.1` answering every connection with `hello`, through the real listener.
async fn server(source: &Source) -> (SocketAddr, String) {
    let loaded = load(source).unwrap();
    let tcp = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = tcp.local_addr().unwrap();
    let listener = TlsListener::new(tcp, loaded.server);
    let app = axum::Router::new().route("/", axum::routing::get(|| async { "hello" }));
    tokio::spawn(async move { axum::serve(listener, app).await });
    (addr, loaded.fingerprint)
}

const GET: &[u8] = b"GET / HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n";

async fn https_get(addr: SocketAddr) -> (String, Vec<u8>) {
    let (connector, keep) = client();
    let tcp = TcpStream::connect(addr).await.unwrap();
    let name = ServerName::try_from("hopsync.test").unwrap();
    let mut tls = connector.connect(name, tcp).await.unwrap();
    tls.write_all(GET).await.unwrap();
    let mut out = String::new();
    tls.read_to_string(&mut out).await.unwrap();
    let cert = keep.seen.lock().unwrap().clone().unwrap();
    (out, cert)
}

#[tokio::test]
async fn https_works_and_the_browser_sees_the_fingerprint_setup_prints() {
    let dir = tempfile::tempdir().unwrap();
    let source = generated(dir.path(), "203.0.113.7");
    let (addr, fingerprint) = server(&source).await;
    let (reply, cert) = https_get(addr).await;
    assert!(reply.ends_with("hello"), "{reply}");
    assert_eq!(hopsync::tls::fingerprint(&cert), fingerprint);
    assert_eq!(
        fingerprint_of(&dir.path().join("tls-cert.pem")).unwrap(),
        fingerprint
    );
    assert_eq!(
        fingerprint.len(),
        95,
        "colon-separated SHA-256: {fingerprint}"
    );
}

#[test]
fn the_certificate_stays_across_restarts_and_changes_with_the_address() {
    let dir = tempfile::tempdir().unwrap();
    let first = load(&generated(dir.path(), "203.0.113.7")).unwrap();
    let again = load(&generated(dir.path(), "203.0.113.7")).unwrap();
    assert_eq!(first.fingerprint, again.fingerprint);
    let moved = load(&generated(dir.path(), "203.0.113.8")).unwrap();
    assert_ne!(first.fingerprint, moved.fingerprint);
}

#[test]
fn the_private_key_is_private() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    load(&generated(dir.path(), "203.0.113.7")).unwrap();
    let mode = std::fs::metadata(dir.path().join("tls-key.pem"))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600);
}

#[test]
fn a_certificate_file_the_user_named_is_used_and_bad_ones_say_which() {
    let dir = tempfile::tempdir().unwrap();
    load(&generated(dir.path(), "own.example")).unwrap();
    let (cert, key) = (
        dir.path().join("tls-cert.pem"),
        dir.path().join("tls-key.pem"),
    );
    let good = Source::Files {
        cert: cert.clone(),
        key: key.clone(),
    };
    assert_eq!(
        load(&good).unwrap().fingerprint,
        fingerprint_of(&cert).unwrap()
    );

    let missing = Source::Files {
        cert: dir.path().join("nope.pem"),
        key: key.clone(),
    };
    assert!(load(&missing).err().unwrap().contains("nope.pem"));

    // The certificate is not a key: a mistake of the two files must not start the daemon.
    let swapped = Source::Files {
        cert: cert.clone(),
        key: cert.clone(),
    };
    assert!(load(&swapped).is_err());

    let other = tempfile::tempdir().unwrap();
    load(&generated(other.path(), "other.example")).unwrap();
    let mismatch = Source::Files {
        cert,
        key: other.path().join("tls-key.pem"),
    };
    assert!(load(&mismatch).is_err(), "a key from another certificate");
}

#[tokio::test]
async fn a_stalled_client_does_not_stop_others() {
    let dir = tempfile::tempdir().unwrap();
    let (addr, _) = server(&generated(dir.path(), "203.0.113.7")).await;
    // These connect and never say anything.
    let mut idle = Vec::new();
    for _ in 0..8 {
        idle.push(TcpStream::connect(addr).await.unwrap());
    }
    let (reply, _) = tokio::time::timeout(Duration::from_secs(5), https_get(addr))
        .await
        .expect("a stalled client blocked the listener");
    assert!(reply.ends_with("hello"));
}

#[tokio::test]
async fn one_address_cannot_use_up_the_connections() {
    let dir = tempfile::tempdir().unwrap();
    let (addr, _) = server(&generated(dir.path(), "203.0.113.7")).await;
    let mut held = Vec::new();
    for _ in 0..hopsync::tls::MAX_PER_ADDRESS {
        held.push(TcpStream::connect(addr).await.unwrap());
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
    let mut extra = TcpStream::connect(addr).await.unwrap();
    let mut buf = [0u8; 1];
    let closed = tokio::time::timeout(Duration::from_secs(2), extra.read(&mut buf)).await;
    assert!(matches!(closed, Ok(Ok(0)) | Ok(Err(_))), "{closed:?}");

    // A slot comes back when its connection closes.
    drop(held.pop());
    tokio::time::sleep(Duration::from_millis(100)).await;
    let (reply, _) = tokio::time::timeout(Duration::from_secs(5), https_get(addr))
        .await
        .unwrap();
    assert!(reply.ends_with("hello"));
}

#[tokio::test]
async fn plain_http_on_loopback_still_works_for_tunnels_and_setup() {
    let dir = tempfile::tempdir().unwrap();
    let (addr, _) = server(&generated(dir.path(), "203.0.113.7")).await;
    let mut tcp = TcpStream::connect(addr).await.unwrap();
    tcp.write_all(GET).await.unwrap();
    let mut out = String::new();
    tcp.read_to_string(&mut out).await.unwrap();
    assert!(out.ends_with("hello"), "{out}");
}

#[test]
fn plain_http_from_elsewhere_is_sent_to_https() {
    use hopsync::tls::redirect_response;
    let reply = |req: &str| String::from_utf8(redirect_response(req.as_bytes())).unwrap();

    let ok = reply("GET / HTTP/1.1\r\nHost: 203.0.113.7:7890\r\n\r\n");
    assert!(ok.starts_with("HTTP/1.1 301"), "{ok}");
    assert!(
        ok.contains("\r\nLocation: https://203.0.113.7:7890/\r\n"),
        "{ok}"
    );
    let lower = reply("GET / HTTP/1.1\r\nhost: box.example:7890\r\n\r\n");
    assert!(
        lower.contains("Location: https://box.example:7890/"),
        "{lower}"
    );

    // Nothing from the request may shape a header or the page.
    for bad in [
        "GET / HTTP/1.1\r\nHost: a\u{b}b\r\n\r\n",
        "GET / HTTP/1.1\r\nHost: a b\r\n\r\n",
        "GET / HTTP/1.1\r\nHost: evil/<script>\r\n\r\n",
        "GET / HTTP/1.1\r\n\r\n",
        "not http at all",
    ] {
        let out = reply(bad);
        assert!(out.starts_with("HTTP/1.1 400"), "{bad:?}: {out}");
        assert!(!out.contains("evil") && !out.contains("script"), "{out}");
    }
}

/// The whole daemon over HTTPS: the login a browser makes, with the cookie it must get.
#[tokio::test]
async fn logging_in_over_https_sets_a_secure_cookie() {
    use hopsync::daemon;
    use hopsync::service::{Service, ServiceConfig};
    use hopsync::ssh::SshOptions;
    use std::os::unix::fs::DirBuilderExt;

    let data = tempfile::tempdir().unwrap();
    let local = tempfile::tempdir().unwrap();
    let svc = Service::open(ServiceConfig {
        data_dir: data.path().join("data"),
        local_root: local.path().to_owned(),
        hidden: Default::default(),
        concurrency: 1,
        ssh: SshOptions::default(),
    })
    .unwrap();
    let password = "correct horse battery staple";
    let token = svc.auth.setup_token().unwrap();
    svc.auth
        .setup(&token, password, "192.0.2.9".parse().unwrap())
        .unwrap();
    let loaded = load(&generated(data.path(), "203.0.113.7")).unwrap();
    let tcp = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = tcp.local_addr().unwrap();
    let run = data.path().join("run");
    std::fs::DirBuilder::new().mode(0o700).create(&run).unwrap();
    let sock = daemon::bind_socket(&run.join("s.sock")).await.unwrap();
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn(daemon::serve(
        svc,
        tcp,
        sock,
        false,
        Some(loaded.server),
        async {
            let _ = stopped.await;
        },
        // As a public daemon reports itself, so `Host` need not be a loopback name.
        "203.0.113.7:7890".parse().unwrap(),
    ));

    let (connector, _) = client();
    let stream = TcpStream::connect(addr).await.unwrap();
    let name = ServerName::try_from("hopsync.test").unwrap();
    let mut tls = connector.connect(name, stream).await.unwrap();
    let body = format!(r#"{{"password":"{password}"}}"#);
    let request = format!(
        "POST /api/login HTTP/1.1\r\nHost: 203.0.113.7:7890\r\nOrigin: https://203.0.113.7:7890\r\n\
         Sec-Fetch-Site: same-origin\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\
         Connection: close\r\n\r\n{body}",
        body.len()
    );
    tls.write_all(request.as_bytes()).await.unwrap();
    let mut reply = String::new();
    tls.read_to_string(&mut reply).await.unwrap();
    assert!(reply.starts_with("HTTP/1.1 200"), "{reply}");
    let cookie = reply
        .lines()
        .find(|l| l.to_ascii_lowercase().starts_with("set-cookie:"))
        .unwrap();
    assert!(cookie.contains("; Secure"), "{cookie}");
    assert!(cookie.contains("HttpOnly"), "{cookie}");
    assert!(cookie.contains("SameSite=Strict"), "{cookie}");
    assert!(
        !reply.to_ascii_lowercase().contains("strict-transport"),
        "{reply}"
    );

    let _ = stop.send(());
    server.await.unwrap().unwrap();
}

/// The login throttle goes by this address, so every client must not look like the same one.
#[tokio::test]
async fn handlers_see_the_real_peer_address() {
    use axum::extract::ConnectInfo;
    let dir = tempfile::tempdir().unwrap();
    let loaded = load(&generated(dir.path(), "203.0.113.7")).unwrap();
    let tcp = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = tcp.local_addr().unwrap();
    let listener = TlsListener::new(tcp, loaded.server);
    let app = hopsync::tls::with_peer(axum::Router::new().route(
        "/",
        axum::routing::get(|ConnectInfo(peer): ConnectInfo<SocketAddr>| async move {
            format!("peer={}", peer.port())
        }),
    ));
    tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<hopsync::tls::Peer>(),
        )
        .await
    });

    let (connector, _) = client();
    let stream = TcpStream::connect(addr).await.unwrap();
    let port = stream.local_addr().unwrap().port();
    let name = ServerName::try_from("hopsync.test").unwrap();
    let mut tls = connector.connect(name, stream).await.unwrap();
    tls.write_all(GET).await.unwrap();
    let mut out = String::new();
    tls.read_to_string(&mut out).await.unwrap();
    assert!(out.ends_with(&format!("peer={port}")), "{out}");
}
