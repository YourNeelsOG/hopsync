//! SSH connect and SFTP session against a real OpenSSH server (Phase 1 step 5).
//!
//! The fixture's remote filesystem is this machine's, so results are checked by reading the files
//! directly from disk.

mod support;

use std::time::Duration;

use hopsync::hostkeys::{HostId, KnownHosts};
use hopsync::sftp::{RemoteKind, RemoteOpenMode, Sftp, SftpError, SftpOptions};
use hopsync::ssh::{KeySource, SshError, SshOptions, SshTarget, connect};
use russh::keys::PublicKey;

const OTHER_KEY: &str =
    "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIEG8meRvoV7EyXJhuHGtXcGoPbVudsgcnhBv8fpUxVzM";

struct Setup {
    sshd: support::TestSshd,
    _pins_dir: tempfile::TempDir,
    known: KnownHosts,
    target: SshTarget,
}

fn setup() -> Option<Setup> {
    let sshd = support::TestSshd::start()?;
    let host = HostId::new(&sshd.addr.ip().to_string(), sshd.addr.port()).expect("host id");
    let pins_dir = tempfile::tempdir().expect("tempdir");
    let mut known = KnownHosts::load(&pins_dir.path().join("known_hosts")).expect("load");
    let host_key = std::fs::read_to_string(sshd.dir().join("host_ed25519.pub")).expect("host key");
    known
        .pin(
            &host,
            &PublicKey::from_openssh(host_key.trim()).expect("parse"),
        )
        .expect("pin");
    let target = SshTarget {
        host,
        user: sshd.user.clone(),
        key: KeySource::File(sshd.client_key()),
    };
    Some(Setup {
        sshd,
        _pins_dir: pins_dir,
        known,
        target,
    })
}

async fn session(s: &Setup, opts: SftpOptions) -> Sftp {
    let conn = connect(&s.target, &s.known, &SshOptions::default())
        .await
        .expect("connect");
    Sftp::start(&conn, opts).await.expect("start sftp")
}

fn pattern(len: usize, seed: u8) -> Vec<u8> {
    (0..len)
        .map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed))
        .collect()
}

#[tokio::test]
async fn refuses_unknown_host_without_connecting() {
    let Some(s) = setup() else { return };
    let empty = KnownHosts::load(&s._pins_dir.path().join("absent")).expect("load");
    let err = connect(&s.target, &empty, &SshOptions::default())
        .await
        .expect_err("unknown host");
    assert!(matches!(err, SshError::UnknownHost { .. }), "got {err}");
}

#[tokio::test]
async fn refuses_changed_host_key() {
    let Some(s) = setup() else { return };
    let dir = tempfile::tempdir().expect("tempdir");
    let mut wrong = KnownHosts::load(&dir.path().join("known_hosts")).expect("load");
    wrong
        .pin(
            &s.target.host,
            &PublicKey::from_openssh(OTHER_KEY).expect("key"),
        )
        .expect("pin");
    let err = connect(&s.target, &wrong, &SshOptions::default())
        .await
        .expect_err("mismatch");
    assert!(matches!(err, SshError::HostKeyMismatch { .. }), "got {err}");
}

#[tokio::test]
async fn rejected_user_key_is_an_auth_error() {
    let Some(s) = setup() else { return };
    let other = tempfile::tempdir().expect("tempdir");
    let key = other.path().join("k");
    let status = std::process::Command::new("ssh-keygen")
        .args(["-q", "-t", "ed25519", "-N", "", "-f"])
        .arg(&key)
        .status()
        .expect("ssh-keygen");
    assert!(status.success());
    let target = SshTarget {
        key: KeySource::File(key),
        ..s.target.clone()
    };
    let err = connect(&target, &s.known, &SshOptions::default())
        .await
        .expect_err("bad key");
    assert!(matches!(err, SshError::AuthRejected { .. }), "got {err}");
}

#[tokio::test]
async fn reports_server_limits() {
    let Some(s) = setup() else { return };
    let sftp = session(&s, SftpOptions::default()).await;
    assert!(
        sftp.max_write() >= 32 * 1024,
        "max_write {}",
        sftp.max_write()
    );
    assert!(sftp.max_read() >= 32 * 1024, "max_read {}", sftp.max_read());
}

#[tokio::test]
async fn oversized_write_and_read_are_split_to_server_limits() {
    let Some(s) = setup() else { return };
    let sftp = session(&s, SftpOptions::default()).await;
    let path = s.sshd.dir().join("big.bin");
    let remote = path.to_str().expect("utf-8 path");
    // 1 MiB + 7 bytes at an odd offset: several max_write pieces plus a short tail. Sent unsplit,
    // OpenSSH's sftp-server drops the connection, so a pass proves the split.
    let data = pattern((1 << 20) + 7, 3);

    let f = sftp
        .open(remote, RemoteOpenMode::WriteCreate)
        .await
        .expect("open for write");
    f.write_at(3, &data).await.expect("write");
    f.close().await.expect("close");
    let on_disk = std::fs::read(&path).expect("read back");
    assert_eq!(
        &on_disk[..3],
        &[0, 0, 0],
        "gap before the offset reads as zeros"
    );
    assert!(on_disk[3..] == data[..], "content mismatch");

    let f = sftp
        .open(remote, RemoteOpenMode::Read)
        .await
        .expect("open for read");
    let mut buf = vec![0u8; data.len()];
    assert_eq!(f.read_at(3, &mut buf).await.expect("read"), data.len());
    assert!(buf == data, "read mismatch");
}

#[tokio::test]
async fn read_stops_at_end_of_file() {
    let Some(s) = setup() else { return };
    let sftp = session(&s, SftpOptions::default()).await;
    let path = s.sshd.dir().join("small.bin");
    std::fs::write(&path, b"0123456789").expect("seed");
    let f = sftp
        .open(path.to_str().expect("utf-8"), RemoteOpenMode::Read)
        .await
        .expect("open");
    let mut buf = [0u8; 64];
    assert_eq!(f.read_at(4, &mut buf).await.expect("short read"), 6);
    assert_eq!(&buf[..6], b"456789");
    assert_eq!(f.read_at(10, &mut buf).await.expect("at EOF"), 0);
    assert_eq!(f.read_at(1000, &mut buf).await.expect("past EOF"), 0);
}

#[tokio::test]
async fn concurrent_writes_at_disjoint_offsets() {
    let Some(s) = setup() else { return };
    let sftp = session(
        &s,
        SftpOptions {
            inflight: 4,
            ..SftpOptions::default()
        },
    )
    .await;
    let path = s.sshd.dir().join("concurrent.bin");
    let f = std::sync::Arc::new(
        sftp.open(path.to_str().expect("utf-8"), RemoteOpenMode::WriteCreate)
            .await
            .expect("open"),
    );
    const CHUNK: usize = 100 * 1024;
    let mut tasks = tokio::task::JoinSet::new();
    for i in 0..32u8 {
        let f = f.clone();
        tasks.spawn(async move {
            f.write_at(i as u64 * CHUNK as u64, &pattern(CHUNK, i))
                .await
        });
    }
    while let Some(r) = tasks.join_next().await {
        r.expect("task").expect("write");
    }
    let on_disk = std::fs::read(&path).expect("read back");
    assert_eq!(on_disk.len(), 32 * CHUNK);
    for i in 0..32u8 {
        let at = i as usize * CHUNK;
        assert!(
            on_disk[at..at + CHUNK] == pattern(CHUNK, i)[..],
            "chunk {i} mismatch"
        );
    }
}

#[tokio::test]
async fn fsync_is_supported_by_openssh() {
    let Some(s) = setup() else { return };
    let sftp = session(&s, SftpOptions::default()).await;
    let path = s.sshd.dir().join("synced.bin");
    let f = sftp
        .open(path.to_str().expect("utf-8"), RemoteOpenMode::WriteCreate)
        .await
        .expect("open");
    f.write_at(0, b"durable").await.expect("write");
    f.fsync().await.expect("fsync@openssh.com");
}

#[tokio::test]
async fn stalled_server_times_out() {
    let Some(s) = setup() else { return };
    let opts = SftpOptions {
        request_timeout: Duration::from_secs(1),
        ..SftpOptions::default()
    };
    let sftp = session(&s, opts).await;
    let path = s.sshd.dir().join("stall.bin");
    std::fs::write(&path, b"x").expect("seed");
    let f = sftp
        .open(path.to_str().expect("utf-8"), RemoteOpenMode::Read)
        .await
        .expect("open");
    assert_eq!(s.sshd.signal_sftp_servers("STOP"), 1);
    let mut buf = [0u8; 1];
    let result = tokio::time::timeout(Duration::from_secs(10), f.read_at(0, &mut buf)).await;
    s.sshd.signal_sftp_servers("CONT");
    let err = result
        .expect("must not hang past the request timeout")
        .expect_err("stalled");
    assert!(matches!(err, SftpError::Timeout), "got {err}");
}

#[tokio::test]
async fn killed_server_is_connection_lost() {
    let Some(s) = setup() else { return };
    let sftp = session(&s, SftpOptions::default()).await;
    let path = s.sshd.dir().join("dropped.bin");
    let f = sftp
        .open(path.to_str().expect("utf-8"), RemoteOpenMode::WriteCreate)
        .await
        .expect("open");
    assert_eq!(s.sshd.signal_sftp_servers("KILL"), 1);
    let result = tokio::time::timeout(Duration::from_secs(10), async {
        // The first write may race the kill; keep writing until the loss is seen.
        loop {
            if let Err(e) = f.write_at(0, &pattern(64 * 1024, 1)).await {
                return e;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await;
    let err = result.expect("must not hang after the server dies");
    assert!(matches!(err, SftpError::ConnectionLost(_)), "got {err}");
}

#[tokio::test]
async fn missing_remote_file_is_a_status_error() {
    let Some(s) = setup() else { return };
    let sftp = session(&s, SftpOptions::default()).await;
    let path = s.sshd.dir().join("does-not-exist");
    let err = sftp
        .open(path.to_str().expect("utf-8"), RemoteOpenMode::Read)
        .await
        .err()
        .expect("missing file");
    assert!(matches!(err, SftpError::NotFound(_)), "got {err}");
}

#[tokio::test]
async fn lstat_reports_kind_size_and_mtime() {
    let Some(s) = setup() else { return };
    let sftp = session(&s, SftpOptions::default()).await;
    let dir = s.sshd.dir().join("meta");
    std::fs::create_dir(&dir).expect("mkdir");
    std::fs::write(dir.join("f"), b"12345").expect("write");
    std::os::unix::fs::symlink("f", dir.join("link")).expect("symlink");
    let path = |n: &str| dir.join(n).to_str().expect("utf8").to_owned();

    let f = sftp.lstat(&path("f")).await.expect("lstat file");
    assert_eq!((f.kind, f.size), (RemoteKind::File, 5));
    let want = std::fs::metadata(dir.join("f"))
        .expect("meta")
        .modified()
        .expect("mtime");
    let want = want
        .duration_since(std::time::UNIX_EPOCH)
        .expect("epoch")
        .as_secs();
    assert_eq!(f.mtime, Some(want));
    assert_eq!(
        sftp.lstat(&path("link")).await.expect("lstat link").kind,
        RemoteKind::Symlink
    );
    assert_eq!(
        sftp.lstat(&path(".")).await.expect("lstat dir").kind,
        RemoteKind::Dir
    );
    assert!(matches!(
        sftp.lstat(&path("nope")).await,
        Err(SftpError::NotFound(_))
    ));
}

#[tokio::test]
async fn set_mtime_and_set_len() {
    let Some(s) = setup() else { return };
    let sftp = session(&s, SftpOptions::default()).await;
    let local = s.sshd.dir().join("sized");
    std::fs::write(&local, vec![7u8; 100]).expect("write");
    let path = local.to_str().expect("utf8");

    sftp.set_mtime(path, 1_000_000_000)
        .await
        .expect("set mtime");
    assert_eq!(
        sftp.lstat(path).await.expect("lstat").mtime,
        Some(1_000_000_000)
    );

    let file = sftp
        .open(path, RemoteOpenMode::WriteCreate)
        .await
        .expect("open");
    file.set_len(40).await.expect("shrink");
    file.close().await.expect("close");
    assert_eq!(std::fs::read(&local).expect("read"), vec![7u8; 40]);
}

#[tokio::test]
async fn rename_replace_and_no_replace() {
    let Some(s) = setup() else { return };
    let sftp = session(&s, SftpOptions::default()).await;
    let d = s.sshd.dir();
    let p = |n: &str| d.join(n).to_str().expect("utf8").to_owned();
    std::fs::write(d.join("a"), b"new").expect("write a");
    std::fs::write(d.join("b"), b"old").expect("write b");

    let err = sftp
        .rename(&p("a"), &p("b"), false)
        .await
        .expect_err("target exists");
    assert!(!matches!(err, SftpError::ConnectionLost(_)), "got {err}");
    assert_eq!(
        std::fs::read(d.join("b")).expect("read"),
        b"old",
        "untouched"
    );

    sftp.rename(&p("a"), &p("b"), true).await.expect("replace");
    assert_eq!(std::fs::read(d.join("b")).expect("read"), b"new");
    assert!(!d.join("a").exists());

    sftp.rename(&p("b"), &p("c"), false)
        .await
        .expect("no target");
    assert_eq!(std::fs::read(d.join("c")).expect("read"), b"new");

    sftp.remove(&p("c")).await.expect("remove");
    assert!(!d.join("c").exists());
    assert!(matches!(
        sftp.remove(&p("c")).await,
        Err(SftpError::NotFound(_))
    ));
}

#[tokio::test]
async fn read_dir_and_mkdir() {
    let Some(s) = setup() else { return };
    let sftp = session(&s, SftpOptions::default()).await;
    let d = s.sshd.dir().join("listing");
    std::fs::create_dir_all(d.join("sub")).expect("mkdir");
    std::fs::write(d.join("a b"), b"12").expect("write");
    std::os::unix::fs::symlink("a b", d.join("ln")).expect("symlink");
    let p = |n: &str| d.join(n).to_str().expect("utf8").to_owned();

    let mut got: Vec<(String, RemoteKind, u64)> = sftp
        .read_dir(&p(""))
        .await
        .expect("read_dir")
        .into_iter()
        .map(|(n, a)| {
            (
                n,
                a.kind,
                if a.kind == RemoteKind::File {
                    a.size
                } else {
                    0
                },
            )
        })
        .collect();
    got.sort_by(|a, b| a.0.cmp(&b.0));
    assert_eq!(
        got,
        vec![
            ("a b".into(), RemoteKind::File, 2),
            ("ln".into(), RemoteKind::Symlink, 0),
            ("sub".into(), RemoteKind::Dir, 0),
        ]
    );
    assert!(sftp.mkdir(&p("new")).await.expect("mkdir"));
    assert!(!sftp.mkdir(&p("new")).await.expect("exists"));
    assert!(sftp.mkdir(&p("a b")).await.is_err(), "file in the way");
    assert!(matches!(
        sftp.read_dir(&p("missing")).await,
        Err(SftpError::NotFound(_))
    ));
}
