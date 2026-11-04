//! Host-key trust flow against a real OpenSSH server: nothing is pinned without explicit confirmation,
//! and a changed key is a hard error that never reaches the confirmation step.

mod support;

use hopsync::hostkeys::{HostId, HostKeyError, KnownHosts, TrustOutcome, fingerprint, trust};
use russh::keys::PublicKey;

const OTHER_KEY: &str =
    "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIEG8meRvoV7EyXJhuHGtXcGoPbVudsgcnhBv8fpUxVzM";

fn fixture_host_key(sshd: &support::TestSshd) -> PublicKey {
    let text = std::fs::read_to_string(sshd.dir().join("host_ed25519.pub")).expect("read host key");
    PublicKey::from_openssh(text.trim()).expect("parse host key")
}

#[tokio::test]
async fn unknown_host_is_pinned_only_after_confirmation() {
    let Some(sshd) = support::TestSshd::start() else {
        return;
    };
    let host = HostId::new(&sshd.addr.ip().to_string(), sshd.addr.port()).expect("host id");
    let want_fp = fingerprint(&fixture_host_key(&sshd));
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("known_hosts");
    let mut kh = KnownHosts::load(&path).expect("load");

    let mut shown = None;
    let outcome = trust(&mut kh, &host, |_, fp, _| {
        shown = Some(fp.to_owned());
        false
    })
    .await
    .expect("trust");
    assert_eq!(outcome, TrustOutcome::Declined);
    assert_eq!(
        shown.as_deref(),
        Some(want_fp.as_str()),
        "the user must see the real fingerprint"
    );
    assert!(!path.exists(), "declining must not write anything");

    let outcome = trust(&mut kh, &host, |_, _, _| true).await.expect("trust");
    assert_eq!(outcome, TrustOutcome::Pinned(want_fp.clone()));
    let reloaded = KnownHosts::load(&path).expect("reload");
    assert_eq!(
        reloaded.check(&host, &fixture_host_key(&sshd)),
        hopsync::hostkeys::Verdict::Trusted
    );

    let mut kh = reloaded;
    let outcome = trust(&mut kh, &host, |_, _, _| {
        panic!("must not ask again for a pinned key")
    })
    .await
    .expect("trust");
    assert_eq!(outcome, TrustOutcome::AlreadyTrusted);
}

#[tokio::test]
async fn changed_host_key_is_a_hard_error_without_prompt() {
    let Some(sshd) = support::TestSshd::start() else {
        return;
    };
    let host = HostId::new(&sshd.addr.ip().to_string(), sshd.addr.port()).expect("host id");
    let dir = tempfile::tempdir().expect("tempdir");
    let mut kh = KnownHosts::load(&dir.path().join("known_hosts")).expect("load");
    kh.pin(&host, &PublicKey::from_openssh(OTHER_KEY).expect("key"))
        .expect("pin other key");

    let err = trust(&mut kh, &host, |_, _, _| {
        panic!("a mismatch must never be offered for re-pinning")
    })
    .await
    .expect_err("mismatch must fail");
    match err {
        HostKeyError::Mismatch {
            offered, pinned, ..
        } => {
            assert_eq!(offered, fingerprint(&fixture_host_key(&sshd)));
            assert_eq!(
                pinned,
                vec![fingerprint(
                    &PublicKey::from_openssh(OTHER_KEY).expect("key")
                )]
            );
        }
        other => panic!("expected Mismatch, got {other}"),
    }
}

#[tokio::test]
async fn unreachable_host_is_a_connect_error() {
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .and_then(|l| l.local_addr())
        .expect("free port")
        .port();
    let host = HostId::new("127.0.0.1", port).expect("host id");
    let dir = tempfile::tempdir().expect("tempdir");
    let mut kh = KnownHosts::load(&dir.path().join("known_hosts")).expect("load");
    let err = trust(&mut kh, &host, |_, _, _| panic!("no key to confirm"))
        .await
        .expect_err("no server");
    assert!(matches!(err, HostKeyError::Connect(_)), "got {err}");
}
