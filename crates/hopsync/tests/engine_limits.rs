//! Server limits (Phase 1 step 14): a server that refuses some of the lanes asked for must cost
//! speed, never the transfer.

mod support;

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use hopsync::engine::{Remote, TransferOptions, push_file, push_tree};
use hopsync::paths::{LocalRoot, RelPath, RemoteRoot};
use hopsync::plan::PlanConfig;

struct Env {
    sshd: support::TestSshd,
    _pins: tempfile::TempDir,
    remote: Remote,
    local_dir: tempfile::TempDir,
    local: LocalRoot,
}

fn env(extra: &[&str]) -> Option<Env> {
    let sshd = support::TestSshd::start_with(extra)?;
    let (pins, known, target) = sshd.client();
    Some(env_for(sshd, pins, known, target))
}

fn env_for(
    sshd: support::TestSshd,
    pins: tempfile::TempDir,
    known: hopsync::hostkeys::KnownHosts,
    target: hopsync::ssh::SshTarget,
) -> Env {
    let remote_dir = sshd.dir().join("remote");
    std::fs::create_dir(&remote_dir).expect("mkdir remote");
    let root = RemoteRoot::parse(remote_dir.to_str().expect("utf8")).expect("remote root");
    let local_dir = tempfile::tempdir().expect("tempdir");
    let local = LocalRoot::open(local_dir.path()).expect("local root");
    Env {
        remote: Remote::new(target, known, root),
        sshd,
        _pins: pins,
        local_dir,
        local,
    }
}

/// A relay in front of the server that closes every connection past `cap` open at once, as a
/// provider's firewall connection limit would. `outage` cuts every open connection and refuses new
/// ones while it is set, as a network blip would.
struct Relay {
    port: u16,
    outage: Arc<AtomicBool>,
    cut: Arc<tokio::sync::Notify>,
}

async fn relay(server: std::net::SocketAddr, cap: usize) -> Relay {
    relay_with(server, cap, Duration::ZERO).await
}

/// `relay`, holding every connection after the first for `slow` before passing it on, like a
/// distant server whose handshakes take that long.
async fn relay_with(server: std::net::SocketAddr, cap: usize, slow: Duration) -> Relay {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let open = Arc::new(tokio::sync::Semaphore::new(cap));
    let outage = Arc::new(AtomicBool::new(false));
    let cut = Arc::new(tokio::sync::Notify::new());
    let (o, c) = (outage.clone(), cut.clone());
    tokio::spawn(async move {
        let mut first = true;
        while let Ok((mut client, _)) = listener.accept().await {
            let delay = if std::mem::take(&mut first) {
                Duration::ZERO
            } else {
                slow
            };
            let permit = match open.clone().try_acquire_owned() {
                Ok(p) if !o.load(Ordering::SeqCst) => p,
                _ => continue,
            };
            let c = c.clone();
            tokio::spawn(async move {
                tokio::time::sleep(delay).await;
                if let Ok(mut upstream) = tokio::net::TcpStream::connect(server).await {
                    tokio::select! {
                        _ = tokio::io::copy_bidirectional(&mut client, &mut upstream) => {}
                        () = c.notified() => {}
                    }
                }
                drop(permit);
            });
        }
    });
    Relay { port, outage, cut }
}

impl Env {
    fn remote_path(&self, name: &str) -> PathBuf {
        self.sshd.dir().join("remote").join(name)
    }
    fn local_path(&self, name: &str) -> PathBuf {
        self.local_dir.path().join(name)
    }
}

fn rel(s: &str) -> RelPath {
    RelPath::parse(s).expect("rel")
}

/// 16 lanes against a server that drops all but 2 unauthenticated connections at a time and allows
/// one session per connection.
fn greedy() -> TransferOptions {
    TransferOptions {
        lanes: 16,
        plan: PlanConfig {
            lanes: 16,
            min_chunk: 1 << 20,
            max_chunk: 1 << 20,
        },
        ..TransferOptions::default()
    }
}

const LIMITS: &[&str] = &["MaxStartups 2", "MaxSessions 1"];

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn refused_lanes_cost_speed_not_the_transfer() {
    let Some(e) = env(LIMITS) else { return };
    let data: Vec<u8> = (0..32 << 20).map(|i: u32| (i % 253) as u8).collect();
    std::fs::write(e.local_path("big"), &data).expect("write");
    let r = push_file(&e.local, &rel("big"), &e.remote, &rel("big"), &greedy())
        .await
        .unwrap_or_else(|err| panic!("push failed: {err}\n{}", e.sshd.log()));
    assert_eq!(r.bytes, data.len() as u64);
    // Pacing the handshakes keeps every lane under MaxStartups: sshd never had to drop one
    // ("drop connection #N ... MaxStartups" in its log).
    assert_eq!(r.retries, 0);
    let log = e.sshd.log();
    assert!(!log.contains("MaxStartups"), "{log}");
    assert_eq!(std::fs::read(e.remote_path("big")).expect("read"), data);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn refused_lanes_do_not_fail_a_tree() {
    let Some(e) = env(LIMITS) else { return };
    std::fs::create_dir(e.local_path("t")).expect("mkdir");
    for i in 0..200 {
        std::fs::write(e.local_path(&format!("t/f{i}")), format!("file {i}")).expect("write");
    }
    let r = push_tree(&e.local, &rel("t"), &e.remote, &rel("t"), &greedy())
        .await
        .unwrap_or_else(|err| panic!("push failed: {err}\n{}", e.sshd.log()));
    assert!(r.problems.is_empty(), "{:?}", r.problems);
    assert_eq!(r.files, 200);
    for i in [0, 99, 199] {
        assert_eq!(
            std::fs::read_to_string(e.remote_path(&format!("t/f{i}"))).expect("read"),
            format!("file {i}")
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_connection_cap_retires_lanes_without_spending_retries() {
    let Some(sshd) = support::TestSshd::start() else {
        return;
    };
    let relay = relay(sshd.addr(), 3).await;
    let (pins, known, target) = sshd.client_via(relay.port);
    let e = env_for(sshd, pins, known, target);
    let data: Vec<u8> = (0..32 << 20).map(|i: u32| (i % 241) as u8).collect();
    std::fs::write(e.local_path("big"), &data).expect("write");
    let o = TransferOptions {
        retries: 0,
        ..greedy()
    };
    let r = push_file(&e.local, &rel("big"), &e.remote, &rel("big"), &o)
        .await
        .unwrap_or_else(|err| panic!("push failed: {err}"));
    // Lane 0 plus whatever the cap leaves: never more than 3.
    assert!((1..=3).contains(&r.lanes), "lanes {}", r.lanes);
    assert_eq!(r.retries, 0);
    assert_eq!(std::fs::read(e.remote_path("big")).expect("read"), data);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_blip_that_drops_every_lane_keeps_them_all() {
    let Some(sshd) = support::TestSshd::start() else {
        return;
    };
    let relay = relay(sshd.addr(), 100).await;
    let (pins, known, target) = sshd.client_via(relay.port);
    let e = env_for(sshd, pins, known, target);
    let data: Vec<u8> = (0..64 << 20).map(|i: u32| (i % 239) as u8).collect();
    std::fs::write(e.local_path("big"), &data).expect("write");
    let moved = Arc::new(AtomicU64::new(0));
    let o = TransferOptions {
        lanes: 4,
        retries: 30,
        plan: PlanConfig {
            lanes: 4,
            min_chunk: 1 << 20,
            max_chunk: 1 << 20,
        },
        progress: Some(Arc::new({
            let (moved, outage, cut) = (moved.clone(), relay.outage.clone(), relay.cut.clone());
            move |n| {
                let before = moved.fetch_add(n, Ordering::SeqCst);
                if before < 16 << 20 && before + n >= 16 << 20 {
                    // Every lane is lost and, for half a second, none can come back.
                    outage.store(true, Ordering::SeqCst);
                    cut.notify_waiters();
                    let outage = outage.clone();
                    tokio::spawn(async move {
                        tokio::time::sleep(Duration::from_millis(500)).await;
                        outage.store(false, Ordering::SeqCst);
                    });
                }
            }
        })),
        ..TransferOptions::default()
    };
    let r = tokio::time::timeout(
        Duration::from_secs(60),
        push_file(&e.local, &rel("big"), &e.remote, &rel("big"), &o),
    )
    .await
    .expect("must not hang")
    .unwrap_or_else(|err| panic!("push failed: {err}"));
    assert!(r.retries > 0, "the outage must have been noticed");
    assert_eq!(r.lanes, 4);
    assert_eq!(std::fs::read(e.remote_path("big")).expect("read"), data);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn small_work_does_not_wait_for_slow_handshakes() {
    let Some(sshd) = support::TestSshd::start() else {
        return;
    };
    let relay = relay_with(sshd.addr(), 100, Duration::from_secs(3)).await;
    let (pins, known, target) = sshd.client_via(relay.port);
    let e = env_for(sshd, pins, known, target);
    std::fs::create_dir(e.local_path("t")).expect("mkdir");
    for i in 0..20 {
        std::fs::write(e.local_path(&format!("t/f{i}")), format!("file {i}")).expect("write");
    }
    let o = TransferOptions {
        lanes: 4,
        ..TransferOptions::default()
    };
    let start = std::time::Instant::now();
    let r = push_tree(&e.local, &rel("t"), &e.remote, &rel("t"), &o)
        .await
        .expect("push");
    let took = start.elapsed();
    assert_eq!(r.files, 20);
    // Lane 0 does it all in well under a second; lanes 1 to 3 are still in their handshake and
    // are dropped, not waited for.
    assert!(took < Duration::from_secs(2), "took {took:?}");
    assert_eq!(r.lanes, 1);
}

/// A relay sending the first connection to `first`, throttled to about 10 MB/s so the transfer
/// stays busy, and every later one to `rest`.
async fn split_relay(first: std::net::SocketAddr, rest: std::net::SocketAddr) -> u16 {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let port = listener.local_addr().expect("addr").port();
    tokio::spawn(async move {
        let mut n = 0;
        while let Ok((mut client, _)) = listener.accept().await {
            n += 1;
            let throttle = n == 1;
            let to = if throttle { first } else { rest };
            tokio::spawn(async move {
                let Ok(upstream) = tokio::net::TcpStream::connect(to).await else {
                    return;
                };
                if !throttle {
                    let mut upstream = upstream;
                    let _ = tokio::io::copy_bidirectional(&mut client, &mut upstream).await;
                    return;
                }
                let (mut cr, mut cw) = client.into_split();
                let (mut ur, mut uw) = upstream.into_split();
                let up = async move {
                    let mut buf = vec![0u8; 64 << 10];
                    while let Ok(n) = cr.read(&mut buf).await {
                        if n == 0 || uw.write_all(&buf[..n]).await.is_err() {
                            break;
                        }
                        tokio::time::sleep(Duration::from_millis(6)).await;
                    }
                };
                let down = async move {
                    let _ = tokio::io::copy(&mut ur, &mut cw).await;
                };
                tokio::join!(up, down);
            });
        }
    });
    port
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_changed_host_key_on_a_later_lane_is_fatal_not_retired() {
    let (Some(real), Some(impostor)) = (support::TestSshd::start(), support::TestSshd::start())
    else {
        return;
    };
    let port = split_relay(real.addr(), impostor.addr()).await;
    let (pins, known, target) = real.client_via(port);
    let e = env_for(real, pins, known, target);
    std::fs::write(e.local_path("big"), vec![7u8; 32 << 20]).expect("write");
    let o = TransferOptions {
        lanes: 4,
        plan: PlanConfig {
            lanes: 4,
            min_chunk: 1 << 20,
            max_chunk: 1 << 20,
        },
        ..TransferOptions::default()
    };
    let err = tokio::time::timeout(
        Duration::from_secs(60),
        push_file(&e.local, &rel("big"), &e.remote, &rel("big"), &o),
    )
    .await
    .expect("must not hang")
    .expect_err("a lane met a different host key");
    assert!(
        matches!(
            err,
            hopsync::engine::TransferError::Remote(hopsync::sftp::SftpError::Ssh(
                hopsync::ssh::SshError::HostKeyMismatch { .. }
            ))
        ),
        "got {err}"
    );
    assert!(!e.remote_path("big").exists());
    drop(impostor);
}
