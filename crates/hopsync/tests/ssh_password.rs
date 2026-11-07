//! Password login against a server that offers exactly the methods each test names.
//!
//! OpenSSH cannot check passwords without root (PAM or shadow), so `tests/support` cannot cover this.
//! An in-process russh server can. `crates/hopsync/web/e2e/password_install.py` drives the same path
//! through the daemon against a real OpenSSH in a container.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use hopsync::hostkeys::{HostId, KnownHosts};
use hopsync::ssh::{KeySource, SshError, SshOptions, SshTarget, connect};
use russh::keys::ssh_key::private::{Ed25519Keypair, KeypairData};
use russh::keys::{PrivateKey, ssh_key};
use russh::server::{Auth, Config, Handler, Response, run_stream};
use russh::{MethodKind, MethodSet};
use tokio::net::TcpListener;
use zeroize::Zeroizing;

const PASSWORD: &str = "correct horse";

/// What the keyboard-interactive method asks.
#[derive(Clone, Copy)]
enum Questions {
    /// One hidden prompt, as PAM asks for a password.
    Password,
    /// A round with no prompts first, as PAM sends for a notice, then the password.
    NoticeThenPassword,
    /// The password and then a one-time code: a password alone cannot pass this.
    PasswordAndCode,
    /// One prompt that shows what is typed, so it is not asking for a secret.
    Visible,
}

struct Server {
    /// russh calls `auth_password` even when `Config::methods` leaves it out, as OpenSSH would not.
    password_method: bool,
    questions: Questions,
    round: usize,
    /// Every answer the client sent, to prove what it did and did not reveal.
    answers: Arc<Mutex<Vec<String>>>,
}

fn prompts(list: &[(&'static str, bool)]) -> Auth {
    Auth::Partial {
        name: "".into(),
        instructions: "".into(),
        prompts: list
            .iter()
            .map(|(text, echo)| ((*text).into(), *echo))
            .collect::<Vec<_>>()
            .into(),
    }
}

impl Handler for Server {
    type Error = russh::Error;

    async fn auth_password(&mut self, _user: &str, password: &str) -> Result<Auth, Self::Error> {
        Ok(if self.password_method && password == PASSWORD {
            Auth::Accept
        } else {
            Auth::reject()
        })
    }

    async fn auth_keyboard_interactive<'a>(
        &'a mut self,
        _user: &str,
        _submethods: &str,
        response: Option<Response<'a>>,
    ) -> Result<Auth, Self::Error> {
        let answers: Vec<String> = response
            .map(|r| {
                r.map(|b| String::from_utf8_lossy(&b).into_owned())
                    .collect()
            })
            .unwrap_or_default();
        self.answers.lock().unwrap().extend(answers.iter().cloned());
        self.round += 1;
        Ok(match (self.questions, self.round) {
            (Questions::Password, 1) | (Questions::NoticeThenPassword, 2) => {
                prompts(&[("Password: ", false)])
            }
            (Questions::NoticeThenPassword, 1) => prompts(&[]),
            (Questions::PasswordAndCode, 1) => prompts(&[("Password: ", false), ("Code: ", false)]),
            (Questions::Visible, 1) => prompts(&[("Favourite colour: ", true)]),
            _ if answers.iter().any(|a| a == PASSWORD) => Auth::Accept,
            _ => Auth::reject(),
        })
    }
}

/// Starts a server offering `methods`; returns what a client needs and the answers it will record.
async fn serve(
    methods: &[MethodKind],
    questions: Questions,
) -> (
    tempfile::TempDir,
    KnownHosts,
    HostId,
    Arc<Mutex<Vec<String>>>,
) {
    let key = PrivateKey::new(
        KeypairData::Ed25519(Ed25519Keypair::from_seed(&[7; 32])),
        "",
    )
    .expect("host key");
    let host = {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let host = HostId::new("127.0.0.1", listener.local_addr().unwrap().port()).unwrap();
        let config = Arc::new(Config {
            keys: vec![key.clone()],
            methods: MethodSet::from(methods),
            auth_rejection_time: Duration::from_millis(1),
            auth_rejection_time_initial: Some(Duration::ZERO),
            ..Default::default()
        });
        let password_method = methods.contains(&MethodKind::Password);
        let answers = Arc::new(Mutex::new(Vec::new()));
        let recorded = answers.clone();
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let handler = Server {
                    password_method,
                    questions,
                    round: 0,
                    answers: recorded.clone(),
                };
                let config = config.clone();
                tokio::spawn(async move {
                    if let Ok(session) = run_stream(config, stream, handler).await {
                        let _ = session.await;
                    }
                });
            }
        });
        (host, answers)
    };
    let pins = tempfile::tempdir().expect("tempdir");
    let mut known = KnownHosts::load(&pins.path().join("known_hosts")).expect("load");
    known
        .pin(&host.0, &ssh_key::PublicKey::from(&key))
        .expect("pin");
    (pins, known, host.0, host.1)
}

async fn login(known: &KnownHosts, host: HostId, password: &str) -> Result<(), SshError> {
    let target = SshTarget {
        host,
        user: "seed".into(),
        key: KeySource::Password(Arc::new(Zeroizing::new(password.to_owned()))),
    };
    connect(&target, known, &SshOptions::default())
        .await
        .map(|_| ())
}

#[tokio::test]
async fn the_password_method_logs_in() {
    let (_d, known, host, _) = serve(&[MethodKind::Password], Questions::Password).await;
    login(&known, host, PASSWORD).await.expect("login");
}

#[tokio::test]
async fn a_server_that_only_asks_by_keyboard_interactive_logs_in() {
    let (_d, known, host, answers) =
        serve(&[MethodKind::KeyboardInteractive], Questions::Password).await;
    login(&known, host, PASSWORD).await.expect("login");
    assert_eq!(*answers.lock().unwrap(), [PASSWORD]);
}

#[tokio::test]
async fn a_round_without_prompts_is_answered_before_the_password() {
    let (_d, known, host, answers) = serve(
        &[MethodKind::KeyboardInteractive],
        Questions::NoticeThenPassword,
    )
    .await;
    login(&known, host, PASSWORD).await.expect("login");
    assert_eq!(*answers.lock().unwrap(), [PASSWORD]);
}

#[tokio::test]
async fn a_wrong_password_is_rejected_by_either_method() {
    for method in [MethodKind::Password, MethodKind::KeyboardInteractive] {
        let (_d, known, host, _) = serve(&[method], Questions::Password).await;
        let e = login(&known, host, "wrong").await.expect_err("rejected");
        assert!(matches!(e, SshError::AuthRejected { .. }), "{e}");
        assert!(e.to_string().contains("rejected the password"), "{e}");
    }
}

#[tokio::test]
async fn a_second_factor_fails_without_revealing_the_password() {
    let (_d, known, host, answers) = serve(
        &[MethodKind::KeyboardInteractive],
        Questions::PasswordAndCode,
    )
    .await;
    assert!(matches!(
        login(&known, host, PASSWORD).await,
        Err(SshError::AuthRejected { .. })
    ));
    assert!(answers.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_visible_prompt_never_receives_the_password() {
    let (_d, known, host, answers) =
        serve(&[MethodKind::KeyboardInteractive], Questions::Visible).await;
    assert!(matches!(
        login(&known, host, PASSWORD).await,
        Err(SshError::AuthRejected { .. })
    ));
    assert!(answers.lock().unwrap().is_empty());
}
