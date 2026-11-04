//! Phase 0 throwaway spike: measures SFTP throughput of candidate SSH crates.
//!
//! Splits `size` bytes into `lanes` contiguous ranges. Each lane is its own SSH connection with one
//! open remote file and `inflight` workers, each keeping one `req`-sized read or write in flight.
//! Timing starts after every lane has connected and opened its file, so handshakes are reported
//! separately from transfer time.
//!
//! Usage: sftp-bench key=value...  (see `Args::parse` for keys). Prints one JSON line per run.
//! Not product code: errors are anyhow, no retries, no path confinement.

use std::borrow::Cow;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use tokio::sync::Barrier;

#[derive(Clone, Copy, PartialEq)]
enum Backend {
    Russh,
    Openssh,
}

#[derive(Clone, Copy, PartialEq)]
enum Dir {
    Up,
    Down,
}

#[derive(Clone)]
struct Args {
    backend: Backend,
    dir: Dir,
    host: String,
    port: u16,
    user: String,
    key: String,
    known_hosts: String,
    /// ssh_config file for the openssh backend (sets Ciphers etc.).
    ssh_config: Option<String>,
    cipher: String,
    remote: String,
    size: u64,
    lanes: u64,
    inflight: u64,
    req: u64,
    /// russh channel window in bytes; None keeps the crate default.
    window: Option<u32>,
    /// openssh-sftp-client write-buffer flush interval in microseconds; None keeps the default.
    flush_us: Option<u64>,
    /// openssh backend: use write_all_zero_copy instead of write_all.
    zero_copy: bool,
    /// russh: set TCP_NODELAY (default true).
    nodelay: bool,
}

impl Args {
    fn parse() -> Result<Self> {
        let mut kv = std::collections::HashMap::new();
        for a in std::env::args().skip(1) {
            let (k, v) = a
                .split_once('=')
                .with_context(|| format!("expected key=value, got {a}"))?;
            kv.insert(k.to_owned(), v.to_owned());
        }
        let get = |k: &str| kv.get(k).cloned().with_context(|| format!("missing {k}="));
        let num = |k: &str, d: u64| -> Result<u64> {
            kv.get(k).map_or(Ok(d), |v| {
                parse_size(v).with_context(|| format!("bad {k}="))
            })
        };
        Ok(Self {
            backend: match get("backend")?.as_str() {
                "russh" => Backend::Russh,
                "openssh" => Backend::Openssh,
                b => bail!("unknown backend {b}"),
            },
            dir: match get("dir")?.as_str() {
                "up" => Dir::Up,
                "down" => Dir::Down,
                d => bail!("unknown dir {d}"),
            },
            host: get("host")?,
            port: num("port", 22)?.try_into()?,
            user: get("user")?,
            key: get("key")?,
            known_hosts: get("known_hosts")?,
            ssh_config: kv.get("ssh_config").cloned(),
            cipher: kv
                .get("cipher")
                .cloned()
                .unwrap_or_else(|| "aes128-gcm@openssh.com".into()),
            remote: get("remote")?,
            size: num("size", 4 << 30)?,
            lanes: num("lanes", 1)?.max(1),
            inflight: num("inflight", 16)?.max(1),
            req: num("req", 256 << 10)?,
            window: kv
                .get("window")
                .map(|v| parse_size(v))
                .transpose()?
                .map(u32::try_from)
                .transpose()?,
            flush_us: kv.get("flush_us").map(|v| v.parse()).transpose()?,
            zero_copy: kv.get("zc").is_some_and(|v| v == "1"),
            nodelay: kv.get("nodelay").is_none_or(|v| v == "1"),
        })
    }
}

/// Accepts plain bytes or a K/M/G suffix (binary units).
fn parse_size(s: &str) -> Result<u64> {
    let (n, mul) = match s.as_bytes().last() {
        Some(b'K' | b'k') => (&s[..s.len() - 1], 1 << 10),
        Some(b'M' | b'm') => (&s[..s.len() - 1], 1 << 20),
        Some(b'G' | b'g') => (&s[..s.len() - 1], 1 << 30),
        _ => (s, 1),
    };
    Ok(n.parse::<u64>()? * mul)
}

/// Hands out `req`-sized chunks of one lane's byte range to that lane's workers.
struct Chunks {
    next: AtomicU64,
    end: u64,
    req: u64,
}

impl Chunks {
    fn take(&self) -> Option<(u64, u64)> {
        let off = self.next.fetch_add(self.req, Ordering::Relaxed);
        (off < self.end).then(|| (off, self.req.min(self.end - off)))
    }
}

/// Busy jiffies across all CPUs from /proc/stat (includes the local sshd on loopback).
fn system_busy_secs() -> Result<f64> {
    let stat = std::fs::read_to_string("/proc/stat")?;
    let cpu = stat.lines().next().context("empty /proc/stat")?;
    let f: Vec<u64> = cpu
        .split_whitespace()
        .skip(1)
        .map(|x| x.parse().unwrap_or(0))
        .collect();
    // user nice system idle iowait irq softirq steal: busy = all but idle and iowait.
    let busy: u64 = f
        .iter()
        .take(8)
        .enumerate()
        .filter(|(i, _)| *i != 3 && *i != 4)
        .map(|(_, v)| v)
        .sum();
    Ok(busy as f64 / 100.0)
}

/// This process's user+system CPU seconds from /proc/self/stat.
fn self_cpu_secs() -> Result<f64> {
    let stat = std::fs::read_to_string("/proc/self/stat")?;
    let after_comm = stat.rsplit_once(") ").context("bad /proc/self/stat")?.1;
    let f: Vec<&str> = after_comm.split_whitespace().collect();
    // Fields 14 and 15 overall are utime and stime; after "pid (comm) " they are index 11 and 12.
    Ok((f[11].parse::<u64>()? + f[12].parse::<u64>()?) as f64 / 100.0)
}

fn peak_rss_kib() -> u64 {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("VmHWM:"))
                .map(str::to_owned)
        })
        .and_then(|l| l.split_whitespace().nth(1).and_then(|v| v.parse().ok()))
        .unwrap_or(0)
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse()?;
    let start_barrier = Arc::new(Barrier::new(args.lanes as usize + 1));
    let per_lane = args.size.div_ceil(args.lanes);

    let t_connect = Instant::now();
    let mut tasks = Vec::new();
    for lane in 0..args.lanes {
        let chunks = Arc::new(Chunks {
            next: AtomicU64::new(lane * per_lane),
            end: ((lane + 1) * per_lane).min(args.size),
            req: args.req,
        });
        let (a, b) = (args.clone(), start_barrier.clone());
        tasks.push(tokio::spawn(async move {
            match a.backend {
                Backend::Russh => russh_lane::run(a, chunks, b).await,
                Backend::Openssh => openssh_lane::run(a, chunks, b).await,
            }
        }));
    }
    start_barrier.wait().await;
    let connect_secs = t_connect.elapsed().as_secs_f64();
    let (sys0, self0, t0) = (system_busy_secs()?, self_cpu_secs()?, Instant::now());

    let mut bytes = 0;
    for t in tasks {
        bytes += t.await??;
    }
    let secs = t0.elapsed().as_secs_f64();
    let (sys_cpu, self_cpu) = (system_busy_secs()? - sys0, self_cpu_secs()? - self0);
    if bytes != args.size {
        bail!("moved {bytes} bytes, expected {}", args.size);
    }

    let gb = bytes as f64 / 1e9;
    println!(
        "{{\"backend\":\"{}\",\"dir\":\"{}\",\"cipher\":\"{}\",\"lanes\":{},\"inflight\":{},\"req\":{},\
\"window\":{},\"bytes\":{bytes},\"connect_s\":{connect_secs:.3},\"secs\":{secs:.3},\"MBps\":{:.1},\
\"sys_cpu_s_per_GB\":{:.3},\"self_cpu_s_per_GB\":{:.3},\"peak_rss_MiB\":{:.1}}}",
        if args.backend == Backend::Russh {
            "russh"
        } else {
            "openssh"
        },
        if args.dir == Dir::Up { "up" } else { "down" },
        args.cipher,
        args.lanes,
        args.inflight,
        args.req,
        args.window.map_or("null".into(), |w| w.to_string()),
        bytes as f64 / 1e6 / secs,
        sys_cpu / gb,
        self_cpu / gb,
        peak_rss_kib() as f64 / 1024.0,
    );
    Ok(())
}

mod russh_lane {
    use super::*;
    use russh::client;
    use russh::keys::{PrivateKeyWithHashAlg, PublicKey, PublicKeyOrCertificate, load_secret_key};
    use russh_sftp::client::RawSftpSession;
    use russh_sftp::protocol::{FileAttributes, OpenFlags};

    /// Accepts only host keys pinned in the known_hosts file.
    struct Pinned(Vec<PublicKey>);

    impl client::Handler for Pinned {
        type Error = russh::Error;
        async fn check_server_key(
            &mut self,
            key: &PublicKeyOrCertificate,
        ) -> Result<bool, Self::Error> {
            Ok(
                matches!(key, PublicKeyOrCertificate::PublicKey { key, .. } if self.0.iter().any(|p| p.key_data() == key.key_data())),
            )
        }
    }

    /// Reads every "<host> <type> <base64>" line as a pinned key. The file must hold only the target
    /// host's entries (for example the output of `ssh-keygen -F <host>`); host names are not matched.
    fn pinned_keys(path: &str) -> Result<Vec<PublicKey>> {
        let text = std::fs::read_to_string(path)?;
        let keys: Vec<PublicKey> = text
            .lines()
            .filter(|l| !l.trim().is_empty() && !l.starts_with('#') && !l.starts_with('@'))
            .filter_map(|l| {
                let key = l
                    .split_whitespace()
                    .skip(1)
                    .take(2)
                    .collect::<Vec<_>>()
                    .join(" ");
                PublicKey::from_openssh(&key).ok()
            })
            .collect();
        if keys.is_empty() {
            bail!("no usable host keys in {path}");
        }
        Ok(keys)
    }

    pub async fn run(a: Args, chunks: Arc<Chunks>, start: Arc<Barrier>) -> Result<u64> {
        let mut cfg = client::Config {
            nodelay: a.nodelay,
            preferred: russh::Preferred {
                cipher: Cow::Owned(vec![
                    russh::cipher::Name::try_from(a.cipher.as_str()).map_err(|_| {
                        anyhow::anyhow!("russh does not support cipher {}", a.cipher)
                    })?,
                ]),
                ..Default::default()
            },
            ..Default::default()
        };
        if let Some(w) = a.window {
            cfg.window_size = w;
        }
        let mut ssh = client::connect(
            Arc::new(cfg),
            (a.host.as_str(), a.port),
            Pinned(pinned_keys(&a.known_hosts)?),
        )
        .await
        .context("russh connect")?;
        let key = PrivateKeyWithHashAlg::new(Arc::new(load_secret_key(&a.key, None)?), None);
        if !ssh.authenticate_publickey(&a.user, key).await?.success() {
            bail!("russh publickey auth rejected");
        }
        let channel = ssh.channel_open_session().await?;
        channel.request_subsystem(true, "sftp").await?;
        let sftp = Arc::new(RawSftpSession::new(channel.into_stream()));
        sftp.set_timeout(60);
        sftp.init().await?;
        // OpenSSH sftp-server drops the connection on oversized packets, so honour limits@openssh.com.
        // 32 KiB is the minimum every SFTP server must accept.
        let (max_w, max_r) = match sftp.limits().await {
            Ok(l) => (l.max_write_len, l.max_read_len),
            Err(_) => (32 << 10, 32 << 10),
        };
        eprintln!("russh: server limits max_write_len={max_w} max_read_len={max_r}");
        let flags = match a.dir {
            Dir::Up => OpenFlags::WRITE | OpenFlags::CREATE,
            Dir::Down => OpenFlags::READ,
        };
        let handle = sftp
            .open(&a.remote, flags, FileAttributes::empty())
            .await?
            .handle;

        start.wait().await;
        let mut workers = Vec::new();
        for _ in 0..a.inflight {
            let (sftp, chunks, handle) = (sftp.clone(), chunks.clone(), handle.clone());
            workers.push(tokio::spawn(async move {
                let mut moved = 0u64;
                let zeros = vec![0u8; chunks.req as usize];
                while let Some((off, len)) = chunks.take() {
                    match a.dir {
                        // The API takes an owned Vec, so every request copies the payload once.
                        Dir::Up => {
                            let mut sent = 0;
                            while sent < len {
                                let n = (len - sent).min(max_w);
                                let data = zeros[..n as usize].to_vec();
                                sftp.write(handle.as_str(), off + sent, data).await?;
                                sent += n;
                            }
                            moved += len;
                        }
                        Dir::Down => {
                            let mut got = 0;
                            while got < len {
                                let n = (len - got).min(max_r) as u32;
                                let d = sftp.read(handle.as_str(), off + got, n).await?;
                                if d.data.is_empty() {
                                    bail!("unexpected EOF at {}", off + got);
                                }
                                got += d.data.len() as u64;
                            }
                            moved += got;
                        }
                    }
                }
                anyhow::Ok(moved)
            }));
        }
        let mut moved = 0;
        for w in workers {
            moved += w.await??;
        }
        sftp.close(handle).await?;
        ssh.disconnect(russh::Disconnect::ByApplication, "", "en")
            .await?;
        Ok(moved)
    }
}

mod openssh_lane {
    use super::*;
    use bytes::BytesMut;
    use openssh::{KnownHosts, SessionBuilder};
    use openssh_sftp_client::{Sftp, SftpOptions};
    use tokio::io::AsyncSeekExt;

    pub async fn run(a: Args, chunks: Arc<Chunks>, start: Arc<Barrier>) -> Result<u64> {
        let mut b = SessionBuilder::default();
        b.port(a.port)
            .user(a.user.clone())
            .keyfile(&a.key)
            .known_hosts_check(KnownHosts::Strict)
            .user_known_hosts_file(&a.known_hosts)
            .connect_timeout(Duration::from_secs(15));
        if let Some(c) = &a.ssh_config {
            b.config_file(c);
        }
        let session = b.connect(&a.host).await.context("openssh connect")?;
        let mut opts = SftpOptions::new();
        if let Some(us) = a.flush_us {
            opts = opts.flush_interval(Duration::from_micros(us));
        }
        let sftp = Sftp::from_session(session, opts).await?;
        let file = match a.dir {
            Dir::Up => {
                sftp.options()
                    .write(true)
                    .create(true)
                    .open(&a.remote)
                    .await?
            }
            Dir::Down => sftp.open(&a.remote).await?,
        };

        start.wait().await;
        let mut workers = Vec::new();
        for _ in 0..a.inflight {
            let (mut f, chunks) = (file.clone(), chunks.clone());
            workers.push(tokio::spawn(async move {
                let mut moved = 0u64;
                let zeros = vec![0u8; chunks.req as usize];
                let zeros_shared = bytes::Bytes::from(zeros.clone());
                let mut buf = BytesMut::with_capacity(chunks.req as usize);
                while let Some((off, len)) = chunks.take() {
                    f.seek(std::io::SeekFrom::Start(off)).await?;
                    match a.dir {
                        Dir::Up if a.zero_copy => {
                            f.write_all_zero_copy(&mut [zeros_shared.slice(..len as usize)])
                                .await?
                        }
                        Dir::Up => f.write_all(&zeros[..len as usize]).await?,
                        Dir::Down => {
                            let mut got = 0;
                            while got < len {
                                buf.clear();
                                let d = f
                                    .read((len - got) as u32, std::mem::take(&mut buf))
                                    .await?
                                    .with_context(|| format!("unexpected EOF at {}", off + got))?;
                                got += d.len() as u64;
                                buf = d;
                            }
                        }
                    }
                    moved += len;
                }
                anyhow::Ok(moved)
            }));
        }
        let mut moved = 0;
        for w in workers {
            moved += w.await??;
        }
        file.close().await?;
        sftp.close().await?;
        Ok(moved)
    }
}
