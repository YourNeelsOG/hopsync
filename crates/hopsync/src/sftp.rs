//! SFTP session over one SSH connection (plan.md §6.1).
//!
//! - Every read and write is split to the server's `limits@openssh.com` sizes. OpenSSH's sftp-server
//!   drops the connection on an oversized packet (Phase 0), and russh-sftp does not split for us.
//! - At most `inflight` requests are outstanding per session, across all callers, so memory and
//!   server load stay bounded. Concurrency comes from several callers sharing one session.
//! - A lost connection is `SftpError::ConnectionLost`, distinct from server status errors, so the
//!   engine can retry a lane instead of failing a file.

use std::fmt;
use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use russh_sftp::client::RawSftpSession;
use russh_sftp::client::error::Error as RawError;
use russh_sftp::protocol::{FileAttributes, FileType, OpenFlags, Packet, StatusCode};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::sync::{Semaphore, watch};

use crate::ssh::{SshConn, SshError};

/// Used when the server does not report limits: the SFTP minimum every server must accept.
const FALLBACK_LEN: u32 = 32 * 1024;
/// Upper bound regardless of what the server reports, to keep per-request buffers bounded.
const MAX_LEN: u32 = 256 * 1024;

#[derive(Debug, Clone)]
pub struct SftpOptions {
    /// Requests outstanding at once on this session (Phase 0: 16 gets most of the pipelining gain).
    pub inflight: usize,
    /// Time allowed for each response; a stalled server fails with `SftpError::Timeout`.
    pub request_timeout: Duration,
    /// A directory listing longer than this fails instead of growing without bound (a hostile or
    /// broken server can send names forever).
    pub max_dir_entries: usize,
}

impl Default for SftpOptions {
    fn default() -> Self {
        Self {
            inflight: 16,
            request_timeout: Duration::from_secs(60),
            max_dir_entries: 10_000_000,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoteOpenMode {
    Read,
    /// Write an existing file; a missing file is `NotFound`.
    Write,
    /// Write, creating the file if missing. Never truncates: partial files are resumed.
    WriteCreate,
    /// `Write`, readable too (verify reads back what it wrote).
    ReadWrite,
    /// `WriteCreate`, readable too.
    ReadWriteCreate,
}

#[derive(Debug)]
pub enum SftpError {
    Ssh(SshError),
    NotFound(String),
    PermissionDenied(String),
    /// Any other status the server returned.
    Status {
        path: String,
        code: String,
        message: String,
    },
    Timeout,
    ConnectionLost(String),
    Unsupported(&'static str),
    Protocol(String),
    /// A directory listing passed `max_dir_entries`.
    ListingTooLong {
        path: String,
        max: usize,
    },
}

impl fmt::Display for SftpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Ssh(e) => write!(f, "{e}"),
            Self::NotFound(p) => write!(f, "{p}: no such file or directory"),
            Self::PermissionDenied(p) => write!(f, "{p}: permission denied"),
            Self::Status {
                path,
                code,
                message,
            } => write!(f, "{path}: {code}: {message}"),
            Self::Timeout => f.write_str("SFTP request timed out"),
            Self::ConnectionLost(reason) => write!(f, "SFTP connection lost: {reason}"),
            Self::Unsupported(what) => write!(f, "server does not support {what}"),
            Self::Protocol(reason) => write!(f, "SFTP protocol error: {reason}"),
            Self::ListingTooLong { path, max } => {
                write!(f, "{path}: directory has more than {max} entries")
            }
        }
    }
}

impl std::error::Error for SftpError {}

impl From<SshError> for SftpError {
    fn from(e: SshError) -> Self {
        Self::Ssh(e)
    }
}

fn map_err(e: RawError, path: &str) -> SftpError {
    match e {
        RawError::Status(s) => match s.status_code {
            StatusCode::NoSuchFile => SftpError::NotFound(path.to_owned()),
            StatusCode::PermissionDenied => SftpError::PermissionDenied(path.to_owned()),
            code => SftpError::Status {
                path: path.to_owned(),
                code: format!("{code:?}"),
                message: s.error_message,
            },
        },
        RawError::Timeout => SftpError::Timeout,
        RawError::IO(reason) => SftpError::ConnectionLost(reason),
        // russh-sftp 3.0.1 has no typed "connection closed" error: its session task dying shows up as
        // these messages (client/rawsession.rs, client/error.rs). Matched by text, so the
        // `killed_server_is_connection_lost` test guards against a wording change on upgrade.
        RawError::UnexpectedBehavior(reason) if is_session_gone(&reason) => {
            SftpError::ConnectionLost(reason)
        }
        RawError::UnexpectedBehavior(reason) | RawError::Limited(reason) => {
            SftpError::Protocol(reason)
        }
        RawError::UnexpectedPacket => SftpError::Protocol("unexpected packet".into()),
    }
}

fn is_session_gone(reason: &str) -> bool {
    ["SendError", "RecvError", "sender dropped", "session closed"]
        .iter()
        .any(|m| reason.contains(m))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoteKind {
    File,
    Dir,
    Symlink,
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RemoteAttrs {
    pub kind: RemoteKind,
    pub size: u64,
    /// Seconds since the epoch, when the server reports it.
    pub mtime: Option<u64>,
}

impl From<&FileAttributes> for RemoteAttrs {
    fn from(a: &FileAttributes) -> Self {
        let kind = match a.file_type() {
            FileType::File => RemoteKind::File,
            FileType::Dir => RemoteKind::Dir,
            FileType::Symlink => RemoteKind::Symlink,
            FileType::Other => RemoteKind::Other,
        };
        Self {
            kind,
            size: a.size.unwrap_or(0),
            mtime: a.mtime.map(u64::from),
        }
    }
}

/// One SFTP session. Clones share the session and its in-flight limit.
#[derive(Clone)]
pub struct Sftp {
    inner: Arc<Inner>,
}

struct Inner {
    raw: RawSftpSession,
    max_read: u32,
    max_write: u32,
    fsync: bool,
    posix_rename: bool,
    statvfs: bool,
    permits: Semaphore,
    max_dir_entries: usize,
    /// Set once the channel stream ends (see `Watched`).
    gone: watch::Receiver<bool>,
    _conn: Arc<dyn Send + Sync>,
}

impl Sftp {
    pub async fn start(conn: &SshConn, opts: SftpOptions) -> Result<Self, SftpError> {
        let (gone_tx, gone) = watch::channel(false);
        let raw = RawSftpSession::new(Watched {
            inner: conn.open_sftp_channel().await?,
            gone: gone_tx,
        });
        raw.set_timeout(opts.request_timeout.as_secs().max(1));
        let version = guarded(&gone, raw.init())
            .await
            .map_err(|e| map_err(e, "<init>"))?;
        let has = |ext: &str| version.extensions.contains_key(ext);
        // limits@openssh.com reports 0 for "unknown"; fall back to the protocol minimum then.
        let clamp = |n: u64| match n {
            0 => FALLBACK_LEN,
            n => u32::try_from(n).unwrap_or(MAX_LEN).min(MAX_LEN),
        };
        let (max_read, max_write) = if has("limits@openssh.com") {
            let l = guarded(&gone, raw.limits())
                .await
                .map_err(|e| map_err(e, "<limits>"))?;
            (clamp(l.max_read_len), clamp(l.max_write_len))
        } else {
            (FALLBACK_LEN, FALLBACK_LEN)
        };
        Ok(Self {
            inner: Arc::new(Inner {
                raw,
                max_read,
                max_write,
                fsync: has("fsync@openssh.com"),
                posix_rename: has("posix-rename@openssh.com"),
                statvfs: has("statvfs@openssh.com"),
                permits: Semaphore::new(opts.inflight.max(1)),
                max_dir_entries: opts.max_dir_entries,
                gone,
                _conn: conn.keepalive_handle(),
            }),
        })
    }

    pub fn max_read(&self) -> u32 {
        self.inner.max_read
    }

    pub fn max_write(&self) -> u32 {
        self.inner.max_write
    }

    pub async fn open(&self, path: &str, mode: RemoteOpenMode) -> Result<RemoteFile, SftpError> {
        let flags = match mode {
            RemoteOpenMode::Read => OpenFlags::READ,
            RemoteOpenMode::Write => OpenFlags::WRITE,
            RemoteOpenMode::WriteCreate => OpenFlags::WRITE | OpenFlags::CREATE,
            RemoteOpenMode::ReadWrite => OpenFlags::READ | OpenFlags::WRITE,
            RemoteOpenMode::ReadWriteCreate => {
                OpenFlags::READ | OpenFlags::WRITE | OpenFlags::CREATE
            }
        };
        let _permit = self.permit().await?;
        let handle = guarded(
            &self.inner.gone,
            self.inner.raw.open(path, flags, FileAttributes::empty()),
        )
        .await
        .map_err(|e| map_err(e, path))?
        .handle;
        Ok(RemoteFile {
            sftp: self.clone(),
            handle,
            path: path.to_owned(),
        })
    }

    /// Attributes of `path` itself; a symlink is reported as a symlink, never followed.
    pub async fn lstat(&self, path: &str) -> Result<RemoteAttrs, SftpError> {
        let _permit = self.permit().await?;
        let attrs = guarded(&self.inner.gone, self.inner.raw.lstat(path))
            .await
            .map_err(|e| map_err(e, path))?
            .attrs;
        Ok(RemoteAttrs::from(&attrs))
    }

    /// Sets the modification (and access) time, in seconds since the epoch. SFTP v3 carries 32-bit
    /// times, so values past 2106 are clamped.
    pub async fn set_mtime(&self, path: &str, mtime: u64) -> Result<(), SftpError> {
        let t = u32::try_from(mtime).unwrap_or(u32::MAX);
        let attrs = FileAttributes {
            atime: Some(t),
            mtime: Some(t),
            ..FileAttributes::empty()
        };
        let _permit = self.permit().await?;
        guarded(&self.inner.gone, self.inner.raw.setstat(path, attrs))
            .await
            .map_err(|e| map_err(e, path))?;
        Ok(())
    }

    /// Sets the permission bits of `path` (key installation: `~/.ssh` 0700, `authorized_keys`
    /// 0600, as sshd's StrictModes requires).
    pub async fn chmod(&self, path: &str, mode: u32) -> Result<(), SftpError> {
        let attrs = FileAttributes {
            permissions: Some(mode),
            ..FileAttributes::empty()
        };
        let _permit = self.permit().await?;
        guarded(&self.inner.gone, self.inner.raw.setstat(path, attrs))
            .await
            .map_err(|e| map_err(e, path))?;
        Ok(())
    }

    /// Renames `from` to `to`. With `replace`, an existing `to` is replaced atomically
    /// (`posix-rename@openssh.com`, required). Without it, an existing `to` makes the rename fail:
    /// plain SFTP v3 rename, which OpenSSH implements as link + unlink.
    pub async fn rename(&self, from: &str, to: &str, replace: bool) -> Result<(), SftpError> {
        let _permit = self.permit().await?;
        if !replace {
            guarded(&self.inner.gone, self.inner.raw.rename(from, to))
                .await
                .map_err(|e| map_err(e, to))?;
            return Ok(());
        }
        if !self.inner.posix_rename {
            return Err(SftpError::Unsupported("posix-rename@openssh.com"));
        }
        let mut data = Vec::with_capacity(8 + from.len() + to.len());
        for s in [from, to] {
            let len =
                u32::try_from(s.len()).map_err(|_| SftpError::Protocol("path too long".into()))?;
            data.extend_from_slice(&len.to_be_bytes());
            data.extend_from_slice(s.as_bytes());
        }
        match guarded(
            &self.inner.gone,
            self.inner.raw.extended("posix-rename@openssh.com", data),
        )
        .await
        {
            Ok(Packet::Status(st)) if st.status_code == StatusCode::Ok => Ok(()),
            Ok(Packet::Status(st)) => Err(map_err(RawError::Status(st), to)),
            Ok(_) => Err(SftpError::Protocol(
                "unexpected reply to posix-rename".into(),
            )),
            Err(e) => Err(map_err(e, to)),
        }
    }

    /// Lists a directory: names (without `.` and `..`) with their attributes, symlinks unfollowed.
    pub async fn read_dir(&self, path: &str) -> Result<Vec<(String, RemoteAttrs)>, SftpError> {
        let handle = {
            let _permit = self.permit().await?;
            guarded(&self.inner.gone, self.inner.raw.opendir(path))
                .await
                .map_err(|e| map_err(e, path))?
                .handle
        };
        let mut out = Vec::new();
        let listed = loop {
            let _permit = self.permit().await?;
            match guarded(&self.inner.gone, self.inner.raw.readdir(handle.as_str())).await {
                Ok(name) => {
                    out.extend(
                        name.files
                            .into_iter()
                            .filter(|f| f.filename != "." && f.filename != "..")
                            .map(|f| (f.filename, RemoteAttrs::from(&f.attrs))),
                    );
                    let max = self.inner.max_dir_entries;
                    if out.len() > max {
                        let path = path.to_owned();
                        break Err(SftpError::ListingTooLong { path, max });
                    }
                }
                Err(RawError::Status(s)) if s.status_code == StatusCode::Eof => break Ok(()),
                Err(e) => break Err(map_err(e, path)),
            }
        };
        let _permit = self.permit().await?;
        let closed = guarded(&self.inner.gone, self.inner.raw.close(handle.as_str())).await;
        listed?;
        closed.map_err(|e| map_err(e, path))?;
        Ok(out)
    }

    /// Creates a directory. Returns false if it already existed as a directory (SFTP v3 reports
    /// EEXIST only as a generic failure, so that case is confirmed with `lstat`).
    pub async fn mkdir(&self, path: &str) -> Result<bool, SftpError> {
        let made = {
            let _permit = self.permit().await?;
            guarded(
                &self.inner.gone,
                self.inner.raw.mkdir(path, FileAttributes::empty()),
            )
            .await
        };
        match made {
            Ok(_) => Ok(true),
            Err(e) => match self.lstat(path).await {
                Ok(a) if a.kind == RemoteKind::Dir => Ok(false),
                _ => Err(map_err(e, path)),
            },
        }
    }

    /// Bytes available to this user on the filesystem holding `path` (`statvfs@openssh.com`), or
    /// None when the server does not offer the extension.
    pub async fn available_bytes(&self, path: &str) -> Result<Option<u64>, SftpError> {
        if !self.inner.statvfs {
            return Ok(None);
        }
        let _permit = self.permit().await?;
        let st = guarded(&self.inner.gone, self.inner.raw.statvfs(path))
            .await
            .map_err(|e| map_err(e, path))?;
        Ok(Some(st.blocks_avail.saturating_mul(st.fragment_size)))
    }

    pub async fn remove(&self, path: &str) -> Result<(), SftpError> {
        let _permit = self.permit().await?;
        guarded(&self.inner.gone, self.inner.raw.remove(path))
            .await
            .map_err(|e| map_err(e, path))?;
        Ok(())
    }

    async fn permit(&self) -> Result<tokio::sync::SemaphorePermit<'_>, SftpError> {
        self.inner
            .permits
            .acquire()
            .await
            .map_err(|_| SftpError::ConnectionLost("session closed".into()))
    }
}

/// An open remote file. Methods take `&self`, so callers can share it (for example in an `Arc`)
/// and keep several requests in flight at different offsets.
pub struct RemoteFile {
    sftp: Sftp,
    handle: String,
    path: String,
}

impl RemoteFile {
    pub fn path(&self) -> &str {
        &self.path
    }

    /// Writes all of `data` at `offset`, in pieces no larger than the server's write limit.
    pub async fn write_at(&self, offset: u64, data: &[u8]) -> Result<(), SftpError> {
        let max = self.sftp.max_write() as usize;
        for (i, piece) in data.chunks(max).enumerate() {
            let at = offset + (i * max) as u64;
            let _permit = self.sftp.permit().await?;
            // russh-sftp takes an owned Vec, so each piece is copied once here.
            guarded(
                &self.sftp.inner.gone,
                self.sftp
                    .inner
                    .raw
                    .write(self.handle.as_str(), at, piece.to_vec()),
            )
            .await
            .map_err(|e| map_err(e, &self.path))?;
        }
        Ok(())
    }

    /// Reads into `buf` from `offset` until `buf` is full or the file ends. Returns bytes read;
    /// 0 means `offset` is at or past the end.
    pub async fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, SftpError> {
        let max = self.sftp.max_read() as usize;
        let mut done = 0;
        while done < buf.len() {
            let want = (buf.len() - done).min(max) as u32;
            let _permit = self.sftp.permit().await?;
            let data = match guarded(
                &self.sftp.inner.gone,
                self.sftp
                    .inner
                    .raw
                    .read(self.handle.as_str(), offset + done as u64, want),
            )
            .await
            {
                Ok(d) => d.data,
                Err(RawError::Status(s)) if s.status_code == StatusCode::Eof => break,
                Err(e) => return Err(map_err(e, &self.path)),
            };
            if data.is_empty() {
                break;
            }
            // A server may return less than asked; never trust it to return more.
            let n = data.len().min(buf.len() - done);
            buf[done..done + n].copy_from_slice(&data[..n]);
            done += n;
        }
        Ok(done)
    }

    /// Asks the server to flush this file to disk (`fsync@openssh.com`).
    pub async fn fsync(&self) -> Result<(), SftpError> {
        if !self.sftp.inner.fsync {
            return Err(SftpError::Unsupported("fsync@openssh.com"));
        }
        let _permit = self.sftp.permit().await?;
        guarded(
            &self.sftp.inner.gone,
            self.sftp.inner.raw.fsync(self.handle.as_str()),
        )
        .await
        .map_err(|e| map_err(e, &self.path))?;
        Ok(())
    }

    /// Truncates or extends the file to `len` bytes.
    pub async fn set_len(&self, len: u64) -> Result<(), SftpError> {
        let attrs = FileAttributes {
            size: Some(len),
            ..FileAttributes::empty()
        };
        let _permit = self.sftp.permit().await?;
        guarded(
            &self.sftp.inner.gone,
            self.sftp.inner.raw.fsetstat(self.handle.as_str(), attrs),
        )
        .await
        .map_err(|e| map_err(e, &self.path))?;
        Ok(())
    }

    pub async fn close(self) -> Result<(), SftpError> {
        let _permit = self.sftp.permit().await?;
        guarded(
            &self.sftp.inner.gone,
            self.sftp.inner.raw.close(self.handle.as_str()),
        )
        .await
        .map_err(|e| map_err(e, &self.path))?;
        Ok(())
    }
}

/// Runs one request, failing it as soon as the channel is gone. russh-sftp 3.0.1 wakes pending
/// requests when its read loop ends, but a request sent between that and its write loop noticing
/// is accepted and never answered: without this it waits out the whole request timeout.
async fn guarded<T>(
    gone: &watch::Receiver<bool>,
    request: impl Future<Output = Result<T, RawError>>,
) -> Result<T, RawError> {
    let mut gone = gone.clone();
    tokio::select! {
        biased;
        r = request => r,
        // An error means the stream (the sender) was dropped: gone as well.
        _ = gone.wait_for(|g| *g) => Err(RawError::UnexpectedBehavior("session closed".into())),
    }
}

/// The SFTP channel stream, flagging `gone` at end of stream, on any I/O error and when dropped.
/// The flag is set while russh-sftp's read loop is still returning, so no request can slip in
/// unflagged after it cleared its pending requests.
struct Watched<S> {
    inner: S,
    gone: watch::Sender<bool>,
}

impl<S> Watched<S> {
    fn flag<T>(&self, r: &Poll<io::Result<T>>) {
        if matches!(r, Poll::Ready(Err(_))) {
            self.gone.send_replace(true);
        }
    }
}

impl<S> Drop for Watched<S> {
    fn drop(&mut self) {
        self.gone.send_replace(true);
    }
}

impl<S: AsyncRead + Unpin> AsyncRead for Watched<S> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let (before, room) = (buf.filled().len(), buf.remaining());
        let r = Pin::new(&mut self.inner).poll_read(cx, buf);
        let eof = matches!(r, Poll::Ready(Ok(()))) && room > 0 && buf.filled().len() == before;
        if eof {
            self.gone.send_replace(true);
        }
        self.flag(&r);
        r
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for Watched<S> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let r = Pin::new(&mut self.inner).poll_write(cx, buf);
        self.flag(&r);
        r
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let r = Pin::new(&mut self.inner).poll_flush(cx);
        self.flag(&r);
        r
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let r = Pin::new(&mut self.inner).poll_shutdown(cx);
        self.flag(&r);
        r
    }
}
