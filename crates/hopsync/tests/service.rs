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

/// Opens a data folder that a dropped service used. A real restart ends the process, which frees the
/// lock at once. In a test the lock can outlive the drop for a moment: a background task may hold the
/// last reference, and a process another test is forking holds a copy of the descriptor until it
/// execs. So a refusal is retried briefly. A lock that is never freed still fails.
fn open_once_free(config: impl Fn() -> ServiceConfig) -> Arc<Service> {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        match Service::open(config()) {
            Err(ServiceError::Conflict(_)) if std::time::Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10));
            }
            other => break other.expect("open service"),
        }
    }
}

impl Env {
    /// Opens the service, as a restart would.
    fn open(&self) -> Arc<Service> {
        let svc = open_once_free(|| ServiceConfig {
            data_dir: self.data.path().join("data"),
            local_root: self.local.path().to_owned(),
            hidden: Default::default(),
            concurrency: 1,
            ssh: SshOptions::default(),
        });
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
        svc.pause_queue().expect("hold");
        let job = svc.add_job(&push(id, "big", "big")).expect("queue");
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(
            svc.store.job(job.id).expect("job").state,
            JobState::Queued,
            "a paused queue starts nothing"
        );
        svc.resume_queue().expect("release");
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
        svc.pause_queue().expect("hold");
        let other = svc.add_job(&push(id, "big", "copy")).expect("queue");
        svc.store
            .set_job_state(other.id, JobState::Running, None)
            .expect("force running");
    }
    let svc = e.open();
    svc.resume_queue().expect("release");
    let jobs = svc.store.jobs(None).expect("jobs");
    let copy = jobs
        .iter()
        .find(|j| j.spec.dst == "copy")
        .expect("copy job");
    let copy = wait_for(&svc, copy.id, |j| j.state.is_final()).await;
    assert_eq!(copy.state, JobState::Done, "{:?}", copy.error);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_held_queue_stays_held_across_a_restart() {
    let Some(e) = env() else { return };
    std::fs::write(e.local.path().join("f"), b"data").expect("write");
    let job = {
        let svc = e.open();
        let id = trusted(&svc, &e, "box").await;
        svc.pause_queue().expect("hold");
        svc.add_job(&push(id, "f", "f")).expect("queue")
    };
    let svc = e.open();
    assert!(svc.queue_paused(), "the hold is still on after a restart");
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(svc.store.job(job.id).expect("job").state, JobState::Queued);
    assert!(!e.remote_dir().join("f").exists());

    svc.resume_queue().expect("release");
    let done = wait_for(&svc, job.id, |j| j.state.is_final()).await;
    assert_eq!(done.state, JobState::Done, "{:?}", done.error);
    drop(svc);
    assert!(
        !e.open().queue_paused(),
        "releasing the queue also lasts across a restart"
    );
}

/// The daemon dies after you press stop and before the transfer has reported that it stopped.
/// The job is forced to running so that nothing can finish the stop before the restart.
async fn stop_then_restart(stop: fn(&Service, i64) -> Result<(), ServiceError>) -> (Env, Job) {
    let e = env().expect("sshd");
    std::fs::write(e.local.path().join("f"), b"data").expect("write");
    let job = {
        let svc = e.open();
        let id = trusted(&svc, &e, "box").await;
        svc.pause_queue().expect("hold");
        let job = svc.add_job(&push(id, "f", "f")).expect("queue");
        svc.store
            .set_job_state(job.id, JobState::Running, None)
            .expect("force running");
        stop(&svc, job.id).expect("stop");
        assert_eq!(svc.store.job(job.id).expect("job").state, JobState::Running);
        job
    };
    let svc = e.open();
    svc.resume_queue().expect("release");
    tokio::time::sleep(Duration::from_millis(300)).await;
    let after = svc.store.job(job.id).expect("job");
    assert!(!e.remote_dir().join("f").exists(), "it must not run again");
    (e, after)
}

#[tokio::test(flavor = "multi_thread")]
async fn a_pause_pressed_just_before_a_restart_is_kept() {
    if env().is_none() {
        return;
    }
    let (e, job) = stop_then_restart(Service::pause_job).await;
    assert_eq!(job.state, JobState::Paused);
    assert!(job.finished.is_none());

    let svc = e.open();
    assert!(
        svc.store
            .job_events(job.id)
            .expect("events")
            .iter()
            .any(|ev| ev.text == "paused")
    );
    svc.resume_job(job.id).expect("resume");
    let done = wait_for(&svc, job.id, |j| j.state.is_final()).await;
    assert_eq!(done.state, JobState::Done, "{:?}", done.error);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_cancel_pressed_just_before_a_restart_is_kept() {
    if env().is_none() {
        return;
    }
    let (_e, job) = stop_then_restart(Service::cancel_job).await;
    assert_eq!(job.state, JobState::Cancelled);
    assert!(job.finished.is_some(), "a cancelled job is finished");
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
async fn an_idle_browse_connection_is_closed_and_reopens_on_demand() {
    let Some(e) = env() else { return };
    let svc = e.open();
    let s = svc.add_server(&e.spec("a"), false).expect("add");
    let probe = svc.probe(s.id).await.expect("probe");
    svc.trust(s.id, &probe.fingerprint, false)
        .await
        .expect("trust");
    svc.list_remote(s.id, "").await.expect("list");
    assert_eq!(e.sshd.sftp_server_count(), 1, "browsing keeps one session");

    assert_eq!(
        svc.close_idle_browse(Duration::from_secs(3600)),
        0,
        "a connection used just now stays open"
    );
    assert_eq!(e.sshd.sftp_server_count(), 1);

    assert_eq!(svc.close_idle_browse(Duration::ZERO), 1);
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while e.sshd.sftp_server_count() != 0 {
        assert!(
            std::time::Instant::now() < deadline,
            "the server still holds the session after it was closed"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    svc.list_remote(s.id, "").await.expect("reopens on demand");
    assert_eq!(e.sshd.sftp_server_count(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn full_access_on_a_home_folder_keeps_hidden_entries_out_of_every_transfer() {
    let Some(e) = env() else { return };
    let home = e.local.path();
    std::fs::create_dir(home.join(".ssh")).expect("mkdir");
    std::fs::write(home.join(".ssh/authorized_keys"), b"mine").expect("write");
    std::fs::write(home.join(".bashrc"), b"mine").expect("write");
    std::fs::create_dir(home.join("media")).expect("mkdir");
    std::fs::write(home.join("media/movie"), b"film").expect("write");
    std::fs::write(e.remote_dir().join("payload"), b"evil").expect("write");

    let svc = Service::open(ServiceConfig {
        data_dir: e.data.path().join("data"),
        local_root: home.to_owned(),
        hidden: hopsync::paths::Hidden::for_full_access(home),
        concurrency: 1,
        ssh: SshOptions::default(),
    })
    .expect("open service");
    svc.start();
    let id = trusted(&svc, &e, "a").await;

    let listed: Vec<String> = svc
        .list_local("")
        .await
        .expect("list")
        .into_iter()
        .map(|en| en.name)
        .collect();
    assert_eq!(listed, ["media"]);
    assert!(svc.list_local(".ssh").await.is_err());

    let visible = svc.add_job(&push(id, "media", "media")).expect("queue");
    let job = wait_for(&svc, visible.id, |j| j.state == JobState::Done).await;
    assert_eq!(job.state, JobState::Done);
    assert_eq!(
        std::fs::read(e.remote_dir().join("media/movie")).expect("sent"),
        b"film"
    );

    // Refused when queued or when run: either way nothing moves.
    if let Ok(j) = svc.add_job(&push(id, ".ssh/authorized_keys", "stolen")) {
        wait_for(&svc, j.id, |j| j.state == JobState::Failed).await;
    }
    assert!(!e.remote_dir().join("stolen").exists());

    let mut overwrite = push(id, "payload", ".bashrc");
    overwrite.direction = Direction::Pull;
    overwrite.existing = "overwrite".into();
    if let Ok(j) = svc.add_job(&overwrite) {
        wait_for(&svc, j.id, |j| j.state == JobState::Failed).await;
    }
    assert_eq!(std::fs::read(home.join(".bashrc")).expect("read"), b"mine");
}

fn browser_body(
    data: &[u8],
) -> impl futures_util::Stream<Item = Result<axum::body::Bytes, std::io::Error>> + Unpin + use<> {
    let pieces: Vec<_> = data
        .chunks(37_000)
        .map(|c| Ok(axum::body::Bytes::copy_from_slice(c)))
        .collect();
    futures_util::stream::iter(pieces)
}

async fn remote_bytes(d: &hopsync::service::RemoteDownload) -> Vec<u8> {
    let mut out = vec![0u8; usize::try_from(d.size).expect("size")];
    let n = d.file.read_at(0, &mut out).await.expect("read");
    out.truncate(n);
    out
}

#[tokio::test(flavor = "multi_thread")]
async fn a_browser_can_upload_to_and_save_from_a_connected_server() {
    let Some(e) = env() else { return };
    let svc = e.open();
    let id = trusted(&svc, &e, "a").await;
    std::fs::create_dir(e.remote_dir().join("in")).expect("mkdir");
    // Longer than many blocks, with content that shows any block out of place.
    let data: Vec<u8> = (0..5_000_000u32).map(|i| (i % 251) as u8).collect();

    let n = svc
        .upload_remote(
            id,
            "in/a b.bin",
            false,
            Some(data.len() as u64),
            browser_body(&data),
        )
        .await
        .expect("upload");
    assert_eq!(n, data.len() as u64);
    assert_eq!(
        std::fs::read(e.remote_dir().join("in/a b.bin")).expect("arrived"),
        data
    );
    assert_eq!(
        std::fs::read_dir(e.remote_dir().join("in"))
            .expect("list")
            .count(),
        1
    );

    let d = svc
        .open_remote_download(id, "in/a b.bin")
        .await
        .expect("open");
    assert_eq!((d.size, d.name.as_str()), (data.len() as u64, "a b.bin"));
    assert_eq!(remote_bytes(&d).await, data);

    // An existing file needs a clear yes.
    assert!(matches!(
        svc.upload_remote(id, "in/a b.bin", false, Some(1), browser_body(b"x"))
            .await,
        Err(ServiceError::Conflict(_))
    ));
    assert_eq!(
        std::fs::read(e.remote_dir().join("in/a b.bin")).expect("kept"),
        data
    );
    svc.upload_remote(id, "in/a b.bin", true, Some(1), browser_body(b"x"))
        .await
        .expect("replace");
    assert_eq!(
        std::fs::read(e.remote_dir().join("in/a b.bin")).expect("new"),
        b"x"
    );

    // An empty file is a file too.
    svc.upload_remote(id, "empty", false, Some(0), browser_body(b""))
        .await
        .expect("empty");
    assert_eq!(
        std::fs::read(e.remote_dir().join("empty")).expect("empty"),
        b""
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn browser_transfers_to_a_server_stay_in_its_root_folder() {
    let Some(e) = env() else { return };
    let svc = e.open();
    let id = trusted(&svc, &e, "a").await;
    let outside = e.sshd.dir().join("outside");
    std::fs::write(&outside, b"secret").expect("write");
    std::os::unix::fs::symlink(&outside, e.remote_dir().join("link")).expect("symlink");
    std::fs::create_dir(e.remote_dir().join("dir")).expect("mkdir");

    assert!(matches!(
        svc.open_remote_download(id, "../outside").await,
        Err(ServiceError::BadRequest(_))
    ));
    assert!(matches!(
        svc.upload_remote(id, "../escaped", false, Some(1), browser_body(b"x"))
            .await,
        Err(ServiceError::BadRequest(_))
    ));
    assert!(!e.sshd.dir().join("escaped").exists());
    assert!(matches!(
        svc.open_remote_download(id, "link").await,
        Err(ServiceError::BadRequest(_))
    ));
    assert!(matches!(
        svc.open_remote_download(id, "dir").await,
        Err(ServiceError::BadRequest(_))
    ));
    assert!(matches!(
        svc.open_remote_download(id, "missing").await,
        Err(ServiceError::NotFound)
    ));
    assert!(matches!(
        svc.upload_remote(id, "dir", true, Some(1), browser_body(b"x"))
            .await,
        Err(ServiceError::Conflict(_))
    ));
    assert!(
        svc.upload_remote(id, "no/such/file", false, Some(1), browser_body(b"x"))
            .await
            .is_err()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn browser_transfers_do_not_pass_through_a_symlink_on_the_server() {
    let Some(e) = env() else { return };
    let svc = e.open();
    let id = trusted(&svc, &e, "a").await;
    let outside = e.sshd.dir().join("outside-dir");
    std::fs::create_dir(&outside).expect("mkdir");
    std::fs::write(outside.join("secret"), b"secret").expect("write");
    std::os::unix::fs::symlink(&outside, e.remote_dir().join("link")).expect("symlink");

    assert!(matches!(
        svc.open_remote_download(id, "link/secret").await,
        Err(ServiceError::BadRequest(_))
    ));
    assert!(matches!(
        svc.upload_remote(id, "link/new", false, Some(1), browser_body(b"x"))
            .await,
        Err(ServiceError::BadRequest(_))
    ));
    assert!(!outside.join("new").exists());
    assert!(!outside.join("new.hopsync-upload").exists());

    // A link planted at the part file's name must not be written through.
    let victim = outside.join("victim");
    std::fs::write(&victim, b"keep me").expect("write");
    std::os::unix::fs::symlink(&victim, e.remote_dir().join("up.bin.hopsync-upload"))
        .expect("symlink");
    assert!(matches!(
        svc.upload_remote(id, "up.bin", false, Some(1), browser_body(b"x"))
            .await,
        Err(ServiceError::Conflict(_))
    ));
    assert_eq!(std::fs::read(&victim).expect("read"), b"keep me");
    assert!(
        e.remote_dir().join("up.bin.hopsync-upload").is_symlink(),
        "a part file this upload did not make stays where it was"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn browsing_a_server_does_not_pass_through_a_symlink() {
    let Some(e) = env() else { return };
    let svc = e.open();
    let id = trusted(&svc, &e, "a").await;
    let outside = e.sshd.dir().join("outside-dir");
    std::fs::create_dir_all(outside.join("sub")).expect("mkdir");
    std::fs::write(outside.join("secret"), b"secret").expect("write");
    std::os::unix::fs::symlink(&outside, e.remote_dir().join("link")).expect("symlink");
    std::fs::create_dir_all(e.remote_dir().join("real/folder")).expect("mkdir");

    for path in ["link", "link/sub"] {
        assert!(
            matches!(
                svc.list_remote(id, path).await,
                Err(ServiceError::BadRequest(_))
            ),
            "{path} must be refused"
        );
    }
    // The link still shows as a row, and real folders still open.
    let top = svc.list_remote(id, "").await.expect("list");
    assert!(
        top.iter()
            .any(|f| f.name == "link" && f.kind == Kind::Symlink)
    );
    let real = svc.list_remote(id, "real").await.expect("list");
    assert!(real.iter().any(|f| f.name == "folder"));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_cut_upload_to_a_server_leaves_nothing_behind() {
    let Some(e) = env() else { return };
    let svc = e.open();
    let id = trusted(&svc, &e, "a").await;
    let cut = futures_util::stream::iter(vec![
        Ok(axum::body::Bytes::from(vec![7u8; 600_000])),
        Err(std::io::Error::other("the browser went away")),
    ]);
    assert!(
        svc.upload_remote(id, "cut.bin", false, Some(9_000_000), cut)
            .await
            .is_err()
    );
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while std::fs::read_dir(e.remote_dir()).expect("list").count() != 0 {
        assert!(std::time::Instant::now() < deadline, "the part file stayed");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    // The name is free again.
    svc.upload_remote(id, "cut.bin", false, Some(1), browser_body(b"x"))
        .await
        .expect("retry");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_second_daemon_on_the_same_data_dir_is_refused() {
    let data = tempfile::tempdir().expect("tempdir");
    let local = tempfile::tempdir().expect("tempdir");
    let config = || ServiceConfig {
        data_dir: data.path().join("data"),
        local_root: local.path().to_owned(),
        hidden: Default::default(),
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
    open_once_free(config);
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

/// A running tree job reports what it will send before its walk is done: files and bytes from a
/// counting walk beside the transfer, bytes landed, and the files in flight.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_tree_job_reports_its_totals_while_running() {
    let Some(e) = env() else { return };
    let dir = e.local.path().join("tree");
    std::fs::create_dir(&dir).expect("mkdir");
    let (files, size) = (8u64, 64usize << 20);
    for i in 0..files {
        std::fs::write(dir.join(format!("f{i}")), vec![i as u8; size]).expect("write");
    }
    let svc = e.open();
    let id = trusted(&svc, &e, "box").await;
    let mut snaps = svc.subscribe();
    let job = svc
        .add_job(&JobSpec {
            lanes: Some(1),
            ..push(id, "tree", "tree")
        })
        .expect("queue");
    let mut seen = None;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    while seen.is_none() && tokio::time::Instant::now() < deadline {
        if tokio::time::timeout(Duration::from_secs(1), snaps.changed())
            .await
            .is_err()
        {
            continue;
        }
        let snap = snaps.borrow_and_update().clone();
        if let Some(p) = snap
            .running
            .iter()
            .find(|p| p.job == job.id && p.total_files.is_some())
        {
            seen = Some(p.clone());
        }
        if svc.store.job(job.id).expect("job").state.is_final() {
            break;
        }
    }
    let p = seen.expect("a running snapshot with totals");
    assert_eq!(p.total_files, Some(files));
    assert_eq!(p.total, Some(files * size as u64));
    assert!(p.done <= files * size as u64);
    assert!(p.active.len() <= 16);
    for f in &p.active {
        assert!(f.done <= f.size && f.size == size as u64, "{f:?}");
    }
    let done = wait_for(&svc, job.id, |j| j.state.is_final()).await;
    assert_eq!(done.state, JobState::Done, "{:?}", done.error);
}
