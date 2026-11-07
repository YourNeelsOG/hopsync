//! `hopsync` command line: standalone push and pull with host-key pinning (Phase 1), the daemon
//! (`serve`), and commands that drive a running daemon over its socket (Phase 2).

use std::io::{BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use clap::{Args, Parser, Subcommand, ValueEnum};

use hopsync::cli::{RemoteSpec, parse_size, printable, split_slash_path};
use hopsync::engine::{self, Remote, TransferOptions};
use hopsync::hostkeys::{self, HostId, KnownHosts, TrustOutcome};
use hopsync::journal::Journal;
use hopsync::localio::CachePolicy;
use hopsync::paths::{LocalRoot, Name, RelPath, RemoteRoot};
use hopsync::policy::Existing;
use hopsync::sftp::RemoteKind;
use hopsync::ssh::{KeySource, SshTarget};

#[derive(Parser)]
#[command(
    name = "hopsync",
    version,
    about = "Fast file transfer between SSH hosts"
)]
struct Cli {
    /// Hopsync's own known_hosts file (default: $XDG_CONFIG_HOME/hopsync/known_hosts).
    #[arg(long, global = true, value_name = "FILE")]
    known_hosts: Option<PathBuf>,
    /// Resume journal (default: $XDG_STATE_HOME/hopsync/journal.sqlite).
    #[arg(long, global = true, value_name = "FILE")]
    journal: Option<PathBuf>,
    /// Async worker threads. Few are faster: more threads cost CPU without adding throughput.
    #[arg(long, global = true, default_value_t = 2, value_parser = clap::value_parser!(u16).range(1..=64))]
    threads: u16,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Copy a local file or directory tree to a remote host. Symlinks are skipped and reported.
    Push {
        local: PathBuf,
        /// [user@]host:path. An existing directory or a trailing '/' receives the file or directory
        /// under its own name.
        remote: String,
        #[command(flatten)]
        t: TransferArgs,
    },
    /// Copy a remote file or directory tree to this machine. Symlinks are skipped and reported.
    Pull {
        /// [user@]host:path
        remote: String,
        /// An existing directory or a trailing '/' receives the file under its name.
        local: PathBuf,
        #[command(flatten)]
        t: TransferArgs,
    },
    /// Manage pinned host keys.
    Hosts {
        #[command(subcommand)]
        cmd: HostsCmd,
    },
    /// Run the daemon: web UI and API over HTTP, and a socket for the commands below.
    Serve(ServeArgs),
    /// List the daemon's jobs.
    Jobs {
        /// Only jobs in this state: queued, running, paused, done, failed or cancelled.
        #[arg(long)]
        state: Option<String>,
        #[command(flatten)]
        d: DaemonArgs,
    },
    /// Queue a transfer on the daemon. Paths are relative to the daemon's local root and to the
    /// server's root.
    Queue {
        #[arg(value_enum)]
        direction: DirectionArg,
        /// Server name, as listed by `hopsync servers`.
        server: String,
        src: String,
        dst: String,
        #[arg(long, value_enum, default_value_t = ExistingArg::Skip)]
        existing: ExistingArg,
        #[arg(long)]
        verify: bool,
        #[arg(short, long, value_parser = clap::value_parser!(u32).range(1..=64))]
        lanes: Option<u32>,
        /// Show what would happen without queueing anything.
        #[arg(long)]
        dry_run: bool,
        #[command(flatten)]
        d: DaemonArgs,
    },
    /// Pause, resume, cancel, show or remove one job.
    Job {
        #[arg(value_enum)]
        action: JobAction,
        id: i64,
        #[command(flatten)]
        d: DaemonArgs,
    },
    /// List the daemon's servers.
    Servers {
        #[command(flatten)]
        d: DaemonArgs,
    },
    /// Install the service and web UI on this box, and set the admin password. Works with or
    /// without root. Safe to run again.
    Setup {
        /// The only local folder transfers can use. Asked for on a terminal if not given.
        #[arg(long, value_name = "DIR")]
        folder: Option<String>,
        /// Let the web UI read and write this whole box as root, or your whole home folder without
        /// root. A stolen web password then controls that much. Asked for on a terminal.
        #[arg(long, conflicts_with = "folder_only")]
        full_access: bool,
        /// Keep the web UI to the one folder. This is the default.
        #[arg(long)]
        folder_only: bool,
        /// Port for the web UI. Without it, the first free port from 7890.
        #[arg(long)]
        port: Option<u16>,
        /// Open the web UI on this server's address, over HTTPS. Asked for on a terminal.
        #[arg(long, conflicts_with = "local_only")]
        public: bool,
        /// Keep the web UI on 127.0.0.1, reached through an SSH tunnel.
        #[arg(long)]
        local_only: bool,
        /// Your own certificate chain in PEM, for a domain you have. Implies --public.
        #[arg(
            long,
            value_name = "FILE",
            requires = "tls_key",
            conflicts_with = "local_only"
        )]
        tls_cert: Option<String>,
        /// The private key of --tls-cert, in PEM.
        #[arg(long, value_name = "FILE", requires = "tls_cert")]
        tls_key: Option<String>,
        /// Read the admin password from a line on standard input, for scripts.
        #[arg(long)]
        password_stdin: bool,
        /// No animation or colour.
        #[arg(long)]
        plain: bool,
    },
    /// Show whether the service is running.
    Status,
    /// Start the service.
    Start,
    /// Stop the service.
    Stop,
    /// Restart the service.
    Restart,
    /// Show the service's log, and keep following it.
    Logs {
        /// Print the latest lines and exit.
        #[arg(long)]
        no_follow: bool,
    },
    /// Remove the service and the binary. Your files and settings stay.
    Uninstall {
        /// Also delete the settings, saved servers and keys.
        #[arg(long)]
        purge: bool,
        /// Do not ask first.
        #[arg(long)]
        yes: bool,
        /// No animation or colour.
        #[arg(long)]
        plain: bool,
    },
}

#[derive(Args)]
struct ServeArgs {
    /// Address for the web UI and API. Anything but loopback needs --tls, or a TLS reverse proxy
    /// in front: it would be plain HTTP on the network.
    #[arg(long, default_value = "127.0.0.1:7890")]
    listen: std::net::SocketAddr,
    /// Database, vault key and pinned host keys (default: $XDG_DATA_HOME/hopsync).
    #[arg(long, value_name = "DIR")]
    data_dir: Option<PathBuf>,
    /// Local directory that every local path stays under (default: the home directory).
    #[arg(long, value_name = "DIR")]
    root: Option<PathBuf>,
    /// The whole of --root is open to the web UI, except what `hopsync setup` documents as hidden.
    #[arg(long, requires = "root")]
    full_access: bool,
    #[arg(long, value_name = "FILE")]
    socket: Option<PathBuf>,
    /// Jobs that run at once.
    #[arg(long, default_value_t = 2, value_parser = clap::value_parser!(u16).range(1..=16))]
    jobs: u16,
    /// Mark session cookies Secure: a TLS reverse proxy serves the UI over HTTPS.
    #[arg(long)]
    secure_cookies: bool,
    /// Serve HTTPS with a certificate generated in the data directory, or the one named by
    /// --tls-cert and --tls-key.
    #[arg(long)]
    tls: bool,
    /// A name or IP address for the generated certificate. Repeat for more.
    #[arg(long, value_name = "NAME", requires = "tls")]
    tls_name: Vec<String>,
    /// Your own certificate chain, in PEM. Read at start, so a renewal needs a restart.
    #[arg(
        long,
        value_name = "FILE",
        requires = "tls",
        requires = "tls_key",
        conflicts_with = "tls_name"
    )]
    tls_cert: Option<PathBuf>,
    /// The private key of --tls-cert, in PEM.
    #[arg(long, value_name = "FILE", requires = "tls_cert")]
    tls_key: Option<PathBuf>,
}

#[derive(Args)]
struct DaemonArgs {
    /// The daemon's socket (default: $XDG_RUNTIME_DIR/hopsync/hopsync.sock).
    #[arg(long, value_name = "FILE")]
    socket: Option<PathBuf>,
}

#[derive(Clone, Copy, ValueEnum)]
enum DirectionArg {
    Push,
    Pull,
}

#[derive(Clone, Copy, ValueEnum)]
enum JobAction {
    Show,
    Pause,
    Resume,
    Cancel,
    Rm,
}

#[derive(Clone, Subcommand)]
enum HostsCmd {
    /// Fetch a host's key, show its fingerprint, and pin it after confirmation.
    Trust {
        host: String,
        #[arg(short, long, default_value_t = 22)]
        port: u16,
        /// Pin without prompting, only if the offered key has exactly this SHA256 fingerprint.
        #[arg(long, value_name = "SHA256:...")]
        fingerprint: Option<String>,
    },
    /// List pinned keys.
    List,
    /// Remove every pin for a host.
    Remove {
        host: String,
        #[arg(short, long, default_value_t = 22)]
        port: u16,
    },
}

#[derive(Args)]
struct TransferArgs {
    #[arg(short, long, default_value_t = 22)]
    port: u16,
    /// Private key (default: ~/.ssh/id_ed25519, then ~/.ssh/id_ecdsa).
    #[arg(short = 'i', long, value_name = "FILE")]
    identity: Option<PathBuf>,
    /// Parallel SSH connections, fixed. Without it, transfers start at 4 and add one every few
    /// seconds while each raises throughput by 10 %, up to --max-lanes.
    #[arg(short, long, value_parser = clap::value_parser!(u32).range(1..=64))]
    lanes: Option<u32>,
    /// Ceiling for the automatic lane count.
    #[arg(long, default_value_t = 16, value_parser = clap::value_parser!(u32).range(1..=64))]
    max_lanes: u32,
    /// What to do with destination files that already exist: skip (default; a single file then
    /// errors), update (replace if size or mtime differ, keep newer ones), overwrite, or rename
    /// (write "name (1).ext" beside it).
    #[arg(long, value_enum, default_value_t = ExistingArg::Skip)]
    existing: ExistingArg,
    /// Same as --existing overwrite.
    #[arg(long, conflicts_with = "existing")]
    overwrite: bool,
    /// Space to leave free on the destination, e.g. 10G. Files that would cut into it fail before
    /// any of their data moves.
    #[arg(long, value_name = "SIZE", value_parser = parse_size, default_value = "0")]
    min_free: u64,
    /// Page cache handling: drop (default) keeps a seedbox's cache for its own work; keep leaves
    /// transferred data cached.
    #[arg(long, value_enum, default_value_t = Cache::Drop)]
    cache: Cache,
    /// Do not record or use the resume journal: interrupted files start over.
    #[arg(long)]
    no_resume: bool,
    /// Read every file back from the destination and compare it with the source (xxh3-128) before
    /// it is renamed into place. Costs a second read at the destination; a push downloads it again.
    #[arg(long)]
    verify: bool,
}

#[derive(Clone, Copy, ValueEnum)]
enum ExistingArg {
    Skip,
    Update,
    Overwrite,
    Rename,
}

#[derive(Clone, Copy, ValueEnum)]
enum Cache {
    Drop,
    Keep,
}

type Error = Box<dyn std::error::Error>;

fn main() -> ExitCode {
    let cli = Cli::parse();
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(usize::from(cli.threads))
        .enable_all()
        .build();
    let result = match rt {
        Ok(rt) => rt.block_on(run(cli)),
        Err(e) => Err(e.into()),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            // Errors carry text the server chose (status messages, names): never raw to a terminal.
            eprintln!(
                "hopsync: error: {}",
                hopsync::cli::printable_lines(&e.to_string())
            );
            ExitCode::FAILURE
        }
    }
}

async fn run(cli: Cli) -> Result<(), Error> {
    let known_path = match cli.known_hosts {
        Some(p) => p,
        None => hostkeys::default_path()
            .ok_or("cannot locate the config directory; pass --known-hosts")?,
    };
    let journal = |t: &TransferArgs| -> Result<Option<Arc<Journal>>, Error> {
        if t.no_resume {
            return Ok(None);
        }
        let path = match &cli.journal {
            Some(p) => p.clone(),
            None => Journal::default_path()
                .ok_or("cannot locate the state directory; pass --journal or --no-resume")?,
        };
        let j = Journal::open(&path)
            .map_err(|e| format!("{}: {e} (pass --no-resume to go without)", path.display()))?;
        Ok(Some(Arc::new(j)))
    };
    match &cli.cmd {
        Cmd::Push { local, remote, t } => {
            let o = Opts::new(t, journal(t)?);
            push(&known_path, local, remote, t, o).await
        }
        Cmd::Pull { remote, local, t } => {
            let o = Opts::new(t, journal(t)?);
            pull(&known_path, remote, local, t, o).await
        }
        Cmd::Hosts { cmd } => hosts(&known_path, cmd.clone()).await,
        Cmd::Serve(a) => serve(a).await,
        Cmd::Setup {
            folder,
            full_access,
            folder_only,
            port,
            public,
            local_only,
            tls_cert,
            tls_key,
            password_stdin,
            plain,
        } => {
            let o = hopsync::setup::SetupOptions {
                folder: folder.clone(),
                full_access: match (*full_access, *folder_only) {
                    (true, _) => Some(true),
                    (_, true) => Some(false),
                    _ => None,
                },
                port: *port,
                public: match (*public || tls_cert.is_some(), *local_only) {
                    (true, _) => Some(true),
                    (_, true) => Some(false),
                    _ => None,
                },
                own_cert: tls_cert.clone().zip(tls_key.clone()),
                password_stdin: *password_stdin,
                plain: *plain,
            };
            on_blocking_thread(move || hopsync::setup::run_setup(&o)).await
        }
        Cmd::Status => control(hopsync::setup::Control::Status).await,
        Cmd::Start => control(hopsync::setup::Control::Start).await,
        Cmd::Stop => control(hopsync::setup::Control::Stop).await,
        Cmd::Restart => control(hopsync::setup::Control::Restart).await,
        Cmd::Logs { no_follow } => {
            control(hopsync::setup::Control::Logs {
                follow: !*no_follow,
            })
            .await
        }
        Cmd::Uninstall { purge, yes, plain } => {
            let (purge, yes, plain) = (*purge, *yes, *plain);
            on_blocking_thread(move || hopsync::setup::run_uninstall(purge, yes, plain)).await
        }
        Cmd::Jobs { state, d } => jobs(d, state.as_deref()).await,
        Cmd::Queue {
            direction,
            server,
            src,
            dst,
            existing,
            verify,
            lanes,
            dry_run,
            d,
        } => {
            let c = daemon_client(d)?;
            let servers = c.get("/api/servers").await?.ok()?;
            let id = servers
                .as_array()
                .and_then(|a| a.iter().find(|s| s["name"] == server.as_str()))
                .and_then(|s| s["id"].as_i64())
                .ok_or_else(|| format!("no server named {server:?}; see `hopsync servers`"))?;
            let body = serde_json::json!({
                "direction": match direction { DirectionArg::Push => "push", DirectionArg::Pull => "pull" },
                "server": id,
                "src": src,
                "dst": dst,
                "existing": match existing {
                    ExistingArg::Skip => "skip",
                    ExistingArg::Update => "update",
                    ExistingArg::Overwrite => "overwrite",
                    ExistingArg::Rename => "rename",
                },
                "verify": verify,
                "lanes": lanes,
            });
            if *dry_run {
                let plan = c.post("/api/jobs/dry-run", body).await?.ok()?;
                for i in plan["items"].as_array().into_iter().flatten() {
                    println!(
                        "{:9} {:>12}  {}  {}",
                        str_of(&i["outcome"]),
                        i["bytes"].as_u64().unwrap_or(0),
                        printable(str_of(&i["path"])),
                        printable(str_of(&i["detail"]))
                    );
                }
                println!(
                    "would send {} files, {} bytes; {} problems{}",
                    plan["files"],
                    plan["bytes"],
                    plan["problems"],
                    if plan["truncated"] == true {
                        " (list truncated)"
                    } else {
                        ""
                    }
                );
            } else {
                let job = c.post("/api/jobs", body).await?.ok()?;
                println!("queued job {}", job["id"]);
            }
            Ok(())
        }
        Cmd::Job { action, id, d } => {
            let c = daemon_client(d)?;
            let path = format!("/api/jobs/{id}");
            match action {
                JobAction::Show => {
                    let j = c.get(&path).await?.ok()?;
                    print_job(&j);
                    if let Some(e) = j["error"].as_str() {
                        println!("  error: {}", printable(e));
                    }
                }
                JobAction::Rm => {
                    c.delete(&path).await?.ok()?;
                }
                JobAction::Pause | JobAction::Resume | JobAction::Cancel => {
                    let verb = match action {
                        JobAction::Pause => "pause",
                        JobAction::Resume => "resume",
                        _ => "cancel",
                    };
                    c.post(&format!("{path}/{verb}"), serde_json::json!({}))
                        .await?
                        .ok()?;
                }
            }
            Ok(())
        }
        Cmd::Servers { d } => {
            let c = daemon_client(d)?;
            for s in c
                .get("/api/servers")
                .await?
                .ok()?
                .as_array()
                .into_iter()
                .flatten()
            {
                println!(
                    "{:4} {:20} {}@{}:{} {}{}",
                    s["id"],
                    printable(str_of(&s["name"])),
                    printable(str_of(&s["user"])),
                    printable(str_of(&s["host"])),
                    s["port"],
                    printable(str_of(&s["root"])),
                    match s["blocked"].as_str() {
                        Some(b) => format!("  BLOCKED: {}", printable(b)),
                        None => String::new(),
                    }
                );
            }
            Ok(())
        }
    }
}

fn str_of(v: &serde_json::Value) -> &str {
    v.as_str().unwrap_or("")
}

fn print_job(j: &serde_json::Value) {
    println!(
        "{:5} {:9} {} {} {} -> {}  {} files, {} bytes, {} problems",
        j["id"],
        str_of(&j["state"]),
        str_of(&j["direction"]),
        j["server"],
        printable(str_of(&j["src"])),
        printable(str_of(&j["dst"])),
        j["files"],
        j["bytes"],
        j["problems"]
    );
}

fn data_dir(given: Option<&PathBuf>) -> Result<PathBuf, Error> {
    match given {
        Some(p) => Ok(p.clone()),
        None => hopsync::daemon::default_data_dir()
            .ok_or_else(|| "cannot locate the data directory; pass --data-dir".into()),
    }
}

fn daemon_client(d: &DaemonArgs) -> Result<hopsync::client::Client, Error> {
    let socket = match &d.socket {
        Some(s) => s.clone(),
        None => hopsync::daemon::default_socket(&data_dir(None)?),
    };
    Ok(hopsync::client::Client::new(
        hopsync::client::Endpoint::Unix(socket),
    ))
}

async fn jobs(d: &DaemonArgs, state: Option<&str>) -> Result<(), Error> {
    let c = daemon_client(d)?;
    let path = match state {
        Some(s) => format!("/api/jobs?state={s}"),
        None => "/api/jobs".to_owned(),
    };
    for j in c.get(&path).await?.ok()?.as_array().into_iter().flatten() {
        print_job(j);
    }
    Ok(())
}

/// Runs terminal work off the async threads, and puts the cursor back if Ctrl-C ends it.
async fn on_blocking_thread(
    f: impl FnOnce() -> Result<(), String> + Send + 'static,
) -> Result<(), Error> {
    tokio::spawn(async {
        if tokio::signal::ctrl_c().await.is_ok() {
            hopsync::ui::restore_terminal();
            std::process::exit(130);
        }
    });
    tokio::task::spawn_blocking(f).await??;
    Ok(())
}

async fn control(what: hopsync::setup::Control) -> Result<(), Error> {
    on_blocking_thread(move || hopsync::setup::control(what)).await
}

async fn serve(a: &ServeArgs) -> Result<(), Error> {
    let data_dir = data_dir(a.data_dir.as_ref())?;
    let root = match &a.root {
        Some(r) => r.clone(),
        None => std::env::var_os("HOME")
            .map(PathBuf::from)
            .ok_or("cannot locate the home directory; pass --root")?,
    };
    let socket = a
        .socket
        .clone()
        .unwrap_or_else(|| hopsync::daemon::default_socket(&data_dir));
    let tls = match (&a.tls_cert, &a.tls_key) {
        (Some(cert), Some(key)) => Some(hopsync::tls::Source::Files {
            cert: cert.clone(),
            key: key.clone(),
        }),
        _ => a.tls.then(|| hopsync::tls::Source::Generated {
            data_dir: data_dir.clone(),
            names: a.tls_name.clone(),
        }),
    };
    if a.full_access {
        eprintln!(
            "hopsync: FULL ACCESS: the web UI reads and writes everything under {}. Anyone with its \
             password controls that much.",
            root.display()
        );
    }
    hopsync::daemon::run(hopsync::daemon::DaemonConfig {
        tls,
        listen: a.listen,
        socket,
        service: hopsync::service::ServiceConfig {
            data_dir,
            hidden: if a.full_access {
                hopsync::paths::Hidden::for_full_access(&root)
            } else {
                hopsync::paths::Hidden::Nothing
            },
            local_root: root,
            concurrency: usize::from(a.jobs),
            ssh: hopsync::ssh::SshOptions::default(),
        },
        secure_cookies: a.secure_cookies,
    })
    .await?;
    Ok(())
}

async fn hosts(known_path: &Path, cmd: HostsCmd) -> Result<(), Error> {
    let mut known = KnownHosts::load(known_path)?;
    match cmd {
        HostsCmd::Trust {
            host,
            port,
            fingerprint,
        } => {
            let id = HostId::new(&host, port)?;
            let outcome = hostkeys::trust(&mut known, &id, |h, fp, alg| {
                // The server names the algorithm; the user decides on what is printed here.
                let alg = printable(alg.as_ref());
                match &fingerprint {
                    Some(want) => {
                        if want != fp {
                            eprintln!("{h} offers {alg} key {fp}, which does not match {want}");
                        }
                        want == fp
                    }
                    None => confirm(&format!("{h} offers {alg} key {fp}\nPin this key? [y/N] ")),
                }
            })
            .await?;
            match outcome {
                TrustOutcome::AlreadyTrusted => println!("{id}: key already pinned"),
                TrustOutcome::Pinned(fp) => println!("{id}: pinned {fp}"),
                TrustOutcome::Declined => return Err(format!("{id}: not pinned").into()),
            }
        }
        HostsCmd::List => {
            for (h, k) in known.iter() {
                println!("{h} {} {}", k.algorithm(), hostkeys::fingerprint(k));
            }
        }
        HostsCmd::Remove { host, port } => {
            let id = HostId::new(&host, port)?;
            let n = known.unpin(&id)?;
            println!("{id}: removed {n} pin(s)");
        }
    }
    Ok(())
}

/// Asks on the terminal. Anything but y/yes, or no terminal at all, is a no.
fn confirm(prompt: &str) -> bool {
    if !std::io::stdin().is_terminal() {
        eprintln!(
            "{prompt}\nno terminal to confirm on; pass --fingerprint to pin non-interactively"
        );
        return false;
    }
    eprint!("{prompt}");
    let _ = std::io::stderr().flush();
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line).is_ok()
        && matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}

fn target(spec: &RemoteSpec, t: &TransferArgs) -> Result<SshTarget, Error> {
    let user = match &spec.user {
        Some(u) => u.clone(),
        None => std::env::var("USER").map_err(|_| "no user given and $USER is not set")?,
    };
    let key_file = match &t.identity {
        Some(k) => k.clone(),
        None => {
            let home = std::env::var_os("HOME")
                .map(PathBuf::from)
                .ok_or("$HOME is not set; pass -i")?;
            ["id_ed25519", "id_ecdsa"]
                .iter()
                .map(|n| home.join(".ssh").join(n))
                .find(|p| p.exists())
                .ok_or("no ~/.ssh/id_ed25519 or id_ecdsa; pass -i")?
        }
    };
    Ok(SshTarget {
        host: HostId::new(&spec.host, t.port)?,
        user,
        key: KeySource::File(key_file),
    })
}

/// Transfer options plus the byte counter the progress line and summary read.
struct Opts {
    transfer: TransferOptions,
    done: Arc<AtomicU64>,
}

impl Opts {
    fn new(t: &TransferArgs, journal: Option<Arc<Journal>>) -> Self {
        let done = Arc::new(AtomicU64::new(0));
        let counter = done.clone();
        let transfer = TransferOptions {
            lanes: t.lanes.unwrap_or(4).min(t.max_lanes),
            max_lanes: t.lanes.unwrap_or(t.max_lanes),
            existing: match (t.overwrite, t.existing) {
                (true, _) | (_, ExistingArg::Overwrite) => Existing::Overwrite,
                (false, ExistingArg::Skip) => Existing::Skip,
                (false, ExistingArg::Update) => Existing::Update,
                (false, ExistingArg::Rename) => Existing::Rename,
            },
            min_free: t.min_free,
            cache: match t.cache {
                Cache::Drop => CachePolicy::Drop,
                Cache::Keep => CachePolicy::Keep,
            },
            progress: Some(Arc::new(move |n| {
                counter.fetch_add(n, Ordering::Relaxed);
            })),
            journal,
            verify: t.verify,
            ..TransferOptions::default()
        };
        Self { transfer, done }
    }
}

fn utf8_name(name: &std::ffi::OsStr) -> Result<Name, Error> {
    let s = name
        .to_str()
        .ok_or("non-UTF-8 file names are not supported yet")?;
    Ok(Name::new(s)?)
}

/// Splits a local path into (directory to use as root, file name). `None` name: path is a directory.
fn split_local(path: &Path) -> Result<(PathBuf, Option<Name>), Error> {
    if path.as_os_str().to_string_lossy().ends_with('/') || path.is_dir() {
        return Ok((path.to_owned(), None));
    }
    let name = path.file_name().ok_or("path has no file name")?;
    let parent = match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p.to_owned(),
        _ => PathBuf::from("."),
    };
    Ok((parent, Some(utf8_name(name)?)))
}

async fn push(
    known_path: &Path,
    local: &Path,
    remote: &str,
    t: &TransferArgs,
    o: Opts,
) -> Result<(), Error> {
    let spec = RemoteSpec::parse(remote)?;
    let target = target(&spec, t)?;
    let known = KnownHosts::load(known_path)?;
    let done = o.done.clone();

    // The path the user typed is theirs to choose, so a symlink there is followed.
    if std::fs::metadata(local).is_ok_and(|m| m.is_dir()) {
        let name = tree_name(local)?;
        let root = LocalRoot::open(local).map_err(|e| format!("{}: {e}", local.display()))?;
        let (parent, mut dst) = split_slash_path(&spec.path);
        let mut remote = Remote::new(target, known, RemoteRoot::parse(&parent)?);
        if dst.is_empty() || is_dir(&remote, &spec.path, &o.transfer).await? {
            remote = remote.with_root(RemoteRoot::parse(&spec.path)?);
            dst = name.as_str().to_owned();
        }
        let report = with_progress(
            &done,
            engine::push_tree(
                &root,
                &RelPath::default(),
                &remote,
                &RelPath::default().join(Name::new(&dst)?),
                &o.transfer,
            ),
        )
        .await?;
        return tree_summary("pushed", &report);
    }

    let (dir, name) = split_local(local)?;
    let name = name.ok_or("not a file or directory")?;
    let root = LocalRoot::open(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let (parent, mut dst) = split_slash_path(&spec.path);
    let mut remote = Remote::new(target, known, RemoteRoot::parse(&parent)?);
    if !dst.is_empty() && is_dir(&remote, &spec.path, &o.transfer).await? {
        remote = remote.with_root(RemoteRoot::parse(&spec.path)?);
        dst = String::new();
    }
    let dst = if dst.is_empty() {
        name.clone()
    } else {
        Name::new(&dst)?
    };
    let report = with_progress(
        &done,
        engine::push_file(
            &root,
            &RelPath::default().join(name),
            &remote,
            &RelPath::default().join(dst),
            &o.transfer,
        ),
    )
    .await?;
    summary("pushed", &report, &done);
    Ok(())
}

async fn pull(
    known_path: &Path,
    remote: &str,
    local: &Path,
    t: &TransferArgs,
    o: Opts,
) -> Result<(), Error> {
    let spec = RemoteSpec::parse(remote)?;
    let path = match spec.path.trim_end_matches('/') {
        "" if spec.path.starts_with('/') => "/",
        p => p,
    };
    let (parent, src) = split_slash_path(path);
    if src.is_empty() {
        return Err("name a remote file or directory to pull".into());
    }
    let src = Name::new(&src)?;
    let target = target(&spec, t)?;
    let known = KnownHosts::load(known_path)?;
    let remote = Remote::new(target, known, RemoteRoot::parse(&parent)?);
    let is_tree = is_dir(&remote, path, &o.transfer).await?;
    let (dir, name) = split_local(local)?;
    let dst = name.unwrap_or_else(|| src.clone());
    let root = LocalRoot::open(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let (src, dst) = (RelPath::default().join(src), RelPath::default().join(dst));

    let done = o.done.clone();
    let o = &o.transfer;
    if is_tree {
        let report = with_progress(&done, engine::pull_tree(&remote, &src, &root, &dst, o)).await?;
        return tree_summary("pulled", &report);
    }
    let report = with_progress(&done, engine::pull_file(&remote, &src, &root, &dst, o)).await?;
    summary("pulled", &report, &done);
    Ok(())
}

/// The name a pushed directory gets on the other side: its own (`.` resolves to the real name).
fn tree_name(dir: &Path) -> Result<Name, Error> {
    let real = std::fs::canonicalize(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    utf8_name(real.file_name().ok_or("cannot push the filesystem root")?)
}

/// Prints skipped and failed entries (control characters escaped, so a file name cannot rewrite
/// the terminal) and a summary. Failures make the command fail; skips alone do not.
fn tree_summary(verb: &str, r: &engine::TreeReport) -> Result<(), Error> {
    let mut failed = 0;
    for (path, p) in &r.problems {
        if matches!(p, engine::Problem::Failed(_)) {
            failed += 1;
        }
        eprintln!("{}: {}", printable(path), printable(&p.to_string()));
    }
    print!(
        "{verb} {} files ({} bytes), {} directories",
        r.files, r.bytes, r.dirs
    );
    if r.unchanged > 0 {
        print!(", {} unchanged", r.unchanged);
    }
    if r.resumed > 0 {
        print!(", resumed {:.1} MiB", mib(r.resumed));
    }
    if r.verified > 0 {
        print!(", {} bytes verified", r.verified);
    }
    if r.retries > 0 {
        print!(", {} reconnects", r.retries);
    }
    print!(", {} lanes", r.lanes);
    println!();
    match failed {
        0 => Ok(()),
        1 => Err("1 entry failed".into()),
        n => Err(format!("{n} entries failed").into()),
    }
}

/// Whether the remote path is a directory. The connection this opens becomes the transfer's first
/// lane.
async fn is_dir(remote: &Remote, path: &str, o: &TransferOptions) -> Result<bool, Error> {
    Ok(remote.kind(path, o).await? == Some(RemoteKind::Dir))
}

/// Runs `fut`, printing a progress line to stderr once a second when it is a terminal.
async fn with_progress<T>(done: &Arc<AtomicU64>, fut: impl Future<Output = T>) -> T {
    if !std::io::stderr().is_terminal() {
        return fut.await;
    }
    let ticker = {
        let done = done.clone();
        tokio::spawn(async move {
            let start = Instant::now();
            let mut tick = tokio::time::interval(Duration::from_secs(1));
            tick.tick().await;
            loop {
                tick.tick().await;
                let (n, secs) = (done.load(Ordering::Relaxed), start.elapsed().as_secs_f64());
                eprint!("\r{:>10.1} MiB  {:>8.1} MiB/s", mib(n), mib(n) / secs);
            }
        })
    };
    let out = fut.await;
    ticker.abort();
    eprint!("\r\x1b[2K");
    out
}

fn summary(verb: &str, r: &engine::FileReport, done: &AtomicU64) {
    if r.unchanged {
        println!(
            "{}: unchanged (same size and mtime), nothing sent",
            printable(&r.dst)
        );
        return;
    }
    print!("{verb} {} bytes to {}", r.bytes, printable(&r.dst));
    if r.resumed > 0 {
        print!(" (resumed {:.1} MiB from an earlier run)", mib(r.resumed));
    }
    if r.verified > 0 {
        print!(", verified");
    }
    print!(" over {} lanes", r.lanes);
    if r.retries > 0 {
        let resent = done
            .load(Ordering::Relaxed)
            .saturating_sub(r.bytes - r.resumed);
        print!(" ({} reconnects, {:.1} MiB resent)", r.retries, mib(resent));
    }
    println!();
}

fn mib(n: u64) -> f64 {
    n as f64 / f64::from(1 << 20)
}
