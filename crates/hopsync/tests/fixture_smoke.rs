//! Proves the test sshd fixture works with the system OpenSSH client before other tests rely on it.

mod support;

use std::process::Command;

#[test]
fn system_ssh_client_reaches_fixture_with_pinned_host_key() {
    let Some(sshd) = support::TestSshd::start() else {
        return;
    };
    let out = Command::new("ssh")
        .args(sshd.ssh_args())
        .arg("echo fixture-ok")
        .output()
        .expect("run ssh");
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        "fixture-ok",
        "stderr: {}\nsshd log:\n{}",
        String::from_utf8_lossy(&out.stderr),
        sshd.log()
    );
}

#[test]
fn fixture_rejects_client_without_pinned_host_key() {
    let Some(sshd) = support::TestSshd::start() else {
        return;
    };
    std::fs::write(sshd.known_hosts(), "").expect("empty known_hosts");
    let out = Command::new("ssh")
        .args(sshd.ssh_args())
        .arg("true")
        .output()
        .expect("run ssh");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !out.status.success(),
        "ssh must refuse an unpinned host key"
    );
    assert!(
        stderr.contains("Host key verification failed"),
        "refused for the wrong reason: {stderr}"
    );
}

/// Regression: with a wide ephemeral port range the readiness probe's source port can equal the
/// target port, and connecting to a not-yet-bound port then succeeds as a TCP self-connect. The
/// fixture took that for "sshd is up" and tests raced the server's bind (connection refused/reset).
#[test]
fn readiness_probe_rejects_a_self_connected_socket() {
    use rustix::net::{AddressFamily, SocketType, bind, connect, getsockname, socket};
    let any: std::net::SocketAddr = "127.0.0.1:0".parse().expect("addr");
    let fd = socket(AddressFamily::INET, SocketType::STREAM, None).expect("socket");
    bind(&fd, &any).expect("bind");
    let own = getsockname(&fd).expect("getsockname");
    connect(&fd, &own).expect("self-connect");
    let stream = std::net::TcpStream::from(fd);
    assert_eq!(
        stream.local_addr().ok(),
        stream.peer_addr().ok(),
        "precondition: self-connected"
    );
    assert!(!support::speaks_ssh(stream));
}
