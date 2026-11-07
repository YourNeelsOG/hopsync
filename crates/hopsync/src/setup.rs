//! `hopsync setup` and the commands around it: install the service, set the admin password, and
//! start, stop, inspect or remove it, on any Linux box, with or without root.
//!
//! Three ways to run, chosen from what the box allows:
//! - System: root with systemd. A hardened system unit that runs as an unprivileged user.
//! - User: no root, but `systemctl --user` works. The hardened user unit.
//! - Background: no root and no user manager, as on many shared seedboxes. A detached process with a
//!   pid file, and a `@reboot` cron line where cron is allowed.
//!
//! Setup adds no way to set a password. It starts the daemon, then calls the same `/api/setup` the
//! web page uses, with the token only this account can read. First it checks in `/proc/net/tcp` that
//! the port belongs to the service's account: on a shared box, another customer could be listening
//! there to collect the password. That call stays on loopback even when the web UI is public, where
//! the listener still takes plain HTTP from this machine. All commands here use fixed arguments and no shell, and every path
//! that reaches a unit file or crontab passes `check_path_text`.

use std::io::{BufRead, IsTerminal, Write};
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, TcpListener};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

use crate::auth::check_password_rules;
use crate::client::{Client, Endpoint};
use crate::settings::Settings;
use crate::ui::Ui;

const SYSTEM_TEMPLATE: &str = include_str!("../../../contrib/systemd/hopsync.service");
const USER_TEMPLATE: &str = include_str!("../../../contrib/systemd/user/hopsync.service");
const FULL_TEMPLATE: &str = include_str!("../../../contrib/systemd/hopsync-full.service");
const SYSTEM_SETTINGS: &str = "/etc/hopsync/config.toml";
const MARKER: &str =
    "# Managed by hopsync setup. The next setup replaces edits; `hopsync uninstall` removes it.";
const SYSTEM_UNIT: &str = "/etc/systemd/system/hopsync.service";
const SYSTEM_HOME: &str = "/var/lib/hopsync";
const SYSTEM_DATA: &str = "/var/lib/hopsync/data";
const SYSTEM_BIN: &str = "/usr/local/bin/hopsync";
const SYSTEM_FOLDER: &str = "/srv/hopsync";
const SERVICE_USER: &str = "hopsync";
const DEFAULT_PORT: u16 = 7890;
/// How far past the default port setup looks for a free one.
const PORT_TRIES: u16 = 100;
const CRON_TAG: &str = "# hopsync-managed";
const LOG_ROTATE_BYTES: u64 = 10 << 20;
const START_TIMEOUT: Duration = Duration::from_secs(25);
const STOP_TIMEOUT: Duration = Duration::from_secs(35);
/// Folders the service must never be given: it would have read and write access to the whole tree.
const FORBIDDEN: [&str; 14] = [
    "/",
    "/bin",
    "/boot",
    "/dev",
    "/etc",
    "/lib",
    "/lib64",
    "/proc",
    "/root",
    "/run",
    "/sbin",
    "/sys",
    "/usr",
    SYSTEM_HOME,
];

type Fallible<T = ()> = Result<T, String>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    System,
    User,
    Background,
}

/// Picks how to run from what the box allows.
pub fn choose_mode(root: bool, systemd_booted: bool, user_manager: bool) -> Fallible<Mode> {
    match (root, systemd_booted, user_manager) {
        (true, true, _) => Ok(Mode::System),
        (true, false, _) => Err(
            "this box has no systemd, and Hopsync will not run as root without a \
             service to drop its privileges. Run `hopsync setup` as a normal user: it then runs in \
             the background without one."
                .into(),
        ),
        (false, _, true) => Ok(Mode::User),
        (false, _, false) => Ok(Mode::Background),
    }
}

/// Unit files and crontabs give meaning to spaces, `%`, `$`, quotes and more. Rather than escape
/// for each of them, only characters that mean nothing to any of them are allowed.
pub fn check_path_text(path: &str) -> Fallible<()> {
    let safe = |c: char| c.is_ascii_alphanumeric() || "._-/+@:,=".contains(c);
    match path.chars().find(|c| !safe(*c)) {
        None => Ok(()),
        Some(c) => Err(format!(
            "{path:?} has the character {c:?}. Paths here may use letters, digits and . _ - / + @ : , = only. \
             Spaces are not allowed: a symlink without spaces can point to the folder."
        )),
    }
}

/// Checks a transfer folder as typed: absolute, plain, and not a system folder.
pub fn check_folder_text(folder: &str) -> Fallible<PathBuf> {
    check_path_text(folder)?;
    let path = Path::new(folder);
    if !path.is_absolute() {
        return Err(format!("{folder}: the folder must be an absolute path"));
    }
    if path.components().any(|c| c == Component::ParentDir) {
        return Err(format!("{folder}: the folder must not contain .."));
    }
    let clean: PathBuf = path.components().collect();
    if FORBIDDEN.iter().any(|f| clean == Path::new(f)) {
        return Err(format!(
            "{folder} is a system folder, and Hopsync could read and write everything under it. \
             Choose a folder for transfers. To open the whole box to the web UI instead, run \
             `hopsync setup --full-access`."
        ));
    }
    Ok(clean)
}

/// Refuses a folder that would give the web UI the box's keys or Hopsync's own secrets. Writing to
/// `~/.ssh/authorized_keys` or `~/.bashrc` is running commands, which no login may lead to, so a home
/// is refused for the same reason as `/root`. A subfolder of it is fine.
pub fn check_folder_safe(folder: &Path, data_dir: &Path, home: &Path) -> Fallible<()> {
    let holds_keys = folder.join(".ssh").exists();
    let inside_keys = folder.components().any(|c| c.as_os_str() == ".ssh");
    if holds_keys
        || inside_keys
        || home.starts_with(folder)
        || data_dir.starts_with(folder)
        || folder.starts_with(data_dir)
    {
        return Err(format!(
            "{} holds SSH keys or Hopsync's own secrets, and anyone logged in to the web UI could \
             overwrite them, which gives a shell on this box. Choose a subfolder, such as \
             {}/downloads.",
            folder.display(),
            folder.display()
        ));
    }
    Ok(())
}

/// `~` and `~/x` as a shell would read them, since a prompt has no shell to do it.
fn expand_tilde(text: &str, home: &Path) -> String {
    match text.strip_prefix('~') {
        Some("") => home.display().to_string(),
        Some(rest) if rest.starts_with('/') => format!("{}{rest}", home.display()),
        _ => text.to_owned(),
    }
}

/// Who a system service runs as, from who owns the transfer folder.
pub fn service_owner(folder: &Path, owner: Option<&str>) -> Fallible<String> {
    match owner {
        None => Ok(SERVICE_USER.to_owned()),
        Some("root") => Err(format!(
            "{f} belongs to root, and Hopsync never runs as root. To let the service use it, run \
             `chown -R {SERVICE_USER}: {f}`, then run setup again. Or choose another folder.",
            f = folder.display()
        )),
        Some(user) => Ok(user.to_owned()),
    }
}

#[derive(Debug, Clone)]
pub struct UnitSpec {
    /// The root the daemon gets: the chosen folder, or the home folder with full access.
    pub folder: PathBuf,
    pub full_access: bool,
    pub port: u16,
    pub public: Option<Public>,
    /// System only: the account and its primary group.
    pub user: String,
    pub group: String,
    /// User only: the binary and the data directory.
    pub exec: PathBuf,
    pub data_dir: PathBuf,
}

/// The web UI on this server's own address, over HTTPS.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Public {
    /// The address people open, and the name in a generated certificate.
    pub address: String,
    /// The user's own certificate and key; a generated certificate when `None`.
    #[serde(default)]
    pub own_cert: Option<(String, String)>,
}

/// The daemon arguments that decide where it listens and how.
pub fn listen_args(port: u16, public: Option<&Public>) -> Fallible<Vec<String>> {
    let Some(p) = public else {
        return Ok(vec!["--listen".into(), format!("127.0.0.1:{port}")]);
    };
    let mut args = vec!["--listen".into(), format!("0.0.0.0:{port}"), "--tls".into()];
    match &p.own_cert {
        Some((cert, key)) => {
            check_path_text(cert)?;
            check_path_text(key)?;
            args.extend(["--tls-cert".into(), cert.clone()]);
            args.extend(["--tls-key".into(), key.clone()]);
        }
        None => {
            check_path_text(&p.address)?;
            args.extend(["--tls-name".into(), p.address.clone()]);
        }
    }
    Ok(args)
}

/// How a unit's daemon is exposed, read back from its command line.
pub fn parse_public(unit: &str) -> Option<Public> {
    let exec = unit.lines().find_map(|l| l.strip_prefix("ExecStart="))?;
    let words: Vec<&str> = exec.split_whitespace().collect();
    let after = |flag: &str| {
        words
            .iter()
            .position(|w| *w == flag)
            .and_then(|i| words.get(i + 1))
            .map(|v| (*v).to_owned())
    };
    if !words.contains(&"--tls") {
        return None;
    }
    let own_cert = after("--tls-cert").zip(after("--tls-key"));
    let address = after("--tls-name").or_else(|| own_cert.as_ref().map(|_| String::new()))?;
    Some(Public { address, own_cert })
}

/// The IPv4 address a person reached this box on. `SSH_CONNECTION` is the address that demonstrably
/// works from their side; IPv6 is left out because the public listener is IPv4 (Decision log).
pub fn address_from_ssh(ssh_connection: Option<&str>) -> Option<String> {
    let ip = ssh_connection?.split_whitespace().nth(2)?;
    let parsed: Ipv4Addr = ip.parse().ok()?;
    (!parsed.is_loopback()).then(|| parsed.to_string())
}

/// The address this box goes out on. Connecting a UDP socket sends nothing: it only picks a route.
fn address_from_route() -> Option<String> {
    let socket = std::net::UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).ok()?;
    socket.connect((Ipv4Addr::new(192, 0, 2, 1), 9)).ok()?;
    match socket.local_addr().ok()?.ip() {
        std::net::IpAddr::V4(ip) if !ip.is_loopback() && !ip.is_unspecified() => {
            Some(ip.to_string())
        }
        _ => None,
    }
}

/// The URL people open, with no port when `address` is empty because a certificate of their own
/// names the host and setup cannot know which of its names they use.
fn public_url(public: &Public, port: u16) -> String {
    let host = if public.address.is_empty() {
        "your-domain"
    } else {
        public.address.as_str()
    };
    format!("https://{host}:{port}")
}

/// Whether `ufw status` output shows a firewall that would block `port`.
pub fn ufw_blocks(status: &str, port: u16) -> bool {
    if !status.lines().any(|l| l.trim() == "Status: active") {
        return false;
    }
    let number = port.to_string();
    !status.lines().any(|l| {
        let rule = l.split_whitespace().next().unwrap_or("");
        l.contains("ALLOW") && (rule == number || rule == format!("{port}/tcp"))
    })
}

fn swap(text: &str, from: &str, to: &str) -> Fallible<String> {
    if !text.contains(from) {
        return Err(format!("the unit template no longer contains {from:?}"));
    }
    Ok(text.replace(from, to))
}

/// The template without its how-to-install comments, which setup has just done.
fn body(template: &str) -> &str {
    template.find("[Unit]").map_or(template, |i| &template[i..])
}

/// True for a folder the system unit's `ProtectHome=yes` would hide.
fn under_home(folder: &Path) -> bool {
    folder.starts_with("/home") || folder.starts_with("/root") || folder.starts_with("/run/user")
}

/// The system unit: the checked-in file with this box's values, so the two cannot drift apart.
pub fn render_system_unit(s: &UnitSpec) -> Fallible<String> {
    let folder = s.folder.to_string_lossy();
    check_path_text(&folder)?;
    check_path_text(&s.user)?;
    check_path_text(&s.group)?;
    let mut u = body(SYSTEM_TEMPLATE).to_owned();
    u = swap(&u, "User=hopsync", &format!("User={}", s.user))?;
    u = swap(&u, "Group=hopsync", &format!("Group={}", s.group))?;
    u = swap(
        &u,
        "--listen 127.0.0.1:7890",
        &listen_args(s.port, s.public.as_ref())?.join(" "),
    )?;
    u = swap(&u, "/srv/hopsync", &folder)?;
    if under_home(&s.folder) {
        // An empty /home with only this folder in it, rather than opening every home to the service.
        u = swap(
            &u,
            "ProtectHome=yes",
            &format!("ProtectHome=tmpfs\nBindPaths={folder}"),
        )?;
    }
    Ok(format!("{MARKER}\n{u}"))
}

/// The system unit for full access: root, over the whole box.
pub fn render_full_system_unit(s: &UnitSpec) -> Fallible<String> {
    let u = swap(
        body(FULL_TEMPLATE),
        "--listen 127.0.0.1:7890",
        &listen_args(s.port, s.public.as_ref())?.join(" "),
    )?;
    Ok(format!("{MARKER}\n{u}"))
}

/// Whether a unit gives the daemon full access.
pub fn unit_has_full_access(unit: &str) -> bool {
    unit.lines()
        .find_map(|l| l.strip_prefix("ExecStart="))
        .is_some_and(|e| e.split_whitespace().any(|w| w == "--full-access"))
}

/// The user unit, with the binary and data directory named outright: a user manager's environment
/// can differ from the login shell's, and the daemon must use the data directory setup reads from.
pub fn render_user_unit(s: &UnitSpec) -> Fallible<String> {
    let (folder, exec, data) = (
        s.folder.to_string_lossy(),
        s.exec.to_string_lossy(),
        s.data_dir.to_string_lossy(),
    );
    check_path_text(&folder)?;
    check_path_text(&exec)?;
    check_path_text(&data)?;
    let u = swap(
        body(USER_TEMPLATE),
        "%h/.local/bin/hopsync serve --listen 127.0.0.1:7890",
        &format!(
            "{exec} serve {} --data-dir {data} --root {folder}{}",
            listen_args(s.port, s.public.as_ref())?.join(" "),
            if s.full_access { " --full-access" } else { "" }
        ),
    )?;
    Ok(format!("{MARKER}\n{u}"))
}

/// The folder and port an earlier setup chose, read back from its unit.
pub fn parse_unit(unit: &str) -> Option<(PathBuf, u16)> {
    let exec = unit.lines().find_map(|l| l.strip_prefix("ExecStart="))?;
    let mut words = exec.split_whitespace();
    let (mut folder, mut port) = (None, None);
    while let Some(w) = words.next() {
        match w {
            "--root" => folder = words.next().map(PathBuf::from),
            "--listen" => {
                port = words
                    .next()
                    .and_then(|a| a.rsplit(':').next())
                    .and_then(|p| p.parse().ok());
            }
            _ => {}
        }
    }
    Some((folder?, port?))
}

/// The uid listening on a loopback or wildcard IPv4 `port`, from `/proc/net/tcp` text. Addresses
/// there are hex in the machine's byte order: every target Hopsync builds for is little-endian.
pub fn listener_uid(proc_net_tcp: &str, port: u16) -> Option<u32> {
    let want = format!("{port:04X}");
    proc_net_tcp.lines().skip(1).find_map(|line| {
        let f: Vec<&str> = line.split_whitespace().collect();
        let (addr, p) = f.get(1)?.split_once(':')?;
        let local = addr == "0100007F" || addr == "00000000";
        (local && p == want && *f.get(3)? == "0A")
            .then(|| f.get(7)?.parse().ok())
            .flatten()
    })
}

/// A crontab with this binary's `@reboot` line, once, and every other line untouched.
pub fn crontab_with(existing: &str, exec: &str) -> String {
    let mut out = crontab_without(existing);
    out.push_str(&format!(
        "@reboot {exec} start >/dev/null 2>&1 {CRON_TAG}\n"
    ));
    out
}

/// A crontab without the line setup added.
pub fn crontab_without(existing: &str) -> String {
    let mut out = String::new();
    for line in existing.lines().filter(|l| !l.ends_with(CRON_TAG)) {
        out.push_str(line);
        out.push('\n');
    }
    out
}

/// True when this argv is a Hopsync daemon using this data directory. A pid file can outlive its
/// process and the pid can go to something else: only a match may receive a signal.
pub fn is_our_daemon(cmdline: &[u8], data_dir: &Path) -> bool {
    let args: Vec<&[u8]> = cmdline.split(|b| *b == 0).collect();
    let name_ok = args
        .first()
        .and_then(|a| a.rsplit(|b| *b == b'/').next())
        .is_some_and(|a| a.starts_with(b"hopsync"));
    let data = data_dir.to_string_lossy();
    name_ok && args.get(1) == Some(&&b"serve"[..]) && args.contains(&data.as_bytes())
}

/// The command that opens a tunnel to the web UI from the reader's own machine.
pub fn tunnel_command(port: u16, user: Option<&str>, ssh_connection: Option<&str>) -> String {
    // SSH_CONNECTION is "client-ip client-port server-ip server-port".
    let host = ssh_connection
        .and_then(|c| c.split_whitespace().nth(2))
        .unwrap_or("your-server");
    let target = match user {
        Some(u) => format!("{u}@{host}"),
        None => host.to_owned(),
    };
    format!("ssh -L {port}:127.0.0.1:{port} {target}")
}

/// The everyday commands, as this mode spells them.
pub fn command_rows(mode: Mode) -> Vec<(&'static str, &'static str)> {
    match mode {
        Mode::System => vec![
            ("Check it", "systemctl status hopsync"),
            ("Logs", "journalctl -u hopsync -f"),
            ("Restart", "systemctl restart hopsync"),
            ("Stop", "systemctl stop hopsync"),
            ("Remove", "hopsync uninstall"),
        ],
        Mode::User => vec![
            ("Check it", "systemctl --user status hopsync"),
            ("Logs", "journalctl --user -u hopsync -f"),
            ("Restart", "systemctl --user restart hopsync"),
            ("Stop", "systemctl --user stop hopsync"),
            ("Remove", "hopsync uninstall"),
        ],
        Mode::Background => vec![
            ("Check it", "hopsync status"),
            ("Logs", "hopsync logs"),
            ("Restart", "hopsync restart"),
            ("Stop", "hopsync stop"),
            ("Remove", "hopsync uninstall"),
        ],
    }
}

/// What a unit write should do about a file already there.
#[derive(Debug, PartialEq, Eq)]
pub enum UnitChange {
    Create,
    Same,
    Replace,
    Foreign,
}

pub fn unit_change(existing: Option<&str>, wanted: &str) -> UnitChange {
    match existing {
        None => UnitChange::Create,
        Some(e) if e == wanted => UnitChange::Same,
        Some(e) if e.starts_with(MARKER) => UnitChange::Replace,
        Some(_) => UnitChange::Foreign,
    }
}

// Everything below touches the machine; contrib/release/setup-e2e.sh tests it in real containers.

fn is_root() -> bool {
    rustix::process::geteuid().is_root()
}

fn my_uid() -> u32 {
    rustix::process::geteuid().as_raw()
}

fn home() -> Fallible<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .ok_or_else(|| "HOME is not set".to_owned())
}

/// Runs a fixed command and returns its stdout, or its stderr as the error. Sbin folders are added
/// because `su` without `-l` and some sudo setups leave them off PATH, where useradd lives.
fn run(program: &str, args: &[&str]) -> Fallible<String> {
    let path = std::env::var("PATH").unwrap_or_default();
    let out = Command::new(program)
        .args(args)
        .env("PATH", format!("{path}:/usr/sbin:/sbin:/usr/bin:/bin"))
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("{program}: {e}"))?;
    if out.status.success() {
        return Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned());
    }
    let err = String::from_utf8_lossy(&out.stderr);
    Err(format!("{program} {}: {}", args.join(" "), err.trim()))
}

/// `systemctl` with `--user` when the mode calls for it.
fn systemctl(mode: Mode, args: &[&str]) -> Fallible<String> {
    let mut all = Vec::with_capacity(args.len() + 1);
    if mode == Mode::User {
        all.push("--user");
    }
    all.extend_from_slice(args);
    run("systemctl", &all)
}

fn service_active(mode: Mode) -> bool {
    systemctl(mode, &["is-active", "--quiet", "hopsync"]).is_ok()
}

fn detect_mode() -> Fallible<Mode> {
    let booted = Path::new("/run/systemd/system").is_dir();
    let user = !is_root() && booted && systemctl(Mode::User, &["show-environment"]).is_ok();
    choose_mode(is_root(), booted, user)
}

fn user_data_dir() -> Fallible<PathBuf> {
    crate::daemon::default_data_dir().ok_or_else(|| "cannot locate the data directory".to_owned())
}

fn user_unit_path() -> Fallible<PathBuf> {
    Ok(home()?.join(".config/systemd/user/hopsync.service"))
}

/// Writes a file so a reader sees the old or the new one, never half of either.
fn write_atomic(path: &Path, content: &[u8], mode: u32) -> Fallible<()> {
    let fail = |e: std::io::Error| format!("{}: {e}", path.display());
    let dir = path
        .parent()
        .ok_or_else(|| format!("{}: no folder", path.display()))?;
    std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let name = path
        .file_name()
        .map_or_else(String::new, |n| n.to_string_lossy().into_owned());
    let tmp = dir.join(format!(".{name}.new"));
    let _ = std::fs::remove_file(&tmp);
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .open(&tmp)
        .map_err(fail)?;
    f.write_all(content).map_err(fail)?;
    f.sync_all().map_err(fail)?;
    drop(f);
    // The mode given to open is reduced by the umask, so set it outright.
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(mode)).map_err(fail)?;
    std::fs::rename(&tmp, path).map_err(fail)
}

/// Writes a unit. Returns whether it changed, and refuses a file setup did not write.
fn write_unit(path: &Path, wanted: &str) -> Fallible<bool> {
    let existing = std::fs::read_to_string(path).ok();
    match unit_change(existing.as_deref(), wanted) {
        UnitChange::Same => Ok(false),
        UnitChange::Foreign => Err(format!(
            "{} already exists and setup did not write it. Move it away, then run setup again.",
            path.display()
        )),
        UnitChange::Create | UnitChange::Replace => {
            write_atomic(path, wanted.as_bytes(), 0o644)?;
            Ok(true)
        }
    }
}

/// Puts this binary at `target`, unless it is already there. Returns whether anything changed.
fn install_binary(target: &Path) -> Fallible<bool> {
    let me = std::env::current_exe().map_err(|e| format!("cannot find this binary: {e}"))?;
    let same_path = me.canonicalize().ok() == target.canonicalize().ok();
    if same_path {
        return Ok(false);
    }
    let bytes = std::fs::read(&me).map_err(|e| format!("{}: {e}", me.display()))?;
    if std::fs::read(target).is_ok_and(|old| old == bytes) {
        return Ok(false);
    }
    write_atomic(target, &bytes, 0o755)?;
    Ok(true)
}

fn tcp_table() -> String {
    std::fs::read_to_string("/proc/net/tcp").unwrap_or_default()
}

/// Free on every address: a holder of any one of them, on a shared box especially, blocks a public
/// listener, and a wildcard holder blocks a loopback one.
fn port_free(port: u16) -> bool {
    TcpListener::bind(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, port)).is_ok()
}

/// The port to use: the running service's, an explicit one that is free, or the first free one.
fn choose_port(explicit: Option<u16>, current: Option<u16>, running: bool) -> Fallible<u16> {
    if let Some(p) = explicit {
        if (running && current == Some(p)) || port_free(p) {
            return Ok(p);
        }
        return Err(format!(
            "port {p} is in use. Choose another with --port, or leave --port off to pick a free one."
        ));
    }
    if let (true, Some(p)) = (running, current) {
        return Ok(p);
    }
    let first = current.unwrap_or(DEFAULT_PORT);
    (first..=first.saturating_add(PORT_TRIES))
        .find(|p| port_free(*p))
        .ok_or_else(|| format!("no free port from {first}; choose one with --port"))
}

fn client(port: u16) -> Client {
    Client::new(Endpoint::Tcp(SocketAddr::V4(SocketAddrV4::new(
        Ipv4Addr::LOCALHOST,
        port,
    ))))
}

/// Waits until the daemon answers on `port` and the port is `uid`'s. Returns `setup_required`.
fn wait_for_daemon(port: u16, uid: u32, alive: &mut dyn FnMut() -> Fallible<()>) -> Fallible<bool> {
    let rt = tokio::runtime::Handle::current();
    let start = Instant::now();
    loop {
        alive()?;
        if listener_uid(&tcp_table(), port) == Some(uid)
            && let Ok(reply) = rt.block_on(client(port).get("/api/health"))
            && let Ok(body) = reply.ok()
        {
            return Ok(body["setup_required"].as_bool().unwrap_or(false));
        }
        if start.elapsed() > START_TIMEOUT {
            return Err(format!(
                "the daemon did not answer on port {port} within {} seconds",
                START_TIMEOUT.as_secs()
            ));
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Sets the admin password through the daemon's own setup call.
fn send_password(port: u16, uid: u32, data_dir: &Path, password: &str) -> Fallible<()> {
    if listener_uid(&tcp_table(), port) != Some(uid) {
        return Err(format!(
            "port {port} no longer belongs to the service's account, so the password was not sent"
        ));
    }
    let file = data_dir.join("setup-token");
    let token = Zeroizing::new(
        std::fs::read_to_string(&file).map_err(|e| format!("{}: {e}", file.display()))?,
    );
    let body = serde_json::json!({ "token": token.trim(), "password": password });
    let rt = tokio::runtime::Handle::current();
    rt.block_on(client(port).post("/api/setup", body))
        .and_then(crate::client::Reply::ok)
        .map(|_| ())
        .map_err(|e| format!("setting the password: {e}"))
}

/// Asks twice on a terminal until the password is acceptable. Ctrl-C is an error.
fn prompt_password(ui: &Ui) -> Fallible<Zeroizing<String>> {
    let min = crate::auth::MIN_PASSWORD;
    let cancelled =
        || "cancelled. Run `hopsync setup` again to finish: the service is waiting.".to_owned();
    loop {
        let first = ui
            .ask_password("Password", min)
            .map_err(|e| e.to_string())?
            .ok_or_else(cancelled)?;
        if let Err(e) = check_password_rules(&first) {
            ui.warn(&e.to_string());
            continue;
        }
        let again = ui
            .ask_password("Type it again", min)
            .map_err(|e| e.to_string())?
            .ok_or_else(cancelled)?;
        if *first == *again {
            return Ok(first);
        }
        ui.warn("The passwords differ. Try again.");
    }
}

fn stdin_line() -> Fallible<Zeroizing<String>> {
    let mut line = Zeroizing::new(String::new());
    std::io::stdin()
        .lock()
        .read_line(&mut line)
        .map_err(|e| format!("reading the password: {e}"))?;
    let trimmed = Zeroizing::new(line.trim_end_matches(['\r', '\n']).to_owned());
    Ok(trimmed)
}

#[derive(Debug, Clone, Default)]
pub struct SetupOptions {
    pub folder: Option<String>,
    /// `--full-access` or `--folder-only`; from the settings file or asked on a terminal when `None`.
    pub full_access: Option<bool>,
    pub port: Option<u16>,
    /// `--public` or `--local-only`; asked for on a terminal when `None`.
    pub public: Option<bool>,
    /// `--tls-cert` and `--tls-key`.
    pub own_cert: Option<(String, String)>,
    pub password_stdin: bool,
    pub plain: bool,
}

/// What the three modes differ in once the folder is known.
struct Started {
    public: Option<Public>,
    port: u16,
    uid: u32,
    data_dir: PathBuf,
    folder: PathBuf,
    full_access: bool,
    setup_required: bool,
}

/// `hopsync setup`. Safe to run again: it finishes what is missing, keeps the folder, port and
/// password, and restarts the service only if something about it changed.
pub fn run_setup(o: &SetupOptions) -> Fallible<()> {
    let ui = Ui::detect(o.plain);
    ui.banner("fast file transfer between SSH boxes");
    let mode = detect_mode()?;
    // Read early, so a bad password stops setup before it changes anything.
    let piped = if o.password_stdin {
        let p = stdin_line()?;
        if !p.is_empty() {
            check_password_rules(&p).map_err(|e| e.to_string())?;
        }
        Some(p)
    } else {
        None
    };
    let started = match mode {
        Mode::System => setup_system(&ui, o)?,
        Mode::User => setup_user(&ui, o)?,
        Mode::Background => setup_background(&ui, o)?,
    };

    if started.setup_required {
        let password = match piped.filter(|p| !p.is_empty()) {
            Some(p) => p,
            None if std::io::stdin().is_terminal() => {
                ui.heading(&format!(
                    "Web UI password, {} characters or more",
                    crate::auth::MIN_PASSWORD
                ));
                prompt_password(&ui)?
            }
            None => {
                return Err(
                    "the web UI has no password yet. Run setup on a terminal, or pass \
                     --password-stdin and pipe one in."
                        .into(),
                );
            }
        };
        ui.step("Securing the web UI", || {
            send_password(started.port, started.uid, &started.data_dir, &password)
        })?;
    } else {
        ui.skipped("Web UI password already set, keeping it");
    }

    warn_about_firewall(&ui, &started);
    summary(&ui, mode, &started);
    Ok(())
}

/// The summary lines for a public web UI, with the fingerprint the browser's warning must match.
fn public_rows(p: &Public, s: &Started, tunnel: String) -> Vec<(String, String)> {
    let mut rows = vec![("Open".to_owned(), public_url(p, s.port))];
    if p.own_cert.is_none() {
        let cert = s.data_dir.join("tls-cert.pem");
        rows.push((
            "Browser warns".to_owned(),
            "normal here. Accept only if SHA-256 matches:".to_owned(),
        ));
        rows.push((
            "SHA-256".to_owned(),
            crate::tls::fingerprint_of(&cert).unwrap_or_else(|e| e),
        ));
    }
    rows.push(("Or tunnel".to_owned(), tunnel));
    rows
}

/// A firewall this setup can see and will not change on its own.
fn warn_about_firewall(ui: &Ui, s: &Started) {
    if s.public.is_some()
        && is_root()
        && let Ok(status) = run("ufw", &["status"])
        && ufw_blocks(&status, s.port)
    {
        ui.warn(&format!(
            "ufw is blocking the web UI. To let it through, run: ufw allow {}/tcp",
            s.port
        ));
    }
}

fn summary(ui: &Ui, mode: Mode, s: &Started) {
    let user = std::env::var("SUDO_USER")
        .or_else(|_| std::env::var("USER"))
        .ok()
        .or_else(|| run("id", &["-un"]).ok());
    let ssh = std::env::var("SSH_CONNECTION").ok();
    let tunnel = tunnel_command(s.port, user.as_deref(), ssh.as_deref());
    let mut rows = match &s.public {
        Some(p) => public_rows(p, s, tunnel),
        None => vec![
            ("On your PC".to_owned(), tunnel),
            (
                "Then open".to_owned(),
                format!("http://127.0.0.1:{}", s.port),
            ),
        ],
    };
    if s.full_access {
        let reach = match mode {
            Mode::System => "the whole box, as root".to_owned(),
            _ => format!("{}, except hidden items", s.folder.display()),
        };
        rows.push(("Web UI reaches".to_owned(), reach));
    } else {
        rows.push(("Files folder".to_owned(), s.folder.display().to_string()));
    }
    if let Ok(path) = settings_path(mode) {
        rows.push(("Settings file".to_owned(), path.display().to_string()));
        // `systemctl restart` alone keeps the old unit, which decides the account and the access.
        rows.push(("Apply a change".to_owned(), "hopsync restart".to_owned()));
    }
    rows.extend(
        command_rows(mode)
            .into_iter()
            .map(|(l, v)| (l.to_owned(), v.to_owned())),
    );
    if mode != Mode::System {
        let exe =
            std::env::current_exe().map_or_else(|_| "hopsync".into(), |p| p.display().to_string());
        rows.extend(sudo_row(&exe, sudo_installed()));
    }
    let title = match mode {
        Mode::System => "Hopsync is running as a system service",
        Mode::User => "Hopsync is running as a user service",
        Mode::Background => "Hopsync is running in the background",
    };
    ui.panel(title, &rows);
    if s.full_access {
        ui.warn(
            "Full access is on: anyone with the web UI password controls what is listed above.",
        );
    }
}

/// Where the settings file lives in `mode`.
fn settings_path(mode: Mode) -> Fallible<PathBuf> {
    Ok(match mode {
        Mode::System => PathBuf::from(SYSTEM_SETTINGS),
        Mode::User | Mode::Background => home()?.join(".config/hopsync/config.toml"),
    })
}

/// The settings file, if there is one. A file with a mistake in it is an error, so nothing changes.
fn read_settings(mode: Mode) -> Fallible<Option<Settings>> {
    let path = settings_path(mode)?;
    match std::fs::read_to_string(&path) {
        Ok(text) => crate::settings::parse(&text)
            .map(Some)
            .map_err(|e| format!("{}: {e}. Nothing was changed.", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}

/// Writes the settings file unless it already says this, which keeps a person's own comments.
fn save_settings(mode: Mode, wanted: &Settings) -> Fallible<()> {
    if read_settings(mode)?.as_ref() == Some(wanted) {
        return Ok(());
    }
    let path = settings_path(mode)?;
    let dir = path.parent().ok_or("the settings file has no folder")?;
    if mode == Mode::System {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    } else {
        private_dir(dir)?;
    }
    write_atomic(&path, crate::settings::render(wanted).as_bytes(), 0o600)
}

/// Whether the web UI gets full access: as asked by a flag, as saved, as answered on a first setup
/// on a terminal, or not. An upgrade or a script that says nothing never gains it.
fn pick_full_access(
    ui: &Ui,
    o: &SetupOptions,
    mode: Mode,
    saved: Option<bool>,
    installed: bool,
) -> Fallible<bool> {
    if let Some(wanted) = o.full_access.or(saved) {
        return Ok(wanted);
    }
    if installed || !std::io::stdin().is_terminal() || o.password_stdin {
        return Ok(false);
    }
    ui.warn(match mode {
        Mode::System => {
            "The web UI can stay in one folder, or reach this whole box as root. With the whole \
             box, anyone with its password controls this server."
        }
        _ => {
            "The web UI can stay in one folder, or reach your whole home folder except hidden \
             items like ~/.ssh. With the home folder, anyone with its password gets those files."
        }
    });
    let question = match mode {
        Mode::System => "Open the whole box to the web UI?",
        _ => "Open your whole home folder to the web UI?",
    };
    ui.confirm(question).map_err(|e| e.to_string())
}

/// Whether the settings file asks for something the service is not running with.
fn settings_differ(mode: Mode, wanted: &Settings) -> Fallible<bool> {
    let (full, folder) = match mode {
        Mode::Background => {
            let state = read_state(&user_data_dir()?).ok_or("Hopsync is not set up")?;
            (state.full_access, state.folder)
        }
        _ => {
            let path = if mode == Mode::System {
                PathBuf::from(SYSTEM_UNIT)
            } else {
                user_unit_path()?
            };
            let unit =
                std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
            let folder = parse_unit(&unit).map(|p| p.0).unwrap_or_default();
            (unit_has_full_access(&unit), folder)
        }
    };
    Ok(full != wanted.full_access || (!full && folder != wanted.folder))
}

/// Whether to open the web UI on this server's address: as asked by a flag, as chosen last time, as
/// answered now, or not. Scripts that say nothing keep the tunnel, so nothing becomes public unasked.
fn pick_public(ui: &Ui, o: &SetupOptions, previous: Option<&Public>) -> Fallible<Option<Public>> {
    let ssh = std::env::var("SSH_CONNECTION").ok();
    let found = address_from_ssh(ssh.as_deref()).or_else(address_from_route);
    if let Some((cert, key)) = &o.own_cert {
        return Ok(Some(Public {
            address: found.unwrap_or_default(),
            own_cert: Some((cert.clone(), key.clone())),
        }));
    }
    if o.public == Some(false) {
        return Ok(None);
    }
    if let Some(p) = previous {
        return Ok(Some(p.clone()));
    }
    let asked = o.public == Some(true);
    let address = match found {
        Some(a) => a,
        None if asked => {
            return Err(
                "could not work out this server's address, so the certificate cannot name it. \
                 Run setup over SSH, so it can read the address you connected to."
                    .into(),
            );
        }
        None => return Ok(None),
    };
    if !asked && (!std::io::stdin().is_terminal() || o.password_stdin) {
        return Ok(None);
    }
    let yes = asked
        || ui
            .confirm_yes(&format!("Open the web UI at https://{address}?"))
            .map_err(|e| e.to_string())?;
    Ok(yes.then_some(Public {
        address,
        own_cert: None,
    }))
}

/// The folder to use: as given, as chosen last time, as answered now, or the default.
fn pick_folder(
    ui: &Ui,
    o: &SetupOptions,
    saved: Option<&Path>,
    previous: Option<&Path>,
    default: &str,
    data_dir: &Path,
    full_access: bool,
) -> Fallible<PathBuf> {
    let home_dir = home().unwrap_or_default();
    let check = |text: &str| -> Fallible<PathBuf> {
        let folder = check_folder_text(&expand_tilde(text, &home_dir))?;
        check_folder_safe(&folder, data_dir, &home_dir)?;
        Ok(folder)
    };
    if let Some(f) = &o.folder {
        return check(f);
    }
    // A person may have typed this one, so it is checked like an answer.
    if let Some(s) = saved {
        return check(&s.to_string_lossy());
    }
    if let Some(p) = previous {
        return Ok(p.to_owned());
    }
    // With full access this is only remembered for later, so there is nothing to ask.
    if full_access || !std::io::stdin().is_terminal() || o.password_stdin {
        return check(default);
    }
    loop {
        let answer = ui
            .ask_line("Folder for your files", default)
            .map_err(|e| e.to_string())?;
        match check(&answer) {
            Ok(p) => return Ok(p),
            Err(e) => ui.warn(&e),
        }
    }
}

/// The line offering a system service, for a box where sudo could give one.
fn sudo_row(exe: &str, sudo_installed: bool) -> Option<(String, String)> {
    sudo_installed.then(|| {
        (
            "With sudo".to_owned(),
            format!("sudo {exe} setup  (a system service instead)"),
        )
    })
}

fn sudo_installed() -> bool {
    std::env::var_os("PATH")
        .is_some_and(|path| std::env::split_paths(&path).any(|d| d.join("sudo").is_file()))
}

/// The last lines of the service log, shown when it will not start.
fn with_journal(mode: Mode, error: String) -> String {
    let mut args = Vec::new();
    if mode == Mode::User {
        args.push("--user");
    }
    args.extend_from_slice(&["-u", "hopsync", "-n", "12", "--no-pager", "-o", "cat"]);
    match run("journalctl", &args) {
        Ok(log) if !log.is_empty() => format!("{error}\nLast log lines:\n{log}"),
        _ => error,
    }
}

/// A system service's folder: created if missing. Returns the account, group and uid that run it.
fn ready_system_folder(ui: &Ui, folder: &Path) -> Fallible<(String, String, u32)> {
    let owner = match std::fs::metadata(folder) {
        Ok(m) if !m.is_dir() => return Err(format!("{} is not a folder", folder.display())),
        Ok(m) => Some(run("id", &["-nu", &m.uid().to_string()]).map_err(|_| {
            format!(
                "{} belongs to uid {}, which has no account here",
                folder.display(),
                m.uid()
            )
        })?),
        Err(_) => None,
    };
    let user = service_owner(folder, owner.as_deref())?;
    let group = run("id", &["-gn", &user])?;
    let uid: u32 = run("id", &["-u", &user])?
        .parse()
        .map_err(|_| format!("cannot read the uid of {user}"))?;

    if owner.is_none() {
        ui.step("Creating your files folder", || -> Fallible<()> {
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o750)
                .create(folder)
                .map_err(|e| format!("{}: {e}", folder.display()))?;
            run(
                "chown",
                &[&format!("{user}:{group}"), "--", &folder.to_string_lossy()],
            )
            .map(|_| ())
        })?;
    }
    Ok((user, group, uid))
}

fn setup_system(ui: &Ui, o: &SetupOptions) -> Fallible<Started> {
    let unit = Path::new(SYSTEM_UNIT);
    let previous = std::fs::read_to_string(unit).ok();
    if let Some(p) = &previous
        && !p.starts_with(MARKER)
    {
        return Err(format!(
            "{SYSTEM_UNIT} already exists and setup did not write it. Move it away, then run setup again."
        ));
    }
    let parsed = previous.as_deref().and_then(parse_unit);
    let running = previous.is_some() && service_active(Mode::System);
    let saved = read_settings(Mode::System)?;
    let was_full = previous.as_deref().is_some_and(unit_has_full_access);
    let full_access = pick_full_access(
        ui,
        o,
        Mode::System,
        saved
            .as_ref()
            .map(|s| s.full_access)
            .or(was_full.then_some(true)),
        previous.is_some(),
    )?;
    let folder = pick_folder(
        ui,
        o,
        saved.as_ref().map(|s| s.folder.as_path()),
        // A full access unit's --root is not a folder anyone chose.
        parsed.as_ref().filter(|_| !was_full).map(|p| p.0.as_path()),
        SYSTEM_FOLDER,
        Path::new(SYSTEM_DATA),
        full_access,
    )?;
    let port = choose_port(o.port, parsed.as_ref().map(|p| p.1), running)?;
    let public = pick_public(ui, o, previous.as_deref().and_then(parse_public).as_ref())?;

    if run("id", &["-u", SERVICE_USER]).is_err() {
        ui.step("Creating the hopsync service account", || {
            let shell = ["/usr/sbin/nologin", "/sbin/nologin", "/bin/false"]
                .into_iter()
                .find(|p| Path::new(p).exists())
                .unwrap_or("/bin/false");
            run(
                "useradd",
                &[
                    "--system",
                    "--create-home",
                    "--home-dir",
                    SYSTEM_HOME,
                    "--shell",
                    shell,
                    SERVICE_USER,
                ],
            )
        })?;
    } else {
        ui.skipped("Service account ready");
    }

    let (user, group, uid) = if full_access {
        ("root".to_owned(), "root".to_owned(), 0)
    } else {
        ready_system_folder(ui, &folder)?
    };
    let spec = UnitSpec {
        folder: folder.clone(),
        full_access,
        port,
        public: public.clone(),
        user,
        group,
        exec: PathBuf::from(SYSTEM_BIN),
        data_dir: PathBuf::from(SYSTEM_DATA),
    };
    let rendered = if full_access {
        render_full_system_unit(&spec)?
    } else {
        render_system_unit(&spec)?
    };
    save_settings(
        Mode::System,
        &Settings {
            full_access,
            folder: folder.clone(),
        },
    )?;
    let new_binary = ui.step("Installing the binary", || {
        install_binary(Path::new(SYSTEM_BIN))
    })?;
    let changed = ui.step("Writing the system service", || -> Fallible<bool> {
        let changed = write_unit(unit, &rendered)?;
        if changed {
            systemctl(Mode::System, &["daemon-reload"])?;
        }
        Ok(changed)
    })?;

    let setup_required = ui
        .step("Starting Hopsync", || -> Fallible<bool> {
            systemctl(Mode::System, &["enable", "--now", "--quiet", "hopsync"])?;
            if running && (changed || new_binary) {
                systemctl(Mode::System, &["restart", "hopsync"])?;
            }
            let mut alive = || {
                // `failed` is final for now; `activating` is a restart in progress.
                match systemctl(Mode::System, &["is-failed", "--quiet", "hopsync"]) {
                    Ok(_) => Err("the service failed to start".to_owned()),
                    Err(_) => Ok(()),
                }
            };
            wait_for_daemon(port, uid, &mut alive)
        })
        .map_err(|e| with_journal(Mode::System, e))?;
    Ok(Started {
        public,
        port,
        uid,
        data_dir: PathBuf::from(SYSTEM_DATA),
        folder,
        full_access,
        setup_required,
    })
}

/// A folder a non-root mode uses: create it if missing, and make sure this account can use it.
fn ready_own_folder(ui: &Ui, folder: &Path) -> Fallible<()> {
    if !folder.exists() {
        ui.step("Creating your files folder", || {
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o750)
                .create(folder)
                .map_err(|e| format!("{}: {e}", folder.display()))
        })?;
    }
    if !folder.is_dir() {
        return Err(format!("{} is not a folder", folder.display()));
    }
    let access =
        rustix::fs::Access::READ_OK | rustix::fs::Access::WRITE_OK | rustix::fs::Access::EXEC_OK;
    rustix::fs::access(folder, access)
        .map_err(|e| format!("{}: this account cannot use it: {e}", folder.display()))
}

fn private_dir(dir: &Path) -> Fallible<()> {
    let fail = |e: std::io::Error| format!("{}: {e}", dir.display());
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .map_err(fail)?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)).map_err(fail)
}

fn setup_user(ui: &Ui, o: &SetupOptions) -> Fallible<Started> {
    let unit = user_unit_path()?;
    let previous = std::fs::read_to_string(&unit).ok();
    let parsed = previous.as_deref().and_then(parse_unit);
    let running = previous.is_some() && service_active(Mode::User);
    let home_dir = home()?;
    let default = home_dir.join("hopsync").to_string_lossy().into_owned();
    let data_dir = user_data_dir()?;
    let saved = read_settings(Mode::User)?;
    let was_full = previous.as_deref().is_some_and(unit_has_full_access);
    let full_access = pick_full_access(
        ui,
        o,
        Mode::User,
        saved
            .as_ref()
            .map(|s| s.full_access)
            .or(was_full.then_some(true)),
        previous.is_some(),
    )?;
    let folder = pick_folder(
        ui,
        o,
        saved.as_ref().map(|s| s.folder.as_path()),
        parsed.as_ref().filter(|_| !was_full).map(|p| p.0.as_path()),
        &default,
        &data_dir,
        full_access,
    )?;
    let chosen = folder.clone();
    let folder = if full_access {
        home_dir.clone()
    } else {
        folder
    };
    let port = choose_port(o.port, parsed.as_ref().map(|p| p.1), running)?;
    let public = pick_public(ui, o, previous.as_deref().and_then(parse_public).as_ref())?;
    let exec = own_binary_path(&home_dir)?;
    let spec = UnitSpec {
        folder: folder.clone(),
        full_access,
        port,
        public: public.clone(),
        user: String::new(),
        group: String::new(),
        exec: exec.clone(),
        data_dir: data_dir.clone(),
    };
    let rendered = render_user_unit(&spec)?;
    ready_own_folder(ui, &folder)?;
    private_dir(&data_dir)?;
    save_settings(
        Mode::User,
        &Settings {
            full_access,
            folder: chosen,
        },
    )?;
    let new_binary = ui.step("Installing the binary", || install_binary(&exec))?;
    let changed = ui.step("Writing the user service", || -> Fallible<bool> {
        let changed = write_unit(&unit, &rendered)?;
        if changed {
            systemctl(Mode::User, &["daemon-reload"])?;
        }
        Ok(changed)
    })?;
    let uid = my_uid();
    let setup_required = ui
        .step("Starting Hopsync", || -> Fallible<bool> {
            systemctl(Mode::User, &["enable", "--now", "--quiet", "hopsync"])?;
            if running && (changed || new_binary) {
                systemctl(Mode::User, &["restart", "hopsync"])?;
            }
            let mut alive = || match systemctl(Mode::User, &["is-failed", "--quiet", "hopsync"]) {
                Ok(_) => Err("the service failed to start".to_owned()),
                Err(_) => Ok(()),
            };
            wait_for_daemon(port, uid, &mut alive)
        })
        .map_err(|e| with_journal(Mode::User, e))?;
    // Without lingering the service stops at logout. Many shared hosts refuse this: say what follows.
    if let Ok(me) = run("id", &["-un"])
        && run("loginctl", &["enable-linger", &me]).is_err()
    {
        ui.warn(
            "This box may stop the service when you log out: loginctl enable-linger was refused.",
        );
    }
    Ok(Started {
        public,
        port,
        uid,
        data_dir,
        folder,
        full_access,
        setup_required,
    })
}

/// Where a non-root install keeps the binary: where it is if that is a system folder, else ~/.local.
fn own_binary_path(home_dir: &Path) -> Fallible<PathBuf> {
    let me = std::env::current_exe().map_err(|e| format!("cannot find this binary: {e}"))?;
    let target = if me.starts_with("/usr/") {
        me
    } else {
        home_dir.join(".local/bin/hopsync")
    };
    check_path_text(&target.to_string_lossy())?;
    Ok(target)
}

#[derive(Debug, Serialize, Deserialize)]
struct BackgroundState {
    exec: PathBuf,
    folder: PathBuf,
    port: u16,
    #[serde(default)]
    public: Option<Public>,
    #[serde(default)]
    full_access: bool,
}

fn state_path(data_dir: &Path) -> PathBuf {
    data_dir.join("background.json")
}

fn read_state(data_dir: &Path) -> Option<BackgroundState> {
    let text = std::fs::read_to_string(state_path(data_dir)).ok()?;
    serde_json::from_str(&text).ok()
}

/// The daemon's pid, only if the pid file names a live Hopsync daemon for this data directory.
fn running_pid(data_dir: &Path) -> Option<i32> {
    let text = std::fs::read_to_string(data_dir.join("hopsync.pid")).ok()?;
    let pid: i32 = text.trim().parse().ok().filter(|p| *p > 0)?;
    let meta = std::fs::metadata(format!("/proc/{pid}")).ok()?;
    let cmdline = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    (meta.uid() == my_uid() && is_our_daemon(&cmdline, data_dir)).then_some(pid)
}

/// Starts the daemon detached from this terminal. Does nothing if it is already running.
fn background_start(data_dir: &Path, state: &BackgroundState) -> Fallible<()> {
    if running_pid(data_dir).is_some() {
        return Ok(());
    }
    let log = data_dir.join("hopsync.log");
    if std::fs::metadata(&log).is_ok_and(|m| m.len() > LOG_ROTATE_BYTES) {
        let _ = std::fs::rename(&log, data_dir.join("hopsync.log.1"));
    }
    let open = || {
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(&log)
            .map_err(|e| format!("{}: {e}", log.display()))
    };
    let (out, err) = (open()?, open()?);
    let mut cmd = Command::new(&state.exec);
    cmd.arg("serve")
        .args(listen_args(state.port, state.public.as_ref())?)
        .arg("--data-dir")
        .arg(data_dir)
        .arg("--root")
        .arg(if state.full_access {
            home()?
        } else {
            state.folder.clone()
        })
        .args(state.full_access.then_some("--full-access"))
        .stdin(Stdio::null())
        .stdout(out)
        .stderr(err);
    // SAFETY: setsid only changes this process's session and is async-signal-safe, so it is sound
    // between fork and exec. A new session means logging out cannot send the daemon a hangup.
    #[allow(unsafe_code)]
    unsafe {
        cmd.pre_exec(|| {
            rustix::process::setsid()
                .map(|_| ())
                .map_err(std::io::Error::from)
        });
    }
    let child = cmd
        .spawn()
        .map_err(|e| format!("{}: {e}", state.exec.display()))?;
    write_atomic(
        &data_dir.join("hopsync.pid"),
        format!("{}\n", child.id()).as_bytes(),
        0o600,
    )
}

/// Stops the daemon if the pid file names ours. Any other process is left alone.
fn background_stop(data_dir: &Path) -> Fallible<()> {
    if let Some(pid) = running_pid(data_dir) {
        let raw = rustix::process::Pid::from_raw(pid).ok_or("bad pid")?;
        rustix::process::kill_process(raw, rustix::process::Signal::TERM)
            .map_err(|e| format!("stopping pid {pid}: {e}"))?;
        let start = Instant::now();
        while running_pid(data_dir).is_some() {
            if start.elapsed() > STOP_TIMEOUT {
                return Err(format!(
                    "pid {pid} did not stop within {} seconds",
                    STOP_TIMEOUT.as_secs()
                ));
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
    let _ = std::fs::remove_file(data_dir.join("hopsync.pid"));
    Ok(())
}

/// Reads this account's crontab. `Ok("")` when it has none; an error when cron cannot be used.
fn read_crontab() -> Fallible<String> {
    let out = Command::new("crontab")
        .arg("-l")
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("crontab: {e}"))?;
    if out.status.success() {
        return Ok(String::from_utf8_lossy(&out.stdout).into_owned());
    }
    let err = String::from_utf8_lossy(&out.stderr);
    if err.contains("no crontab") {
        return Ok(String::new());
    }
    Err(err.trim().to_owned())
}

fn write_crontab(content: &str) -> Fallible<()> {
    let mut child = Command::new("crontab")
        .arg("-")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("crontab: {e}"))?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(content.as_bytes())
            .map_err(|e| format!("crontab: {e}"))?;
    }
    let out = child
        .wait_with_output()
        .map_err(|e| format!("crontab: {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_owned())
    }
}

fn setup_background(ui: &Ui, o: &SetupOptions) -> Fallible<Started> {
    let data_dir = user_data_dir()?;
    check_path_text(&data_dir.to_string_lossy())?;
    let previous = read_state(&data_dir);
    let running = running_pid(&data_dir).is_some();
    let home_dir = home()?;
    let default = home_dir.join("hopsync").to_string_lossy().into_owned();
    let saved = read_settings(Mode::Background)?;
    let full_access = pick_full_access(
        ui,
        o,
        Mode::Background,
        saved
            .as_ref()
            .map(|s| s.full_access)
            .or(previous.as_ref().filter(|p| p.full_access).map(|_| true)),
        previous.is_some(),
    )?;
    let folder = pick_folder(
        ui,
        o,
        saved.as_ref().map(|s| s.folder.as_path()),
        previous.as_ref().map(|s| s.folder.as_path()),
        &default,
        &data_dir,
        full_access,
    )?;
    if full_access {
        check_path_text(&home_dir.to_string_lossy())?;
    }
    let port = choose_port(o.port, previous.as_ref().map(|s| s.port), running)?;
    let public = pick_public(ui, o, previous.as_ref().and_then(|s| s.public.as_ref()))?;
    let exec = own_binary_path(&home_dir)?;
    if !full_access {
        ready_own_folder(ui, &folder)?;
    }
    private_dir(&data_dir)?;
    save_settings(
        Mode::Background,
        &Settings {
            full_access,
            folder: folder.clone(),
        },
    )?;
    let new_binary = ui.step("Installing the binary", || install_binary(&exec))?;
    let state = BackgroundState {
        exec: exec.clone(),
        folder: folder.clone(),
        port,
        public: public.clone(),
        full_access,
    };
    let changed = previous.as_ref().is_none_or(|p| {
        p.folder != state.folder
            || p.port != state.port
            || p.exec != state.exec
            || p.public != state.public
            || p.full_access != state.full_access
    });
    if changed {
        let text = serde_json::to_vec(&state).map_err(|e| e.to_string())?;
        write_atomic(&state_path(&data_dir), &text, 0o600)?;
    }
    let uid = my_uid();
    let setup_required = ui.step(
        "Starting Hopsync in the background",
        || -> Fallible<bool> {
            if running && (changed || new_binary) {
                background_stop(&data_dir)?;
            }
            background_start(&data_dir, &state)?;
            let dir = data_dir.clone();
            let mut alive = move || {
                if running_pid(&dir).is_some() {
                    return Ok(());
                }
                let log = std::fs::read_to_string(dir.join("hopsync.log")).unwrap_or_default();
                let tail: Vec<&str> = log.lines().rev().take(6).collect();
                let tail: Vec<&str> = tail.into_iter().rev().collect();
                Err(format!(
                    "the daemon stopped. Last log lines:\n{}",
                    tail.join("\n")
                ))
            };
            wait_for_daemon(port, uid, &mut alive)
        },
    )?;
    let exe_text = exec.to_string_lossy();
    match read_crontab().and_then(|c| {
        let wanted = crontab_with(&c, &exe_text);
        if wanted == c {
            Ok(false)
        } else {
            write_crontab(&wanted).map(|()| true)
        }
    }) {
        Ok(true) => ui.skipped("Hopsync will start again after a reboot"),
        Ok(false) => ui.skipped("Restart after reboot already set"),
        Err(_) => ui.warn("This box does not allow cron, so run `hopsync start` after a reboot."),
    }
    Ok(Started {
        public,
        port,
        uid,
        data_dir,
        folder: if full_access { home_dir } else { folder },
        full_access,
        setup_required,
    })
}

/// How Hopsync is installed here, if it is: what the control commands act on.
fn installed_mode() -> Fallible<Mode> {
    if Path::new(SYSTEM_UNIT).exists() {
        return Ok(Mode::System);
    }
    if !is_root() {
        if user_unit_path().is_ok_and(|p| p.exists()) {
            return Ok(Mode::User);
        }
        if read_state(&user_data_dir()?).is_some() {
            return Ok(Mode::Background);
        }
    }
    Err("Hopsync is not set up on this box. Run `hopsync setup`.".into())
}

#[derive(Debug, Clone, Copy)]
pub enum Control {
    Status,
    Start,
    Stop,
    Restart,
    Logs { follow: bool },
}

/// Runs a command with this terminal attached, for output the user reads as it comes.
fn passthrough(program: &str, args: &[&str]) -> Fallible<()> {
    Command::new(program)
        .args(args)
        .status()
        .map(|_| ())
        .map_err(|e| format!("{program}: {e}"))
}

/// `hopsync status|start|stop|restart|logs`: the same commands in every mode.
pub fn control(what: Control) -> Fallible<()> {
    let done = match what {
        Control::Start => Some("started"),
        Control::Stop => Some("stopped"),
        Control::Restart => Some("restarted"),
        Control::Status | Control::Logs { .. } => None,
    };
    run_control(what)?;
    if let Some(done) = done {
        println!("hopsync: {done}");
    }
    Ok(())
}

fn run_control(what: Control) -> Fallible<()> {
    let mode = installed_mode()?;
    // Without root a system install's settings cannot be read, and systemctl says why that fails.
    if matches!(what, Control::Restart)
        && (mode != Mode::System || is_root())
        && let Some(wanted) = read_settings(mode)?
        && settings_differ(mode, &wanted)?
    {
        return run_setup(&SetupOptions::default());
    }
    if mode == Mode::Background {
        return background_control(what);
    }
    let flag: &[&str] = if mode == Mode::User { &["--user"] } else { &[] };
    let with = |rest: &[&'static str]| -> Vec<&'static str> {
        flag.iter().chain(rest.iter()).copied().collect()
    };
    match what {
        Control::Status => passthrough("systemctl", &with(&["status", "hopsync", "--no-pager"])),
        Control::Start => systemctl(mode, &["start", "hopsync"]).map(|_| ()),
        Control::Stop => systemctl(mode, &["stop", "hopsync"]).map(|_| ()),
        Control::Restart => systemctl(mode, &["restart", "hopsync"]).map(|_| ()),
        Control::Logs { follow } => {
            let rest: &[&'static str] = if follow {
                &["-u", "hopsync", "-n", "50", "--no-pager", "-f"]
            } else {
                &["-u", "hopsync", "-n", "50", "--no-pager"]
            };
            passthrough("journalctl", &with(rest))
        }
    }
}

fn background_control(what: Control) -> Fallible<()> {
    let data_dir = user_data_dir()?;
    let state = read_state(&data_dir).ok_or("Hopsync is not set up. Run `hopsync setup`.")?;
    match what {
        Control::Status => {
            match running_pid(&data_dir) {
                Some(pid) => println!(
                    "hopsync: running in the background, pid {pid}, web UI on {}",
                    state.public.as_ref().map_or_else(
                        || format!("http://127.0.0.1:{}", state.port),
                        |p| public_url(p, state.port)
                    )
                ),
                None => println!("hopsync: stopped. Start it with `hopsync start`."),
            }
            Ok(())
        }
        Control::Start => background_start(&data_dir, &state),
        Control::Stop => background_stop(&data_dir),
        Control::Restart => {
            background_stop(&data_dir)?;
            background_start(&data_dir, &state)
        }
        Control::Logs { follow } => {
            let log = data_dir.join("hopsync.log");
            let log = log.to_string_lossy();
            if follow {
                passthrough("tail", &["-n", "50", "-f", "--", &log])
            } else {
                passthrough("tail", &["-n", "50", "--", &log])
            }
        }
    }
}

/// The panel row saying what happened to the user's files. Plain "not touched" is only for an
/// install whose folder can no longer be read back.
fn files_kept_row(folder: Option<&Path>) -> (String, String) {
    let text = match folder {
        Some(f) => format!("still in {}", f.display()),
        None => "were not touched".to_owned(),
    };
    ("Your files".to_owned(), text)
}

/// `hopsync uninstall`. Removes the service and binary. The data stays unless `purge` is set, and
/// the files folder always stays: those are the user's files.
pub fn run_uninstall(purge: bool, yes: bool, plain: bool) -> Fallible<()> {
    let ui = Ui::detect(plain);
    let mode = installed_mode()?;
    let data_dir = match mode {
        Mode::System => PathBuf::from(SYSTEM_HOME),
        _ => user_data_dir()?,
    };
    // Read now: the unit and the state file are gone by the time the panel is printed. Full access
    // has no folder of its own to point to.
    let folder = read_settings(mode)
        .ok()
        .flatten()
        .filter(|s| !s.full_access)
        .map(|s| s.folder)
        .or_else(|| match mode {
            Mode::System => std::fs::read_to_string(SYSTEM_UNIT)
                .ok()
                .filter(|u| !unit_has_full_access(u))
                .and_then(|u| parse_unit(&u))
                .map(|(f, _)| f),
            Mode::User => user_unit_path()
                .ok()
                .and_then(|p| std::fs::read_to_string(p).ok())
                .filter(|u| !unit_has_full_access(u))
                .and_then(|u| parse_unit(&u))
                .map(|(f, _)| f),
            Mode::Background => read_state(&data_dir)
                .filter(|s| !s.full_access)
                .map(|s| s.folder),
        });
    if !yes {
        let what = if purge {
            format!(
                "This removes the service and deletes {}, including saved servers and keys.",
                data_dir.display()
            )
        } else {
            "This removes the service. Your settings and files stay.".to_owned()
        };
        ui.warn(&what);
        if !ui.confirm("Continue?").map_err(|e| e.to_string())? {
            return Err("nothing was changed".into());
        }
    }
    match mode {
        Mode::System | Mode::User => {
            let unit = if mode == Mode::System {
                PathBuf::from(SYSTEM_UNIT)
            } else {
                user_unit_path()?
            };
            let text =
                std::fs::read_to_string(&unit).map_err(|e| format!("{}: {e}", unit.display()))?;
            if !text.starts_with(MARKER) {
                return Err(format!(
                    "{} was not written by setup, so it stays.",
                    unit.display()
                ));
            }
            ui.step("Stopping the service", || -> Fallible<()> {
                // Fine to fail: the unit may already be stopped or disabled.
                let _ = systemctl(mode, &["disable", "--now", "--quiet", "hopsync"]);
                std::fs::remove_file(&unit).map_err(|e| format!("{}: {e}", unit.display()))?;
                systemctl(mode, &["daemon-reload"]).map(|_| ())
            })?;
        }
        Mode::Background => {
            ui.step("Stopping Hopsync", || background_stop(&data_dir))?;
            if let Ok(c) = read_crontab() {
                let wanted = crontab_without(&c);
                if wanted != c {
                    ui.step("Removing the reboot entry", || write_crontab(&wanted))?;
                }
            }
            if !purge {
                let _ = std::fs::remove_file(state_path(&data_dir));
            }
        }
    }
    let home_bin = home().ok().map(|h| h.join(".local/bin/hopsync"));
    let bin = if mode == Mode::System {
        Some(PathBuf::from(SYSTEM_BIN))
    } else {
        home_bin
    };
    if let Some(bin) = bin.filter(|b| b.exists()) {
        ui.step("Removing the binary", || {
            std::fs::remove_file(&bin).map_err(|e| format!("{}: {e}", bin.display()))
        })?;
    }
    if purge {
        // A fixed folder for a system install; for a user install, only one that is clearly ours.
        if mode != Mode::System && !(data_dir.is_absolute() && data_dir.ends_with("hopsync")) {
            return Err(format!(
                "{} does not look like Hopsync's folder, so it stays.",
                data_dir.display()
            ));
        }
        ui.step("Deleting saved servers and keys", || {
            std::fs::remove_dir_all(&data_dir).map_err(|e| format!("{}: {e}", data_dir.display()))
        })?;
        if let Ok(path) = settings_path(mode) {
            let _ = std::fs::remove_file(&path);
            // Fails if anything else is in there, which then stays.
            if let Some(dir) = path.parent() {
                let _ = std::fs::remove_dir(dir);
            }
        }
        if mode == Mode::System {
            let _ = run("userdel", &[SERVICE_USER]);
        }
        ui.panel("Hopsync is removed", &[files_kept_row(folder.as_deref())]);
    } else {
        ui.panel(
            "Hopsync is removed",
            &[
                (
                    "Settings kept in".to_owned(),
                    data_dir.display().to_string(),
                ),
                (
                    "Delete them too".to_owned(),
                    format!("rm -rf {}", data_dir.display()),
                ),
                files_kept_row(folder.as_deref()),
            ],
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(folder: &str) -> UnitSpec {
        UnitSpec {
            public: None,
            full_access: false,
            folder: PathBuf::from(folder),
            port: 8123,
            user: "alice".into(),
            group: "media".into(),
            exec: PathBuf::from("/home/alice/.local/bin/hopsync"),
            data_dir: PathBuf::from("/home/alice/.local/share/hopsync"),
        }
    }

    #[test]
    fn root_needs_systemd_and_others_always_have_a_mode() {
        assert_eq!(choose_mode(true, true, false), Ok(Mode::System));
        assert_eq!(choose_mode(false, true, true), Ok(Mode::User));
        assert_eq!(choose_mode(false, true, false), Ok(Mode::Background));
        assert_eq!(choose_mode(false, false, false), Ok(Mode::Background));
        assert!(
            choose_mode(true, false, false)
                .unwrap_err()
                .contains("as root")
        );
    }

    #[test]
    fn a_public_unit_listens_everywhere_over_https_and_reads_back() {
        let mut s = spec("/srv/data");
        s.public = Some(Public {
            address: "203.0.113.7".into(),
            own_cert: None,
        });
        for unit in [
            render_system_unit(&s).unwrap(),
            render_user_unit(&s).unwrap(),
        ] {
            assert!(
                unit.contains("--listen 0.0.0.0:8123 --tls --tls-name 203.0.113.7 "),
                "{unit}"
            );
            assert_eq!(parse_unit(&unit).unwrap().1, 8123);
            assert_eq!(parse_public(&unit), s.public);
        }
    }

    #[test]
    fn an_unasked_unit_stays_on_loopback_with_no_tls() {
        let s = spec("/srv/data");
        for unit in [
            render_system_unit(&s).unwrap(),
            render_user_unit(&s).unwrap(),
        ] {
            assert!(unit.contains("--listen 127.0.0.1:8123 "), "{unit}");
            assert!(!unit.contains("--tls"), "{unit}");
            assert_eq!(parse_public(&unit), None);
        }
    }

    #[test]
    fn an_own_certificate_reads_back_and_a_bad_path_cannot_reach_the_unit() {
        let mut s = spec("/srv/data");
        let own = Some(("/etc/hs/cert.pem".to_owned(), "/etc/hs/key.pem".to_owned()));
        s.public = Some(Public {
            address: String::new(),
            own_cert: own.clone(),
        });
        let unit = render_system_unit(&s).unwrap();
        assert!(unit.contains("--tls-cert /etc/hs/cert.pem --tls-key /etc/hs/key.pem"));
        assert_eq!(parse_public(&unit).unwrap().own_cert, own);

        for bad in ["/x y.pem", "/x\nExecStartPre=/bin/sh"] {
            s.public = Some(Public {
                address: String::new(),
                own_cert: Some((bad.to_owned(), "/k.pem".to_owned())),
            });
            assert!(render_system_unit(&s).is_err(), "{bad}");
        }
        s.public = Some(Public {
            address: "1.2.3.4 --evil".to_owned(),
            own_cert: None,
        });
        assert!(render_system_unit(&s).is_err());
    }

    #[test]
    fn the_address_comes_from_the_ssh_connection_when_it_can_be_used() {
        let from = |c| address_from_ssh(Some(c));
        assert_eq!(
            from("198.51.100.1 5000 203.0.113.7 22").as_deref(),
            Some("203.0.113.7")
        );
        assert_eq!(from("2001:db8::1 5000 2001:db8::2 22"), None);
        assert_eq!(from("127.0.0.1 5000 127.0.0.1 22"), None);
        assert_eq!(from("garbage"), None);
        assert_eq!(address_from_ssh(None), None);
    }

    #[test]
    fn an_older_background_state_file_still_reads_as_local() {
        let old = r#"{"exec":"/x/hopsync","folder":"/x/f","port":7890}"#;
        let state: BackgroundState = serde_json::from_str(old).unwrap();
        assert_eq!(state.public, None);
    }

    #[test]
    fn ufw_is_noticed_only_when_it_would_block_the_port() {
        let active = "Status: active\n\nTo Action From\n-- ------ ----\n22/tcp ALLOW Anywhere\n";
        assert!(ufw_blocks(active, 7890));
        assert!(!ufw_blocks(
            &format!("{active}7890/tcp ALLOW Anywhere\n"),
            7890
        ));
        assert!(!ufw_blocks(&format!("{active}7890 ALLOW Anywhere\n"), 7890));
        assert!(ufw_blocks(
            &format!("{active}17890/tcp ALLOW Anywhere\n"),
            7890
        ));
        assert!(!ufw_blocks("Status: inactive\n", 7890));
    }

    #[test]
    fn a_folder_holding_ssh_keys_or_hopsyncs_own_data_is_refused() {
        let t = tempfile::tempdir().unwrap();
        let home = t.path().join("home/neels");
        let data = home.join(".local/share/hopsync");
        std::fs::create_dir_all(home.join(".ssh")).unwrap();
        std::fs::create_dir_all(&data).unwrap();
        std::fs::create_dir_all(home.join("media")).unwrap();
        // Someone else's home, as root names it: it has keys too.
        let other = t.path().join("home/alice");
        std::fs::create_dir_all(other.join(".ssh")).unwrap();

        let parent = t.path().join("home");
        let dot_local = home.join(".local");
        let keys = home.join(".ssh");
        for bad in [&home, &parent, &other, &dot_local, &keys, &data] {
            let err = check_folder_safe(bad, &data, &home).unwrap_err();
            assert!(err.contains("subfolder"), "{}: {err}", bad.display());
        }
        assert!(check_folder_safe(&home.join("media"), &data, &home).is_ok());
        assert!(check_folder_safe(&home.join("not-made-yet"), &data, &home).is_ok());
    }

    #[test]
    fn a_tilde_means_the_home_folder() {
        let home = Path::new("/home/neels");
        assert_eq!(expand_tilde("~", home), "/home/neels");
        assert_eq!(expand_tilde("~/media", home), "/home/neels/media");
        assert_eq!(expand_tilde("/srv/data", home), "/srv/data");
        assert_eq!(expand_tilde("~alice/media", home), "~alice/media");
    }

    #[test]
    fn the_sudo_hint_appears_only_where_sudo_exists() {
        assert!(sudo_row("/x/hopsync", false).is_none());
        let (_, command) = sudo_row("/x/hopsync", true).unwrap();
        assert!(command.starts_with("sudo /x/hopsync setup"));
    }

    #[test]
    fn folders_that_would_expose_the_system_are_refused() {
        for f in [
            "/",
            "/etc",
            "/etc/",
            "/usr",
            "/root",
            "/var/lib/hopsync",
            "/srv/../etc",
        ] {
            assert!(check_folder_text(f).is_err(), "{f}");
        }
        assert!(check_folder_text("relative/path").is_err());
        assert_eq!(
            check_folder_text("/srv/data/").unwrap(),
            PathBuf::from("/srv/data")
        );
        assert_eq!(
            check_folder_text("/home/a.b-c_d/x+y").unwrap(),
            PathBuf::from("/home/a.b-c_d/x+y")
        );
    }

    #[test]
    fn anything_a_unit_file_or_cron_would_interpret_is_refused() {
        for f in [
            "/a b", "/a%h", "/a$b", "/a\nb", "/a\"b", "/a'b", "/a;b", "/a`b", "/a\\b", "/a#b",
            "/a*b",
        ] {
            assert!(check_path_text(f).is_err(), "{f:?}");
        }
    }

    #[test]
    fn service_runs_as_a_folder_owner_but_never_root() {
        let f = Path::new("/srv/x");
        assert_eq!(service_owner(f, None).unwrap(), "hopsync");
        assert_eq!(service_owner(f, Some("alice")).unwrap(), "alice");
        let e = service_owner(f, Some("root")).unwrap_err();
        assert!(e.contains("chown -R hopsync: /srv/x"), "{e}");
    }

    #[test]
    fn system_unit_carries_this_boxs_values_and_keeps_the_hardening() {
        let u = render_system_unit(&spec("/srv/data")).unwrap();
        assert!(u.starts_with(MARKER));
        assert!(u.contains("\nUser=alice\n"));
        assert!(u.contains("\nGroup=media\n"));
        assert!(u.contains("--listen 127.0.0.1:8123 "));
        assert!(u.contains("--root /srv/data "));
        assert!(u.contains("\nReadWritePaths=/srv/data\n"));
        assert!(u.contains("\nProtectHome=yes\n"));
        assert!(!u.contains("BindPaths"));
        for keep in [
            "NoNewPrivileges=yes",
            "ProtectSystem=strict",
            "CapabilityBoundingSet=",
            "MemoryDenyWriteExecute=yes",
        ] {
            assert!(u.contains(keep), "{keep}");
        }
        assert!(!u.contains("/srv/hopsync"));
        assert!(
            !u.contains("useradd"),
            "the how-to-install comments stay out"
        );
    }

    #[test]
    fn a_folder_in_a_home_gets_only_that_folder_not_every_home() {
        let u = render_system_unit(&spec("/home/alice/media")).unwrap();
        assert!(u.contains("\nProtectHome=tmpfs\nBindPaths=/home/alice/media\n"));
        assert!(!u.contains("ProtectHome=yes"));
    }

    #[test]
    fn a_bad_value_cannot_inject_a_unit_line() {
        let mut s = spec("/srv/x");
        s.user = "a\nExecStartPre=/bin/evil".into();
        assert!(render_system_unit(&s).is_err());
        assert!(render_system_unit(&spec("/srv/x\nExecStartPre=/bin/evil")).is_err());
    }

    #[test]
    fn user_unit_names_binary_data_and_folder() {
        let u = render_user_unit(&spec("/home/alice/media")).unwrap();
        assert!(u.contains(
            "ExecStart=/home/alice/.local/bin/hopsync serve --listen 127.0.0.1:8123 --data-dir /home/alice/.local/share/hopsync --root /home/alice/media\n"
        ));
        assert!(u.contains("NoNewPrivileges=yes"));
    }

    #[test]
    fn a_rendered_unit_can_be_read_back() {
        let sys = render_system_unit(&spec("/srv/data")).unwrap();
        assert_eq!(parse_unit(&sys), Some((PathBuf::from("/srv/data"), 8123)));
        let user = render_user_unit(&spec("/home/a/b")).unwrap();
        assert_eq!(parse_unit(&user), Some((PathBuf::from("/home/a/b"), 8123)));
        assert_eq!(parse_unit("[Service]\nExecStart=/bin/true\n"), None);
    }

    #[test]
    fn the_full_access_unit_runs_as_root_over_the_whole_box() {
        let mut s = spec("/srv/x");
        s.full_access = true;
        let u = render_full_system_unit(&s).unwrap();
        assert!(u.starts_with(MARKER));
        // systemd sets HOME only when User= is named, and the daemon cannot start without it.
        assert!(u.contains("\nUser=root\n"), "{u}");
        assert!(u.contains("--listen 127.0.0.1:8123"), "{u}");
        assert!(u.contains("--root / --full-access"), "{u}");
        assert!(unit_has_full_access(&u));
        assert_eq!(parse_unit(&u), Some((PathBuf::from("/"), 8123)));
        // Root's powers beyond files stay out of reach.
        assert!(
            u.contains("CapabilityBoundingSet=CAP_DAC_OVERRIDE CAP_DAC_READ_SEARCH CAP_FOWNER\n")
        );
        assert!(u.contains("NoNewPrivileges=yes"));
        assert!(u.contains("UMask=0022"));
        // SQLite calls fchown on its journal files when it runs as root. Without this the filter
        // kills the daemon at its first write.
        let (denied, allowed) = (
            u.find("SystemCallFilter=~@privileged").unwrap(),
            u.find("SystemCallFilter=@chown\n").expect("chown allowed"),
        );
        assert!(denied < allowed, "a later line must add it back");
    }

    #[test]
    fn the_default_units_do_not_have_full_access() {
        let s = spec("/srv/data");
        let sys = render_system_unit(&s).unwrap();
        assert!(sys.contains("User=alice"));
        assert!(!unit_has_full_access(&sys));
        assert!(!unit_has_full_access(&render_user_unit(&s).unwrap()));
        assert!(!unit_has_full_access(
            "[Service]\nDescription=--full-access\n"
        ));
    }

    #[test]
    fn a_user_unit_with_full_access_says_so() {
        let mut s = spec("/home/alice");
        s.full_access = true;
        let u = render_user_unit(&s).unwrap();
        assert!(u.contains("--root /home/alice --full-access"), "{u}");
        assert!(unit_has_full_access(&u));
    }

    #[test]
    fn a_refused_system_folder_names_the_way_to_full_access() {
        assert!(
            check_folder_text("/root")
                .unwrap_err()
                .contains("--full-access")
        );
    }

    #[test]
    fn uninstall_says_where_the_files_stayed() {
        assert_eq!(
            files_kept_row(Some(Path::new("/srv/hopsync"))),
            ("Your files".to_owned(), "still in /srv/hopsync".to_owned())
        );
        assert_eq!(
            files_kept_row(None),
            ("Your files".to_owned(), "were not touched".to_owned())
        );
    }

    #[test]
    fn listener_uid_finds_only_a_local_listening_socket() {
        let table = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n\
           0: 0100007F:1ED2 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1035        0 1\n\
           1: 0100007F:1ED3 0100007F:9999 01 00000000:00000000 00:00000000 00000000  1036        0 2\n\
           2: 5FA8A2E3:1ED4 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1037        0 3\n\
           3: 00000000:1ED5 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1038        0 4\n";
        assert_eq!(listener_uid(table, 0x1ED2), Some(1035), "loopback listener");
        assert_eq!(
            listener_uid(table, 0x1ED3),
            None,
            "established, not listening"
        );
        assert_eq!(listener_uid(table, 0x1ED4), None, "another address");
        assert_eq!(listener_uid(table, 0x1ED5), Some(1038), "wildcard listener");
        assert_eq!(listener_uid(table, 7), None);
        assert_eq!(listener_uid("", 7), None);
    }

    #[test]
    fn crontab_edits_touch_only_the_managed_line() {
        let mine = "MAILTO=me\n5 4 * * * /usr/bin/true # mine\n";
        let with = crontab_with(mine, "/h/.local/bin/hopsync");
        assert!(with.starts_with(mine));
        assert!(
            with.contains(
                "@reboot /h/.local/bin/hopsync start >/dev/null 2>&1 # hopsync-managed\n"
            )
        );
        assert_eq!(
            crontab_with(&with, "/h/.local/bin/hopsync"),
            with,
            "no second line"
        );
        assert_eq!(crontab_without(&with), mine);
        assert_eq!(crontab_without(""), "");
    }

    #[test]
    fn only_our_daemon_counts_as_running() {
        let data = Path::new("/home/c/.local/share/hopsync");
        let ours = b"/home/c/.local/bin/hopsync\0serve\0--listen\0localhost:7890\0--data-dir\0/home/c/.local/share/hopsync\0--root\0/home/c\0";
        assert!(is_our_daemon(ours, data));
        assert!(!is_our_daemon(b"sleep\0five\0", data));
        assert!(!is_our_daemon(b"", data));
        assert!(!is_our_daemon(
            b"vim\0serve\0/home/c/.local/share/hopsync\0",
            data
        ));
        let other = b"/x/hopsync\0serve\0--data-dir\0/elsewhere\0";
        assert!(
            !is_our_daemon(other, data),
            "a daemon for another data directory"
        );
        let push = b"/x/hopsync\0push\0/home/c/.local/share/hopsync\0";
        assert!(!is_our_daemon(push, data), "a transfer, not the daemon");
    }

    #[test]
    fn tunnel_uses_the_address_the_user_connected_to() {
        assert_eq!(
            tunnel_command(7891, Some("neels"), Some("1.2.3.4 5555 154.12.118.60 22")),
            "ssh -L 7891:127.0.0.1:7891 neels@154.12.118.60"
        );
        assert_eq!(
            tunnel_command(7890, None, None),
            "ssh -L 7890:127.0.0.1:7890 your-server"
        );
    }

    #[test]
    fn each_mode_prints_commands_that_work_in_it() {
        assert_eq!(command_rows(Mode::System)[0].1, "systemctl status hopsync");
        assert_eq!(
            command_rows(Mode::User)[0].1,
            "systemctl --user status hopsync"
        );
        assert_eq!(command_rows(Mode::Background)[0].1, "hopsync status");
    }

    #[test]
    fn a_unit_setup_did_not_write_is_never_replaced() {
        let ours = format!("{MARKER}\nold");
        assert_eq!(unit_change(None, "new"), UnitChange::Create);
        assert_eq!(unit_change(Some("new"), "new"), UnitChange::Same);
        assert_eq!(unit_change(Some(&ours), "new"), UnitChange::Replace);
        assert_eq!(unit_change(Some("[Service]\n"), "new"), UnitChange::Foreign);
    }
}
