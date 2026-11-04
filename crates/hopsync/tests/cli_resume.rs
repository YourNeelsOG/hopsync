//! The real `hopsync` binary, killed with SIGKILL mid-push, resumes on the next run (Phase 1
//! step 11).

mod support;

use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const MIB: usize = 1 << 20;

fn same_contents(a: &Path, b: &Path) -> bool {
    let (mut fa, mut fb) = (
        std::fs::File::open(a).expect("open a"),
        std::fs::File::open(b).expect("open b"),
    );
    let (mut ba, mut bb) = (vec![0u8; MIB], vec![0u8; MIB]);
    loop {
        let n = fa.read(&mut ba).expect("read a");
        let mut m = 0;
        while m < n {
            match fb.read(&mut bb[m..n]).expect("read b") {
                0 => return false,
                k => m += k,
            }
        }
        if ba[..n] != bb[..n] {
            return false;
        }
        if n == 0 {
            return fb.read(&mut bb).expect("read b") == 0;
        }
    }
}

fn recorded_chunks(journal: &Path) -> i64 {
    rusqlite::Connection::open_with_flags(journal, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
        .and_then(|c| c.query_row("SELECT count(*) FROM chunks", [], |r| r.get(0)))
        .unwrap_or(0)
}

#[test]
fn killed_push_resumes_on_the_next_run() {
    let Some(sshd) = support::TestSshd::start() else {
        return;
    };
    let (pins, _known, target) = sshd.client();
    let journal = pins.path().join("journal.sqlite");
    // Disk-backed, like a real transfer; 4 sequential chunks of 64 MiB on one lane.
    let work = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).expect("tempdir");
    let src = work.path().join("big.bin");
    {
        let mut f = std::fs::File::create(&src).expect("create");
        let mut block = vec![0u8; MIB];
        for i in 0..256u32 {
            for (j, b) in block.iter_mut().enumerate() {
                *b = (j as u32 ^ i.wrapping_mul(2_654_435_761)) as u8;
            }
            std::io::Write::write_all(&mut f, &block).expect("write");
        }
    }
    let remote_dir = sshd.dir().join("remote");
    std::fs::create_dir(&remote_dir).expect("mkdir");
    let cmd = || {
        let mut c = Command::new(env!("CARGO_BIN_EXE_hopsync"));
        c.arg("--known-hosts")
            .arg(pins.path().join("known_hosts"))
            .arg("--journal")
            .arg(&journal)
            .arg("push")
            .arg(&src)
            .arg(format!(
                "{}@{}:{}/",
                target.user,
                sshd.addr.ip(),
                remote_dir.display()
            ))
            .args(["-p", &sshd.addr.port().to_string(), "-l", "1", "-i"])
            .arg(sshd.client_key());
        c
    };

    let mut child = cmd()
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn hopsync");
    let start = Instant::now();
    while recorded_chunks(&journal) < 1 {
        assert!(
            child.try_wait().expect("try_wait").is_none(),
            "hopsync finished before it could be killed"
        );
        assert!(
            start.elapsed() < Duration::from_secs(120),
            "no chunk recorded"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    child.kill().expect("SIGKILL");
    child.wait().expect("reap");
    assert!(
        !remote_dir.join("big.bin").exists(),
        "killed run must not rename"
    );
    assert!(remote_dir.join("big.bin.hopsync-part").exists());

    let out = cmd().output().expect("rerun");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "rerun failed: {stdout}{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(stdout.contains("resumed"), "no resume reported: {stdout}");
    assert!(same_contents(&src, &remote_dir.join("big.bin")));
    assert!(!remote_dir.join("big.bin.hopsync-part").exists());
}
