//! Integration-test support: a throwaway OpenSSH server per test.
//!
//! `TestSshd::start()` runs the system `sshd` as the current user on a free loopback port, with a fresh
//! host key, a fresh client key, and a known_hosts file pinning the host key. Nothing outside the
//! temporary directory is read or changed. The server is killed when the value is dropped.

#![allow(dead_code)] // Each test binary uses a different subset.

use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use hopsync::hostkeys::{HostId, KnownHosts};
use hopsync::ssh::{KeySource, SshTarget};
use russh::keys::PublicKey;

const SSHD_CANDIDATES: &[&str] = &["/usr/sbin/sshd", "/usr/bin/sshd"];
const SFTP_SERVER_CANDIDATES: &[&str] = &[
    "/usr/lib/openssh/sftp-server",
    "/usr/libexec/openssh/sftp-server",
    "/usr/lib/ssh/sftp-server",
    "/usr/libexec/sftp-server",
];

pub struct TestSshd {
    child: Child,
    dir: tempfile::TempDir,
    pub addr: SocketAddr,
    pub user: String,
}

impl TestSshd {
    /// Starts a server, or returns None (after printing why) when OpenSSH is not installed,
    /// so tests can skip instead of fail on machines without sshd.
    pub fn start() -> Option<Self> {
        Self::start_with(&[])
    }

    /// Like `start`, with extra sshd_config lines (for example `MaxStartups 2`).
    pub fn start_with(extra_config: &[&str]) -> Option<Self> {
        let Some(sshd) = first_existing(SSHD_CANDIDATES) else {
            eprintln!("skipping: no sshd binary found in {SSHD_CANDIDATES:?}");
            return None;
        };
        let Some(sftp_server) = first_existing(SFTP_SERVER_CANDIDATES) else {
            eprintln!("skipping: no sftp-server binary found in {SFTP_SERVER_CANDIDATES:?}");
            return None;
        };
        let user = current_user();
        let dir = tempfile::tempdir().expect("create temp dir");
        let d = dir.path();
        keygen(&d.join("host_ed25519"));
        keygen(&d.join("client_ed25519"));
        std::fs::copy(d.join("client_ed25519.pub"), d.join("authorized_keys"))
            .expect("write authorized_keys");

        // The port is picked by binding :0 and releasing it; another process could grab it in between,
        // so startup is retried on a new port.
        for _ in 0..3 {
            let addr = free_loopback_port();
            let config = d.join("sshd_config");
            let mut text = format!(
                "ListenAddress {addr}\nHostKey {host}\nPidFile {pid}\nAuthorizedKeysFile {auth}\n\
                 PubkeyAuthentication yes\nPasswordAuthentication no\nKbdInteractiveAuthentication no\n\
                 UsePAM no\nStrictModes no\nLogLevel VERBOSE\nAllowUsers {user}\nSubsystem sftp {sftp}\n",
                host = d.join("host_ed25519").display(),
                pid = d.join("sshd.pid").display(),
                auth = d.join("authorized_keys").display(),
                sftp = sftp_server.display(),
            );
            for line in extra_config {
                text.push_str(line);
                text.push('\n');
            }
            std::fs::write(&config, text).expect("write sshd_config");
            let host_pub =
                std::fs::read_to_string(d.join("host_ed25519.pub")).expect("read host key");
            let host_pub: Vec<&str> = host_pub.split_whitespace().take(2).collect();
            std::fs::write(
                d.join("known_hosts"),
                format!("[{}]:{} {}\n", addr.ip(), addr.port(), host_pub.join(" ")),
            )
            .expect("write known_hosts");

            let mut child = Command::new(&sshd)
                .args(["-D", "-e", "-f"])
                .arg(&config)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::from(
                    std::fs::File::create(d.join("sshd.log")).expect("create sshd.log"),
                ))
                .spawn()
                .expect("spawn sshd");
            if wait_for_listen(addr, &mut child, Duration::from_secs(5)) {
                return Some(Self {
                    child,
                    dir,
                    addr,
                    user,
                });
            }
            let _ = child.kill();
            let _ = child.wait();
        }
        panic!(
            "sshd did not start; log:\n{}",
            std::fs::read_to_string(dir.path().join("sshd.log")).unwrap_or_default()
        );
    }

    pub fn addr(&self) -> std::net::SocketAddr {
        self.addr
    }

    pub fn dir(&self) -> &Path {
        self.dir.path()
    }

    pub fn client_key(&self) -> PathBuf {
        self.dir().join("client_ed25519")
    }

    pub fn known_hosts(&self) -> PathBuf {
        self.dir().join("known_hosts")
    }

    /// The server's log, for assertion messages.
    pub fn log(&self) -> String {
        std::fs::read_to_string(self.dir().join("sshd.log")).unwrap_or_default()
    }

    /// Sends `signal` (for example "STOP", "CONT", "KILL") to every `sftp-server` started by this
    /// sshd, to simulate a stalled or dropped SFTP session. Returns how many processes were signalled.
    pub fn signal_sftp_servers(&self, signal: &str) -> usize {
        let pids = descendants_named(self.child.id(), "sftp-server");
        for pid in &pids {
            let status = Command::new("kill")
                .arg(format!("-{signal}"))
                .arg(pid.to_string())
                .status()
                .expect("run kill");
            assert!(status.success(), "kill -{signal} {pid} failed");
        }
        pids.len()
    }

    /// Arguments for the system `ssh` client that reach this server with strict host-key checking.
    pub fn ssh_args(&self) -> Vec<String> {
        vec![
            "-F".into(),
            "none".into(),
            "-p".into(),
            self.addr.port().to_string(),
            "-i".into(),
            self.client_key().display().to_string(),
            "-o".into(),
            "IdentitiesOnly=yes".into(),
            "-o".into(),
            format!("UserKnownHostsFile={}", self.known_hosts().display()),
            "-o".into(),
            "StrictHostKeyChecking=yes".into(),
            "-o".into(),
            "BatchMode=yes".into(),
            "-l".into(),
            self.user.clone(),
            self.addr.ip().to_string(),
        ]
    }
}

impl TestSshd {
    /// A client for this server: Hopsync known_hosts pinning its host key (in the returned temp
    /// dir, which must outlive the client) and a target using the fixture's client key.
    pub fn client(&self) -> (tempfile::TempDir, KnownHosts, SshTarget) {
        self.client_via(self.addr.port())
    }

    /// Like `client`, for reaching this server through a relay on another local port: the host key
    /// is pinned for that port.
    pub fn client_via(&self, port: u16) -> (tempfile::TempDir, KnownHosts, SshTarget) {
        let host = HostId::new(&self.addr.ip().to_string(), port).expect("host id");
        let pins = tempfile::tempdir().expect("tempdir");
        let mut known = KnownHosts::load(&pins.path().join("known_hosts")).expect("load pins");
        let key = std::fs::read_to_string(self.dir().join("host_ed25519.pub")).expect("host key");
        known
            .pin(
                &host,
                &PublicKey::from_openssh(key.trim()).expect("parse host key"),
            )
            .expect("pin");
        let target = SshTarget {
            host,
            user: self.user.clone(),
            key: KeySource::File(self.client_key()),
        };
        (pins, known, target)
    }
}

impl Drop for TestSshd {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        // A failing test usually needs the server's side of the story.
        if std::thread::panicking() {
            eprintln!("sshd log for {}:\n{}", self.addr, self.log());
        }
    }
}

fn first_existing(candidates: &[&str]) -> Option<PathBuf> {
    candidates.iter().map(PathBuf::from).find(|p| p.exists())
}

fn current_user() -> String {
    let out = Command::new("id").arg("-un").output().expect("run id -un");
    String::from_utf8(out.stdout)
        .expect("utf-8 user name")
        .trim()
        .to_owned()
}

fn keygen(path: &Path) {
    let status = Command::new("ssh-keygen")
        .args(["-q", "-t", "ed25519", "-N", "", "-C", "hopsync-test", "-f"])
        .arg(path)
        .status()
        .expect("run ssh-keygen");
    assert!(status.success(), "ssh-keygen failed for {}", path.display());
}

/// PIDs of processes named `comm` whose ancestor chain includes `root`.
fn descendants_named(root: u32, comm: &str) -> Vec<u32> {
    let parent_of = |pid: u32| -> Option<u32> {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        stat.rsplit_once(") ")?
            .1
            .split_whitespace()
            .nth(1)?
            .parse()
            .ok()
    };
    let mut found = Vec::new();
    for entry in std::fs::read_dir("/proc").expect("read /proc").flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|s| s.parse::<u32>().ok())
        else {
            continue;
        };
        let name = std::fs::read_to_string(format!("/proc/{pid}/comm")).unwrap_or_default();
        if name.trim() != comm {
            continue;
        }
        let mut cur = pid;
        while let Some(parent) = parent_of(cur) {
            if parent == root {
                found.push(pid);
                break;
            }
            if parent <= 1 {
                break;
            }
            cur = parent;
        }
    }
    found
}

fn free_loopback_port() -> SocketAddr {
    TcpListener::bind("127.0.0.1:0")
        .and_then(|l| l.local_addr())
        .expect("bind a free loopback port")
}

fn wait_for_listen(addr: SocketAddr, child: &mut Child, timeout: Duration) -> bool {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if let Ok(Some(_)) = child.try_wait() {
            return false;
        }
        if let Ok(stream) = TcpStream::connect_timeout(&addr, Duration::from_millis(100))
            && speaks_ssh(stream)
        {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    false
}

/// Whether `stream` reaches an SSH server: it sends an `SSH-` banner. A connect alone proves nothing:
/// with a wide ephemeral port range the probe can self-connect to a port nobody listens on yet.
pub fn speaks_ssh(mut stream: TcpStream) -> bool {
    use std::io::Read;
    if stream.local_addr().ok() == stream.peer_addr().ok() {
        return false;
    }
    let mut banner = [0u8; 4];
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .is_ok()
        && stream.read_exact(&mut banner).is_ok()
        && &banner == b"SSH-"
}
