//! The daemon core against a real OpenSSH server (Phase 2 steps 5 and 6): servers, trust,
//! browsing, generated keys, and the job queue with pause, resume and restart.

mod support;

use std::sync::Arc;
use std::time::Duration;

use hopsync::service::{KeyStatus, Kind, Service, ServiceConfig, ServiceError};
use hopsync::ssh::SshOptions;
use hopsync::store::{Direction, Job, JobSpec, JobState, ServerSpec};

struct Env {
    sshd: support::TestSshd,
    data: tempfile::TempDir,
    local: tempfile::TempDir,
    user: String,
}

fn env() -> Option<Env> {
    let sshd = support::TestSshd::start()?;
    std::fs::create_dir(sshd.dir().join("remote")).expect("mkdir remote");
    let (_pins, _known, target) = sshd.client();
    Some(Env {
        user: target.user,
        sshd,
        data: tempfile::tempdir().expect("tempdir"),
        local: tempfile::tempdir().expect("tempdir"),
    })
}

impl Env {
    fn open(&self) -> Arc<Service> {
        let svc = Service::open(ServiceConfig {
            data_dir: self.data.path().join("data"),
            local_root: self.local.path().to_owned(),
            concurrency: 1,
            ssh: SshOptions::default(),
        })
        .expect("open service");
        svc.start();
        svc
    }

    fn spec(&self, name: &str) -> ServerSpec {
        ServerSpec {
            name: name.into(),
            host: self.sshd.addr().ip().to_string(),
            port: self.sshd.addr().port(),
            user: self.user.clone(),
            root: self.remote_dir().to_str().expect("utf8").into(),
            key_file: Some(self.sshd.client_key().to_str().expect("utf8").into()),
            min_free: 0,
        }
    }

    fn remote_dir(&self) -> std::path::PathBuf {
        self.sshd.dir().join("remote")
    }
}

fn push(server: i64, src: &str, dst: &str) -> JobSpec {
    JobSpec {
        direction: Direction::Push,
        server,
        src: src.into(),
        dst: dst.into(),
        existing: "skip".into(),
        verify: false,
        lanes: Some(2),
    }
}

async fn wait_for(svc: &Service, id: i64, done: impl Fn(&Job) -> bool) -> Job {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let job = svc.store.job(id).expect("job");
        if done(&job) {
            return job;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "job {id} stuck in {:?}",
            job.state
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn trusted(svc: &Service, e: &Env, name: &str) -> i64 {
    let s = svc.add_server(&e.spec(name), false).expect("add");
    let probe = svc.probe(s.id).await.expect("probe");
    if probe.status == KeyStatus::Unknown {
        svc.trust(s.id, &probe.fingerprint, false)
            .await
            .expect("trust");
    }
    s.id
}

#[tokio::test(flavor = "multi_thread")]
async fn a_server_is_usable_only_after_its_key_is_confirmed() {
    let Some(e) = env() else { return };
    let svc = e.open();
    let s = svc.add_server(&e.spec("box"), false).expect("add");
    // Not trusted yet: the connection is refused before authentication.
    assert!(matches!(
        svc.list_remote(s.id, "").await,
        Err(ServiceError::Transfer(_))
    ));
    let probe = svc.probe(s.id).await.expect("probe");
    assert_eq!(probe.status, KeyStatus::Unknown);
    assert!(probe.fingerprint.starts_with("SHA256:"));
    assert!(matches!(
        svc.trust(s.id, "SHA256:not-the-key", false).await,
        Err(ServiceError::Conflict(_))
    ));
    svc.trust(s.id, &probe.fingerprint, false)
        .await
        .expect("trust");
    assert_eq!(
        svc.probe(s.id).await.expect("probe").status,
        KeyStatus::Trusted
    );

    std::fs::create_dir(e.remote_dir().join("dir")).expect("mkdir");
    std::fs::write(e.remote_dir().join("file"), b"12345").expect("write");
    std::fs::write(e.remote_dir().join("bad\nname"), b"x").expect("write");
    let listed = svc.list_remote(s.id, "").await.expect("list");
    let summary: Vec<(String, Kind, u64, bool)> = listed
        .iter()
        .map(|en| (en.name.clone(), en.kind, en.size, en.valid))
        .collect();
    assert!(
        listed
            .iter()
            .any(|en| en.name == "dir" && en.kind == Kind::Dir && en.valid)
    );
    assert!(summary.contains(&("file".into(), Kind::File, 5, true)));
    assert!(
        listed.iter().any(|en| en.name == "bad\\nname" && !en.valid),
        "odd names are escaped and marked: {summary:?}"
    );
    assert!(matches!(
        svc.list_remote(s.id, "../etc").await,
        Err(ServiceError::BadRequest(_))
    ));
}

#[tokio::test(flavor = "multi_thread")]
async fn jobs_run_in_order_and_record_every_file() {
    let Some(e) = env() else { return };
    let svc = e.open();
    let id = trusted(&svc, &e, "box").await;
    std::fs::create_dir_all(e.local.path().join("show/s01")).expect("mkdir");
    std::fs::write(e.local.path().join("show/s01/e1.mkv"), vec![1u8; 4000]).expect("write");
    std::fs::write(e.local.path().join("show/s01/e2.mkv"), vec![2u8; 5000]).expect("write");
    std::fs::write(e.local.path().join("one.bin"), vec![3u8; 100]).expect("write");

    let plan = svc
        .dry_run(&push(id, "show", "show"))
        .await
        .expect("dry run");
    assert_eq!((plan.files, plan.bytes, plan.problems), (2, 9000, 0));
    assert!(plan.items.iter().all(|i| i.outcome == "create"));
    assert!(
        !e.remote_dir().join("show").exists(),
        "a dry run writes nothing"
    );

    let a = svc.add_job(&push(id, "show", "show")).expect("queue a");
    let b = svc
        .add_job(&push(id, "one.bin", "one.bin"))
        .expect("queue b");
    assert!(matches!(
        svc.add_job(&push(id, "one.bin", "one.bin")),
        Err(ServiceError::Conflict(_))
    ));
    let a = wait_for(&svc, a.id, |j| j.state.is_final()).await;
    let b = wait_for(&svc, b.id, |j| j.state.is_final()).await;
    assert_eq!((a.state, a.files, a.bytes), (JobState::Done, 2, 9000));
    assert_eq!(b.state, JobState::Done);
    assert!(a.finished <= b.started, "one at a time, in queue order");
    assert_eq!(
        std::fs::read(e.remote_dir().join("show/s01/e2.mkv")).expect("read"),
        vec![2u8; 5000]
    );
    let mut items = svc.store.job_items(a.id, 0, 100).expect("items");
    items.sort_by(|x, y| x.path.cmp(&y.path));
    let summary: Vec<(&str, &str, u64)> = items
        .iter()
        .map(|i| (i.path.as_str(), i.outcome.as_str(), i.bytes))
        .collect();
    assert_eq!(
        summary,
        [("s01/e1.mkv", "sent", 4000), ("s01/e2.mkv", "sent", 5000)]
    );

    // A second run skips what is there (default policy) and says so per file.
    let again = svc.add_job(&push(id, "show", "show")).expect("queue again");
    let again = wait_for(&svc, again.id, |j| j.state.is_final()).await;
    assert_eq!(
        (again.state, again.files, again.problems),
        (JobState::Done, 0, 2)
    );
    let skipped = svc.store.job_items(again.id, 0, 10).expect("items");
    assert!(skipped.iter().all(|i| i.outcome == "skipped"));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_paused_job_resumes_and_a_restart_requeues_running_jobs() {
    let Some(e) = env() else { return };
    let size = 64usize << 20;
    let data: Vec<u8> = (0..size).map(|i| (i % 251) as u8).collect();
    std::fs::write(e.local.path().join("big"), &data).expect("write");
    {
        let svc = e.open();
        let id = trusted(&svc, &e, "box").await;
        svc.pause_queue();
        let job = svc.add_job(&push(id, "big", "big")).expect("queue");
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(
            svc.store.job(job.id).expect("job").state,
            JobState::Queued,
            "a paused queue starts nothing"
        );
        svc.resume_queue();
        wait_for(&svc, job.id, |j| j.state == JobState::Running).await;
        svc.pause_job(job.id).expect("pause");
        let paused = wait_for(&svc, job.id, |j| j.state != JobState::Running).await;
        assert_eq!(paused.state, JobState::Paused);
        assert!(!e.remote_dir().join("big").exists());
        svc.resume_job(job.id).expect("resume");
        let done = wait_for(&svc, job.id, |j| j.state.is_final()).await;
        assert_eq!(done.state, JobState::Done, "{:?}", done.error);
        assert!(std::fs::read(e.remote_dir().join("big")).expect("read") == data);

        // Simulate a daemon that died mid-job.
        svc.pause_queue();
        let other = svc.add_job(&push(id, "big", "copy")).expect("queue");
        svc.store
            .set_job_state(other.id, JobState::Running, None)
            .expect("force running");
    }
    let svc = e.open();
    let jobs = svc.store.jobs(None).expect("jobs");
    let copy = jobs
        .iter()
        .find(|j| j.spec.dst == "copy")
        .expect("copy job");
    let copy = wait_for(&svc, copy.id, |j| j.state.is_final()).await;
    assert_eq!(copy.state, JobState::Done, "{:?}", copy.error);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_changed_host_key_blocks_the_server_until_replaced() {
    let Some(e) = env() else { return };
    let Some(impostor) = support::TestSshd::start() else {
        return;
    };
    let svc = e.open();
    let id = trusted(&svc, &e, "box").await;
    std::fs::write(e.local.path().join("f"), b"data").expect("write");

    // Same host and port, different server: point the record at the impostor's port, and pin
    // the real key for that port too, as if the box had been reinstalled.
    let mut spec = e.spec("box");
    spec.port = impostor.addr().port();
    svc.update_server(id, &spec).expect("update");
    let real_key = std::fs::read_to_string(e.sshd.dir().join("host_ed25519.pub")).expect("key");
    let host = hopsync::hostkeys::HostId::new(&spec.host, spec.port).expect("host");
    let mut known =
        hopsync::hostkeys::KnownHosts::load(&e.data.path().join("data/known_hosts")).expect("load");
    known
        .pin(
            &host,
            &russh::keys::PublicKey::from_openssh(real_key.trim()).expect("parse"),
        )
        .expect("pin");

    let job = svc.add_job(&push(id, "f", "f")).expect("queue");
    let failed = wait_for(&svc, job.id, |j| j.state.is_final()).await;
    assert_eq!(failed.state, JobState::Failed);
    let server = svc.store.server(id).expect("server");
    assert!(
        server
            .blocked
            .as_deref()
            .is_some_and(|b| b.contains("host key changed")),
        "{server:?}"
    );
    assert!(
        matches!(
            svc.add_job(&push(id, "f", "g")),
            Err(ServiceError::Conflict(_))
        ),
        "no new jobs for a blocked server"
    );
    let probe = svc.probe(id).await.expect("probe");
    assert!(matches!(probe.status, KeyStatus::Changed { .. }));
    assert!(matches!(
        svc.trust(id, &probe.fingerprint, false).await,
        Err(ServiceError::Conflict(_))
    ));
    svc.trust(id, &probe.fingerprint, true)
        .await
        .expect("replace");
    assert_eq!(svc.store.server(id).expect("server").blocked, None);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_generated_key_is_sealed_and_works_once_installed() {
    let Some(e) = env() else { return };
    let svc = e.open();
    let mut spec = e.spec("gen");
    spec.key_file = None;
    let s = svc.add_server(&spec, true).expect("add");
    let public = svc.public_key(s.id).expect("public").expect("generated");
    assert!(public.starts_with("ssh-ed25519 "), "{public}");
    // Not in the database in the clear.
    let db = std::fs::read(e.data.path().join("data/hopsync.db")).expect("read db");
    assert!(
        !db.windows(35)
            .any(|w| w == b"-----BEGIN OPENSSH PRIVATE KEY-----")
    );

    let probe = svc.probe(s.id).await.expect("probe");
    svc.trust(s.id, &probe.fingerprint, false)
        .await
        .expect("trust");
    assert!(
        svc.list_remote(s.id, "").await.is_err(),
        "key not installed yet"
    );
    let auth = e.sshd.dir().join("authorized_keys");
    let mut keys = std::fs::read_to_string(&auth).expect("read");
    keys.push_str(&public);
    keys.push('\n');
    std::fs::write(&auth, keys).expect("install");
    svc.list_remote(s.id, "")
        .await
        .expect("generated key works");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_second_daemon_on_the_same_data_dir_is_refused() {
    let data = tempfile::tempdir().expect("tempdir");
    let local = tempfile::tempdir().expect("tempdir");
    let config = || ServiceConfig {
        data_dir: data.path().join("data"),
        local_root: local.path().to_owned(),
        concurrency: 1,
        ssh: SshOptions::default(),
    };
    let first = Service::open(config()).expect("first");
    let server = first
        .store
        .add_server(&ServerSpec {
            name: "box".into(),
            host: "127.0.0.1".into(),
            port: 22,
            user: "u".into(),
            root: "/r".into(),
            key_file: Some("/k".into()),
            min_free: 0,
        })
        .expect("server");
    let job = first
        .store
        .add_job(&push(server.id, "a", "a"))
        .expect("job");
    first
        .store
        .set_job_state(job.id, JobState::Running, None)
        .expect("running");
    let err = Service::open(config()).err().expect("second open refused");
    assert!(err.to_string().contains("already"), "{err}");
    assert_eq!(
        first.store.job(job.id).expect("job").state,
        JobState::Running,
        "the refused daemon touched nothing"
    );
    drop(first);
    Service::open(config()).expect("open after the first one is gone");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_generated_key_is_installed_with_an_existing_login_once() {
    let Some(e) = env() else { return };
    let svc = e.open();
    let mut spec = e.spec("gen");
    spec.key_file = None;
    let s = svc.add_server(&spec, true).expect("add");
    let probe = svc.probe(s.id).await.expect("probe");
    svc.trust(s.id, &probe.fingerprint, false)
        .await
        .expect("trust");
    // The fixture's sshd reads this file; never the real ~/.ssh of the user running the tests.
    let auth_file = e.sshd.dir().join("authorized_keys");
    let before = std::fs::read_to_string(&auth_file).expect("read");
    let target = auth_file.to_str().expect("utf8");
    let login = || hopsync::ssh::KeySource::File(e.sshd.client_key());

    assert!(matches!(
        svc.install_key(s.id, login(), Some("/tmp/evil.sh")).await,
        Err(ServiceError::BadRequest(_))
    ));
    svc.install_key(s.id, login(), Some(target))
        .await
        .expect("install");
    let after = std::fs::read_to_string(&auth_file).expect("read");
    let public = svc.public_key(s.id).expect("public").expect("generated");
    assert_eq!(
        after,
        format!("{before}{public}\n"),
        "appended, nothing else changed"
    );
    svc.list_remote(s.id, "")
        .await
        .expect("the generated key logs in");

    svc.install_key(s.id, login(), Some(target))
        .await
        .expect("again");
    assert_eq!(
        std::fs::read_to_string(&auth_file).expect("read"),
        after,
        "installing twice adds nothing"
    );
}
