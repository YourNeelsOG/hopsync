//! `hopsync serve` (Phase 2 steps 7 and 8): the HTTP listener for browsers, the Unix socket for
//! the local CLI, and shutdown.
//!
//! - The HTTP listener binds loopback by default. Any other address gets a loud warning, because
//!   the UI is plain HTTP until Phase 4 adds TLS; put a TLS reverse proxy in front.
//! - The Unix socket lives in a directory only this user can enter, and every connection's peer
//!   uid (`SO_PEERCRED`) must be ours. Requests on it skip login: being this user already gives
//!   access to the data directory.
//! - A socket file left by a crashed daemon is replaced only when nothing answers on it.
//! - On first start the log names the file holding the setup token, never the token.

use std::net::SocketAddr;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use tokio::net::{TcpListener, UnixListener, UnixStream};

use crate::api::{self, Transport};
use crate::service::{Service, ServiceConfig};

pub struct DaemonConfig {
    pub listen: SocketAddr,
    pub socket: PathBuf,
    pub service: ServiceConfig,
    /// Set when a TLS proxy terminates HTTPS in front of the daemon.
    pub secure_cookies: bool,
}

/// `$XDG_RUNTIME_DIR/hopsync/hopsync.sock`, else `<data dir>/run/hopsync.sock`.
pub fn default_socket(data_dir: &Path) -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .map_or_else(|| data_dir.join("run"), |p| p.join("hopsync"))
        .join("hopsync.sock")
}

/// `$XDG_DATA_HOME/hopsync`, else `~/.local/share/hopsync`.
pub fn default_data_dir() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))?;
    Some(base.join("hopsync"))
}

/// Accepts only connections from processes running as our own uid.
pub struct OwnerOnly {
    inner: UnixListener,
    uid: u32,
}

impl axum::serve::Listener for OwnerOnly {
    type Io = UnixStream;
    type Addr = tokio::net::unix::SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        loop {
            match self.inner.accept().await {
                Ok((stream, addr)) => match stream.peer_cred() {
                    Ok(cred) if peer_allowed(cred.uid(), self.uid) => return (stream, addr),
                    Ok(cred) => eprintln!(
                        "hopsync: refused a socket connection from uid {}",
                        cred.uid()
                    ),
                    Err(e) => eprintln!("hopsync: socket peer check failed: {e}"),
                },
                // Out of file descriptors and the like: wait a moment instead of spinning.
                Err(_) => tokio::time::sleep(Duration::from_millis(100)).await,
            }
        }
    }

    fn local_addr(&self) -> std::io::Result<Self::Addr> {
        self.inner.local_addr()
    }
}

/// The uid rule for socket peers, separate so it can be tested without a second user.
pub fn peer_allowed(peer_uid: u32, our_uid: u32) -> bool {
    peer_uid == our_uid
}

/// Longest socket path Linux accepts (`sun_path` is 108 bytes with the terminating NUL).
pub const MAX_SOCKET_PATH: usize = 107;

/// Socket paths are short on Linux; a long data directory runs into that, so say what to do.
pub fn check_socket_path(path: &Path) -> Result<(), String> {
    let len = path.as_os_str().len();
    if len > MAX_SOCKET_PATH {
        return Err(format!(
            "{}: socket paths may have at most {MAX_SOCKET_PATH} bytes, this one has {len}; pass --socket with a shorter path",
            path.display()
        ));
    }
    Ok(())
}

/// Binds the CLI socket: a private directory, no stale socket, mode 0600.
pub async fn bind_socket(path: &Path) -> Result<OwnerOnly, String> {
    check_socket_path(path)?;
    let uid = rustix::process::geteuid().as_raw();
    let dir = path
        .parent()
        .ok_or_else(|| format!("{}: no parent directory", path.display()))?;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .map_err(|e| format!("{}: {e}", dir.display()))?;
    let meta = std::fs::symlink_metadata(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    if !meta.is_dir() || meta.uid() != uid || meta.mode() & 0o077 != 0 {
        return Err(format!(
            "{}: the socket directory must be a directory owned by this user with mode 0700",
            dir.display()
        ));
    }
    match std::fs::symlink_metadata(path) {
        Ok(_) => {
            if UnixStream::connect(path).await.is_ok() {
                return Err(format!(
                    "{}: another hopsync daemon is already running",
                    path.display()
                ));
            }
            std::fs::remove_file(path).map_err(|e| format!("{}: {e}", path.display()))?;
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(format!("{}: {e}", path.display())),
    }
    let inner = UnixListener::bind(path).map_err(|e| format!("{}: {e}", path.display()))?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(OwnerOnly { inner, uid })
}

async fn shutdown_signal() {
    let term = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut s) => {
                s.recv().await;
            }
            Err(_) => std::future::pending().await,
        }
    };
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        () = term => {}
    }
}

/// Runs the daemon until SIGINT or SIGTERM.
pub async fn run(cfg: DaemonConfig) -> Result<(), String> {
    // The service takes the data directory's lock first; nothing is bound before that.
    let svc = Service::open(cfg.service).map_err(|e| e.to_string())?;
    let sock = bind_socket(&cfg.socket).await?;
    let tcp = TcpListener::bind(cfg.listen)
        .await
        .map_err(|e| format!("listen on {}: {e}", cfg.listen))?;
    let local = tcp.local_addr().map_err(|e| e.to_string())?;
    serve(svc, tcp, sock, cfg.secure_cookies, shutdown_signal(), local).await?;
    let _ = std::fs::remove_file(&cfg.socket);
    Ok(())
}

/// Serves both listeners until `shutdown` resolves. Split from `run` for tests.
pub async fn serve(
    svc: Arc<Service>,
    tcp: TcpListener,
    sock: OwnerOnly,
    secure_cookies: bool,
    shutdown: impl Future<Output = ()> + Send + 'static,
    shown: SocketAddr,
) -> Result<(), String> {
    svc.start();
    eprintln!("hopsync: web UI and API on http://{shown}/");
    if !shown.ip().is_loopback() && !secure_cookies {
        eprintln!(
            "hopsync: WARNING: serving plain HTTP on {shown}, which is not a loopback address. \
             Passwords and session cookies cross the network unencrypted. Put a TLS reverse \
             proxy in front and pass --secure-cookies, or listen on 127.0.0.1."
        );
    }
    if svc.auth.needs_setup() {
        // The token itself never goes to the log (CLAUDE.md §3).
        eprintln!(
            "hopsync: first start: open http://{shown}/ and set the admin password with the setup token in {}",
            svc.setup_token_file().display()
        );
    }
    {
        let svc = svc.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(3600));
            loop {
                tick.tick().await;
                if let Err(e) = svc.auth.prune_sessions() {
                    eprintln!("hopsync: pruning sessions: {e}");
                }
            }
        });
    }
    let local_router = api::router(svc.clone(), Transport::Local);
    tokio::spawn(async move {
        if let Err(e) = axum::serve(sock, local_router.into_make_service()).await {
            eprintln!("hopsync: socket server stopped: {e}");
        }
    });
    let stopping = svc.clone();
    let shutdown = async move {
        shutdown.await;
        stopping.begin_shutdown();
    };
    let net = api::router(
        svc.clone(),
        Transport::Network {
            secure_cookies,
            loopback_only: shown.ip().is_loopback(),
        },
    );
    let server = axum::serve(tcp, net.into_make_service_with_connect_info::<SocketAddr>())
        .with_graceful_shutdown(shutdown)
        .into_future();
    let deadline = async {
        svc.stopping().await;
        tokio::time::sleep(SHUTDOWN_GRACE).await;
    };
    // Requests still open after the grace period (a long listing, a key install) are dropped.
    tokio::select! {
        r = server => r.map_err(|e| e.to_string()),
        () = deadline => {
            eprintln!("hopsync: stopping with requests still open");
            Ok(())
        }
    }
}

/// How long shutdown waits for open requests.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_our_uid_may_use_the_socket() {
        assert!(peer_allowed(1000, 1000));
        assert!(
            !peer_allowed(0, 1000),
            "not even root's processes by uid alone"
        );
        assert!(!peer_allowed(1001, 1000));
    }

    #[tokio::test]
    async fn a_live_socket_is_not_replaced_and_a_stale_one_is() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("run/hopsync.sock");
        let first = bind_socket(&path).await.expect("bind");
        let mode = std::fs::metadata(path.parent().expect("dir"))
            .expect("stat")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o700);
        assert!(
            bind_socket(&path).await.is_err(),
            "a running daemon keeps its socket"
        );
        drop(first);
        bind_socket(&path)
            .await
            .expect("a stale socket is replaced");
    }

    #[tokio::test]
    async fn a_too_long_socket_path_says_what_to_do() {
        let long = PathBuf::from(format!("/tmp/{}/hopsync.sock", "d".repeat(100)));
        let err = bind_socket(&long).await.err().expect("refused");
        assert!(err.contains("pass --socket"), "{err}");
        assert!(
            !Path::new(&format!("/tmp/{}", "d".repeat(100))).exists(),
            "nothing created"
        );
    }

    #[tokio::test]
    async fn a_shared_socket_directory_is_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        let run = dir.path().join("run");
        std::fs::create_dir(&run).expect("mkdir");
        std::fs::set_permissions(&run, std::fs::Permissions::from_mode(0o777)).expect("chmod");
        assert!(bind_socket(&run.join("hopsync.sock")).await.is_err());
    }
}
