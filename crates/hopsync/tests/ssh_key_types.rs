//! RSA is off by decision (plan.md Decision log 2026-11-03). Against real OpenSSH, both directions
//! must say so and name the way out, not fail with a misleading authentication or signature error.

mod support;

use std::path::Path;
use std::process::Command;

use hopsync::hostkeys::{HostId, offered_key};
use hopsync::ssh::{KeySource, SshError, SshOptions, connect};

fn rsa_key(path: &Path) {
    let status = Command::new("ssh-keygen")
        .args(["-q", "-t", "rsa", "-b", "2048", "-N", "", "-f"])
        .arg(path)
        .status()
        .expect("ssh-keygen");
    assert!(status.success());
}

#[tokio::test]
async fn an_rsa_user_key_is_refused_with_the_way_out() {
    let Some(sshd) = support::TestSshd::start() else {
        return;
    };
    let dir = tempfile::tempdir().expect("tempdir");
    let key = dir.path().join("id_rsa");
    rsa_key(&key);
    let (_pins, known, mut target) = sshd.client();
    target.key = KeySource::File(key);

    let Err(e) = connect(&target, &known, &SshOptions::default()).await else {
        panic!("an RSA key must be refused");
    };
    assert!(matches!(e, SshError::KeyFile { .. }), "{e}");
    let text = e.to_string();
    assert!(text.contains("RSA"), "{text}");
    assert!(text.contains("ssh-keygen -t ed25519"), "{text}");
}

#[tokio::test]
async fn an_rsa_only_host_is_named_as_the_problem() {
    let dir = tempfile::tempdir().expect("tempdir");
    let key = dir.path().join("host_rsa");
    rsa_key(&key);
    let host_key = format!("HostKey {}", key.display());
    let Some(sshd) =
        support::TestSshd::start_with(&[&host_key, "HostKeyAlgorithms rsa-sha2-512,rsa-sha2-256"])
    else {
        return;
    };
    let host = HostId::new(&sshd.addr.ip().to_string(), sshd.addr.port()).expect("host id");

    let text = offered_key(&host).await.expect_err("refused").to_string();
    assert!(text.contains("RSA"), "{text}");
    assert!(text.contains("ed25519"), "{text}");
}
