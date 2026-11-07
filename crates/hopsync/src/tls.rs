//! HTTPS for the web UI, so it can be opened at the server's own address (Decision log 2026-11-07).
//!
//! The certificate is one this daemon generates, or files the user names. A browser cannot trust a
//! generated one, so it warns once, and comparing the fingerprint setup prints with the one in the
//! warning is what makes that first visit safe. There is no HSTS for that reason: it would forbid
//! accepting the warning at all.
//!
//! The listener faces the internet. Each connection is handled in its own task with a time limit, so
//! a client that says nothing cannot hold up the others, and connections are capped per address.
//! The first byte picks what a connection gets: TLS goes to HTTPS, plain HTTP from this machine is
//! served as it is (SSH tunnels and setup's own password call), and plain HTTP from elsewhere gets a
//! redirect, because `http://ip:port` is what people type.

use std::collections::HashMap;
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;

use rustls::ServerConfig;
use rustls_pki_types::pem::PemObject;
use rustls_pki_types::{CertificateDer, PrivateKeyDer};
use time::OffsetDateTime;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt, ReadBuf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_rustls::TlsAcceptor;
use tokio_rustls::server::TlsStream;

const CERT_FILE: &str = "tls-cert.pem";
const KEY_FILE: &str = "tls-key.pem";
/// What the generated certificate was made for: its names, then when it expires.
const STATE_FILE: &str = "tls-cert.state";
/// Apple's platforms reject a TLS certificate valid for longer.
const VALID_DAYS: i64 = 825;
const RENEW_WITHIN_DAYS: i64 = 30;
/// Time to send the first byte and finish the handshake, each.
const HANDSHAKE_LIMIT: Duration = Duration::from_secs(10);
/// A browser opens a handful per tab; this leaves room for several people behind one address.
pub const MAX_PER_ADDRESS: usize = 32;
const MAX_TOTAL: usize = 1024;
/// Enough to hold a browser's `Host` header.
const PEEK_BYTES: usize = 2048;
const TLS_HANDSHAKE_RECORD: u8 = 0x16;

/// Where the certificate comes from.
#[derive(Debug, Clone)]
pub enum Source {
    /// Generated in `data_dir` for these names, and kept until they change or it nears expiry.
    Generated {
        data_dir: PathBuf,
        names: Vec<String>,
    },
    /// Files the user named. Read once, so a renewed certificate needs a restart.
    Files { cert: PathBuf, key: PathBuf },
}

pub struct Loaded {
    pub server: Arc<ServerConfig>,
    pub fingerprint: String,
}

/// SHA-256 of a certificate, as browsers show it: colon-separated upper-case hex.
pub fn fingerprint(der: &[u8]) -> String {
    let digest = aws_lc_rs::digest::digest(&aws_lc_rs::digest::SHA256, der);
    let hex: Vec<String> = digest.as_ref().iter().map(|b| format!("{b:02X}")).collect();
    hex.join(":")
}

/// The fingerprint of the first certificate in a PEM file.
pub fn fingerprint_of(path: &Path) -> Result<String, String> {
    let first = read_chain(path)?.into_iter().next();
    first
        .map(|c| fingerprint(&c))
        .ok_or_else(|| format!("{}: no certificate in the file", path.display()))
}

fn read_chain(path: &Path) -> Result<Vec<CertificateDer<'static>>, String> {
    CertificateDer::pem_file_iter(path)
        .and_then(Iterator::collect)
        .map_err(|e| format!("certificate {}: {e}", path.display()))
}

/// Reads or generates the certificate and builds the TLS settings. TLS 1.3 only: every browser
/// that can run the web UI has it, and there is no older protocol to configure safely.
pub fn load(source: &Source) -> Result<Loaded, String> {
    let (cert, key) = match source {
        Source::Generated { data_dir, names } => {
            ensure_generated(data_dir, names)?;
            (data_dir.join(CERT_FILE), data_dir.join(KEY_FILE))
        }
        Source::Files { cert, key } => (cert.clone(), key.clone()),
    };
    let chain = read_chain(&cert)?;
    let first = chain
        .first()
        .ok_or_else(|| format!("certificate {}: no certificate in the file", cert.display()))?;
    let fingerprint = fingerprint(first);
    let private =
        PrivateKeyDer::from_pem_file(&key).map_err(|e| format!("key {}: {e}", key.display()))?;
    let mut server = ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::aws_lc_rs::default_provider(),
    ))
    .with_protocol_versions(&[&rustls::version::TLS13])
    .map_err(|e| e.to_string())?
    .with_no_client_auth()
    .with_single_cert(chain, private)
    .map_err(|e| {
        format!(
            "certificate {} and key {} do not work together: {e}",
            cert.display(),
            key.display()
        )
    })?;
    server.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(Loaded {
        server: Arc::new(server),
        fingerprint,
    })
}

fn state_text(names: &[String], expires: i64) -> String {
    format!("{}\n{expires}\n", names.join(","))
}

/// Keeps the certificate users have already accepted, unless it no longer fits.
fn ensure_generated(data_dir: &Path, names: &[String]) -> Result<(), String> {
    let now = OffsetDateTime::now_utc();
    let state = std::fs::read_to_string(data_dir.join(STATE_FILE)).unwrap_or_default();
    let fits = state.lines().next() == Some(names.join(",").as_str())
        && state
            .lines()
            .nth(1)
            .and_then(|e| e.parse::<i64>().ok())
            .is_some_and(|e| e > now.unix_timestamp() + RENEW_WITHIN_DAYS * 86_400);
    if fits && data_dir.join(CERT_FILE).exists() && data_dir.join(KEY_FILE).exists() {
        return Ok(());
    }

    let mut all = names.to_vec();
    for local in ["localhost", "127.0.0.1"] {
        if !all.iter().any(|n| n == local) {
            all.push(local.to_owned());
        }
    }
    let failed = |e: rcgen::Error| format!("generating the certificate: {e}");
    let mut params = rcgen::CertificateParams::new(all).map_err(failed)?;
    params
        .distinguished_name
        .push(rcgen::DnType::CommonName, "Hopsync");
    let expires = now + time::Duration::days(VALID_DAYS);
    params.not_before = now - time::Duration::days(1);
    params.not_after = expires;
    let key = rcgen::KeyPair::generate().map_err(failed)?;
    let cert = params.self_signed(&key).map_err(failed)?;

    std::fs::create_dir_all(data_dir).map_err(|e| format!("{}: {e}", data_dir.display()))?;
    // The key first and the state last: a crash in between makes the next start generate again.
    write_private(&data_dir.join(KEY_FILE), key.serialize_pem().as_bytes())?;
    write_private(&data_dir.join(CERT_FILE), cert.pem().as_bytes())?;
    write_private(
        &data_dir.join(STATE_FILE),
        state_text(names, expires.unix_timestamp()).as_bytes(),
    )
}

/// Replaces `path` in one step, owner-only from the moment it exists.
fn write_private(path: &Path, content: &[u8]) -> Result<(), String> {
    use std::io::Write;
    let failed = |e: io::Error| format!("{}: {e}", path.display());
    let temp = path.with_extension("new");
    let _ = std::fs::remove_file(&temp);
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temp)
        .map_err(failed)?;
    file.write_all(content).map_err(failed)?;
    file.sync_all().map_err(failed)?;
    std::fs::rename(&temp, path).map_err(failed)
}

/// The reply to plain HTTP from another machine. Only a checked `Host` reaches it, so a request
/// cannot add a header or send the browser anywhere but the name it already asked for.
pub fn redirect_response(request: &[u8]) -> Vec<u8> {
    let host = std::str::from_utf8(request)
        .ok()
        .filter(|r| r.contains(" HTTP/1."))
        .and_then(|r| {
            r.lines()
                .filter_map(|l| l.split_once(':'))
                .find(|(name, _)| name.eq_ignore_ascii_case("host"))
                .map(|(_, value)| value.trim().to_owned())
        })
        .filter(|h| {
            !h.is_empty()
                && h.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b".-:[]".contains(&b))
        });
    match host {
        Some(host) => format!(
            "HTTP/1.1 301 Moved Permanently\r\nLocation: https://{host}/\r\nContent-Length: 0\r\n\
             Connection: close\r\n\r\n"
        )
        .into_bytes(),
        None => {
            let body = "This port serves HTTPS. Open it with https://\n";
            format!(
                "HTTP/1.1 400 Bad Request\r\nContent-Type: text/plain\r\nContent-Length: {}\r\n\
                 Connection: close\r\n\r\n{body}",
                body.len()
            )
            .into_bytes()
        }
    }
}

#[derive(Default)]
struct Counts {
    per_address: HashMap<IpAddr, usize>,
    total: usize,
}

/// A connection's place in the limits, given back when the connection closes.
struct Slot {
    counts: Arc<Mutex<Counts>>,
    address: IpAddr,
}

impl Drop for Slot {
    fn drop(&mut self) {
        let mut c = self.counts.lock().unwrap_or_else(|p| p.into_inner());
        c.total -= 1;
        if let Some(n) = c.per_address.get_mut(&self.address) {
            *n -= 1;
            if *n == 0 {
                c.per_address.remove(&self.address);
            }
        }
    }
}

fn admit(counts: &Arc<Mutex<Counts>>, address: IpAddr) -> Option<Slot> {
    let mut c = counts.lock().unwrap_or_else(|p| p.into_inner());
    if c.total >= MAX_TOTAL || c.per_address.get(&address).copied().unwrap_or(0) >= MAX_PER_ADDRESS
    {
        return None;
    }
    c.total += 1;
    *c.per_address.entry(address).or_insert(0) += 1;
    Some(Slot {
        counts: counts.clone(),
        address,
    })
}

enum Stream {
    Plain(TcpStream),
    Tls(Box<TlsStream<TcpStream>>),
}

/// One accepted connection, plain or TLS, holding its place in the limits.
pub struct Connection {
    stream: Stream,
    _slot: Slot,
}

impl AsyncRead for Connection {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        match &mut self.get_mut().stream {
            Stream::Plain(s) => Pin::new(s).poll_read(cx, buf),
            Stream::Tls(s) => Pin::new(s).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for Connection {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        match &mut self.get_mut().stream {
            Stream::Plain(s) => Pin::new(s).poll_write(cx, buf),
            Stream::Tls(s) => Pin::new(s).poll_write(cx, buf),
        }
    }
    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match &mut self.get_mut().stream {
            Stream::Plain(s) => Pin::new(s).poll_flush(cx),
            Stream::Tls(s) => Pin::new(s).poll_flush(cx),
        }
    }
    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match &mut self.get_mut().stream {
            Stream::Plain(s) => Pin::new(s).poll_shutdown(cx),
            Stream::Tls(s) => Pin::new(s).poll_shutdown(cx),
        }
    }
}

/// Works out what a new connection is. `None` means it has been dealt with or given up on.
async fn open(stream: TcpStream, peer: IpAddr, acceptor: TlsAcceptor) -> Option<Stream> {
    let _ = stream.set_nodelay(true);
    let mut first = [0u8; PEEK_BYTES];
    let n = tokio::time::timeout(HANDSHAKE_LIMIT, stream.peek(&mut first))
        .await
        .ok()?
        .ok()?;
    if n == 0 {
        return None;
    }
    if first[0] == TLS_HANDSHAKE_RECORD {
        let tls = tokio::time::timeout(HANDSHAKE_LIMIT, acceptor.accept(stream))
            .await
            .ok()?
            .ok()?;
        return Some(Stream::Tls(Box::new(tls)));
    }
    if peer.is_loopback() {
        return Some(Stream::Plain(stream));
    }
    let mut stream = stream;
    let reply = redirect_response(&first[..n]);
    let _ = tokio::time::timeout(HANDSHAKE_LIMIT, async {
        let _ = stream.write_all(&reply).await;
        let _ = stream.shutdown().await;
    })
    .await;
    None
}

/// The accept loop lives in a task of its own, and each connection in another.
async fn accept_loop(
    tcp: TcpListener,
    acceptor: TlsAcceptor,
    ready: mpsc::Sender<(Connection, SocketAddr)>,
) {
    let counts = Arc::new(Mutex::new(Counts::default()));
    loop {
        let (stream, addr) = match tcp.accept().await {
            Ok(accepted) => accepted,
            // Out of file descriptors and the like: wait a moment instead of spinning.
            Err(_) => {
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
        };
        let peer = addr.ip().to_canonical();
        // Over the limit: dropping the stream closes it.
        let Some(slot) = admit(&counts, peer) else {
            continue;
        };
        let (acceptor, ready) = (acceptor.clone(), ready.clone());
        tokio::spawn(async move {
            if let Some(stream) = open(stream, peer, acceptor).await {
                let connection = Connection {
                    stream,
                    _slot: slot,
                };
                let _ = ready.send((connection, addr)).await;
            }
        });
    }
}

/// An `axum::serve` listener that serves HTTPS.
pub struct TlsListener {
    ready: mpsc::Receiver<(Connection, SocketAddr)>,
    local: io::Result<SocketAddr>,
    accepting: JoinHandle<()>,
}

impl TlsListener {
    pub fn new(tcp: TcpListener, server: Arc<ServerConfig>) -> Self {
        let local = tcp.local_addr();
        let (tx, ready) = mpsc::channel(64);
        let accepting = tokio::spawn(accept_loop(tcp, TlsAcceptor::from(server), tx));
        Self {
            ready,
            local,
            accepting,
        }
    }
}

impl Drop for TlsListener {
    fn drop(&mut self) {
        self.accepting.abort();
    }
}

impl axum::serve::Listener for TlsListener {
    type Io = Connection;
    type Addr = SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        match self.ready.recv().await {
            Some(ready) => ready,
            // The accept loop never returns while this listener holds its handle.
            None => std::future::pending().await,
        }
    }

    fn local_addr(&self) -> io::Result<Self::Addr> {
        match &self.local {
            Ok(a) => Ok(*a),
            Err(e) => Err(io::Error::new(e.kind(), e.to_string())),
        }
    }
}

/// The peer's address as this listener reports it. Axum only knows how to get a `SocketAddr` from
/// its own listeners, and the rules on foreign traits keep that from being added from here.
#[derive(Clone, Copy)]
pub struct Peer(SocketAddr);

impl axum::extract::connect_info::Connected<axum::serve::IncomingStream<'_, TlsListener>> for Peer {
    fn connect_info(stream: axum::serve::IncomingStream<'_, TlsListener>) -> Self {
        Self(*stream.remote_addr())
    }
}

/// Lets handlers keep taking `ConnectInfo<SocketAddr>`, which the login throttle depends on.
pub fn with_peer(router: axum::Router) -> axum::Router {
    router.layer(axum::middleware::map_request(
        |mut request: axum::extract::Request| async move {
            let peer = request
                .extensions()
                .get::<axum::extract::ConnectInfo<Peer>>()
                .map(|c| c.0.0);
            if let Some(addr) = peer {
                request
                    .extensions_mut()
                    .insert(axum::extract::ConnectInfo(addr));
            }
            request
        },
    ))
}
