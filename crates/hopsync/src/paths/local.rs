//! `LocalRoot`: every local file and directory is opened relative to a root directory handle, and the
//! kernel refuses any resolution that leaves the root or passes through a symlink.
//!
//! Uses `openat2(RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS | RESOLVE_NO_MAGICLINKS)` (Linux 5.6+). Older
//! kernels, and sandboxes that hide `openat2` (ENOSYS), get a component-by-component walk with
//! `O_NOFOLLOW` on every step. The walk is safe because `RelPath` components can never be `.`, `..`
//! or contain `/`, so the only escape route left is a symlink, which `O_NOFOLLOW` refuses.

use std::ffi::OsString;
use std::fs::File;
use std::io;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::os::unix::ffi::OsStringExt;
use std::path::{Path, PathBuf};

use rustix::fs::{AtFlags, FileType, Mode, OFlags, RenameFlags, ResolveFlags};
use rustix::io::Errno;

use super::RelPath;

/// Permissions for created files and directories, before the process umask.
const FILE_MODE: u32 = 0o644;
const DIR_MODE: u32 = 0o755;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenMode {
    Read,
    /// Write, creating the file if missing. Never truncates: partial files are resumed.
    WriteCreate,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    File,
    Dir,
    Symlink,
    /// Sockets, FIFOs, devices.
    Other,
}

impl From<FileType> for EntryKind {
    fn from(t: FileType) -> Self {
        match t {
            FileType::RegularFile => Self::File,
            FileType::Directory => Self::Dir,
            FileType::Symlink => Self::Symlink,
            _ => Self::Other,
        }
    }
}

/// A directory entry as the filesystem has it: the name is raw bytes, not yet validated.
#[derive(Debug, Clone)]
pub struct DirEntry {
    pub name: OsString,
    pub kind: EntryKind,
}

/// What a root keeps out of reach at its top level, however a path is asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Hidden {
    #[default]
    Nothing,
    /// Every hidden entry, for a home folder: `.ssh`, `.bashrc` and `.config` would let a web UI
    /// login run commands, and Hopsync's own settings and secrets live below `.local`.
    DotEntries,
    /// `/proc`, `/sys`, `/dev` and `/run`, for a root of `/`: walking them hangs or reads devices.
    KernelFilesystems,
}

impl Hidden {
    /// What full access over `root` hides.
    pub fn for_full_access(root: &Path) -> Self {
        if root == Path::new("/") {
            Self::KernelFilesystems
        } else {
            Self::DotEntries
        }
    }

    fn covers(self, top: &str) -> bool {
        match self {
            Self::Nothing => false,
            Self::DotEntries => top.starts_with('.'),
            Self::KernelFilesystems => matches!(top, "proc" | "sys" | "dev" | "run"),
        }
    }
}

pub struct LocalRoot {
    dir: OwnedFd,
    path: PathBuf,
    use_openat2: bool,
    hidden: Hidden,
}

impl LocalRoot {
    /// Opens the root chosen by the user. Symlinks in this path itself are followed: the user picked it.
    pub fn open(path: &Path) -> io::Result<Self> {
        Self::open_hiding(path, Hidden::Nothing)
    }

    /// Like `open`, with `hidden` entries unlisted and refused.
    pub fn open_hiding(path: &Path, hidden: Hidden) -> io::Result<Self> {
        let dir = rustix::fs::open(path, dir_flags(), Mode::empty())?;
        Ok(Self {
            dir,
            path: path.to_owned(),
            use_openat2: true,
            hidden,
        })
    }

    /// Refuses a path whose first component is hidden. Every path-taking method passes through here.
    fn reachable(&self, rel: &RelPath) -> io::Result<()> {
        match rel.components().first() {
            Some(top) if self.hidden.covers(top.as_str()) => Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "hidden by full access",
            )),
            _ => Ok(()),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Opens `rel` (a file, never the root itself) beneath the root.
    pub fn open_file(&self, rel: &RelPath, mode: OpenMode) -> io::Result<File> {
        if rel.components().is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "the root is not a file",
            ));
        }
        let base = OFlags::CLOEXEC | OFlags::NOCTTY;
        // openat2 rejects a non-zero mode without O_CREAT (EINVAL), unlike openat.
        let (oflags, create_mode) = match mode {
            OpenMode::Read => (base | OFlags::RDONLY, Mode::empty()),
            OpenMode::WriteCreate => (
                base | OFlags::WRONLY | OFlags::CREATE,
                Mode::from_raw_mode(FILE_MODE),
            ),
        };
        Ok(File::from(self.open_beneath(rel, oflags, create_mode)?))
    }

    /// Creates every directory of `rel` that does not exist yet. Existing symlinks are never followed.
    pub fn create_dir_all(&self, rel: &RelPath) -> io::Result<()> {
        self.reachable(rel)?;
        // Single-component steps with O_NOFOLLOW are safe without openat2: a component cannot be
        // `..`, so the only way out is a symlink, and an existing symlink fails the O_DIRECTORY open.
        let mut cur: Option<OwnedFd> = None;
        for name in rel.components() {
            let parent = cur.as_ref().map_or(self.dir.as_fd(), AsFd::as_fd);
            match rustix::fs::mkdirat(parent, name.as_str(), Mode::from_raw_mode(DIR_MODE)) {
                Ok(()) | Err(Errno::EXIST) => {}
                Err(e) => return Err(e.into()),
            }
            let next = rustix::fs::openat(
                parent,
                name.as_str(),
                dir_flags() | OFlags::NOFOLLOW,
                Mode::empty(),
            )?;
            cur = Some(next);
        }
        Ok(())
    }

    /// Metadata of `rel` itself: a symlink is reported as a symlink, never followed. The empty path is
    /// the root.
    pub fn metadata(&self, rel: &RelPath) -> io::Result<std::fs::Metadata> {
        let flags = OFlags::PATH | OFlags::CLOEXEC | OFlags::NOFOLLOW;
        let fd = if rel.components().is_empty() {
            rustix::fs::openat(&self.dir, ".", flags, Mode::empty())?
        } else {
            // With O_PATH | O_NOFOLLOW, openat2 returns a final symlink itself instead of ELOOP.
            self.open_beneath(rel, flags, Mode::empty())?
        };
        File::from(fd).metadata()
    }

    /// Renames `from` to `to`, both beneath the root. Without `replace`, an existing `to` fails with
    /// `AlreadyExists` (`renameat2(RENAME_NOREPLACE)`, atomic). On a filesystem without
    /// RENAME_NOREPLACE it falls back to check-then-rename, which can race with another writer.
    pub fn rename(&self, from: &RelPath, to: &RelPath, replace: bool) -> io::Result<()> {
        let (Some(from_name), Some(to_name)) = (from.components().last(), to.components().last())
        else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "cannot rename the root",
            ));
        };
        let from_dir = self.open_parent(from, dir_flags())?;
        let to_dir = self.open_parent(to, dir_flags())?;
        let (a, b) = (from_name.as_str(), to_name.as_str());
        if replace {
            return Ok(rustix::fs::renameat(&from_dir, a, &to_dir, b)?);
        }
        match rustix::fs::renameat_with(&from_dir, a, &to_dir, b, RenameFlags::NOREPLACE) {
            Err(Errno::INVAL) => {
                let at = rustix::fs::AtFlags::SYMLINK_NOFOLLOW;
                match rustix::fs::statat(&to_dir, b, at) {
                    Ok(_) => Err(io::ErrorKind::AlreadyExists.into()),
                    Err(Errno::NOENT) => Ok(rustix::fs::renameat(&from_dir, a, &to_dir, b)?),
                    Err(e) => Err(e.into()),
                }
            }
            other => Ok(other?),
        }
    }

    /// Flushes the directory containing `rel`, so a create or rename in it survives a crash.
    pub fn sync_parent(&self, rel: &RelPath) -> io::Result<()> {
        let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC;
        File::from(self.open_parent(rel, flags)?).sync_all()
    }

    /// Flushes the whole filesystem holding `rel` (`syncfs`): one call instead of an fsync per file
    /// and directory at the end of a pull. A root of `/` can hold many filesystems.
    pub fn sync_fs(&self, rel: &RelPath) -> io::Result<()> {
        Ok(rustix::fs::syncfs(self.nearest_dir(rel)?)?)
    }

    /// Bytes available to this user on the filesystem that holds, or will hold, `rel`.
    pub fn available_bytes(&self, rel: &RelPath) -> io::Result<u64> {
        let st = rustix::fs::fstatvfs(self.nearest_dir(rel)?)?;
        Ok(st.f_bavail.saturating_mul(st.f_frsize))
    }

    /// `rel`, or its closest parent that exists: a destination is often not created yet. Opened for
    /// reading, because the root's own handle is `O_PATH` and `syncfs` refuses that.
    fn nearest_dir(&self, rel: &RelPath) -> io::Result<OwnedFd> {
        let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW;
        let parts = rel.components();
        let found = (1..=parts.len()).rev().find_map(|n| {
            self.open_beneath(&RelPath(parts[..n].to_vec()), flags, Mode::empty())
                .ok()
        });
        match found {
            Some(fd) => Ok(fd),
            None => Ok(rustix::fs::openat(&self.dir, ".", flags, Mode::empty())?),
        }
    }

    /// A second handle on the same root (the directory fd is duplicated), for use from other tasks.
    pub fn try_clone(&self) -> io::Result<Self> {
        Ok(Self {
            dir: self.dir.try_clone()?,
            path: self.path.clone(),
            use_openat2: self.use_openat2,
            hidden: self.hidden,
        })
    }

    /// Lists the directory `rel` (empty: the root). Entries are not followed: a symlink is reported
    /// as `Symlink`. Fails if `rel` is not a directory or passes through a symlink.
    pub fn read_dir(&self, rel: &RelPath) -> io::Result<Vec<DirEntry>> {
        let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW;
        let fd = if rel.components().is_empty() {
            rustix::fs::openat(&self.dir, ".", flags, Mode::empty())?
        } else {
            self.open_beneath(rel, flags, Mode::empty())?
        };
        let mut out = Vec::new();
        for entry in rustix::fs::Dir::read_from(&fd)? {
            let entry = entry?;
            let name = entry.file_name().to_bytes();
            if name == b"." || name == b".." {
                continue;
            }
            if rel.components().is_empty()
                && std::str::from_utf8(name).is_ok_and(|n| self.hidden.covers(n))
            {
                continue;
            }
            let ty = match entry.file_type() {
                // Some filesystems leave d_type empty; ask for this entry only.
                FileType::Unknown => {
                    let st = rustix::fs::statat(&fd, entry.file_name(), AtFlags::SYMLINK_NOFOLLOW)?;
                    FileType::from_raw_mode(st.st_mode)
                }
                t => t,
            };
            out.push(DirEntry {
                name: OsString::from_vec(name.to_vec()),
                kind: EntryKind::from(ty),
            });
        }
        Ok(out)
    }

    /// Creates the directory `rel` (its parent must exist). Returns false if it already existed as a
    /// directory; anything else in the way, a symlink included, is an error.
    pub fn create_dir(&self, rel: &RelPath) -> io::Result<bool> {
        let Some(name) = rel.components().last() else {
            return Ok(false);
        };
        let parent = self.open_parent(rel, dir_flags())?;
        match rustix::fs::mkdirat(&parent, name.as_str(), Mode::from_raw_mode(DIR_MODE)) {
            Ok(()) => Ok(true),
            Err(Errno::EXIST) => {
                let st = rustix::fs::statat(&parent, name.as_str(), AtFlags::SYMLINK_NOFOLLOW)?;
                if FileType::from_raw_mode(st.st_mode) == FileType::Directory {
                    Ok(false)
                } else {
                    Err(io::Error::new(
                        io::ErrorKind::AlreadyExists,
                        "exists and is not a directory",
                    ))
                }
            }
            Err(e) => Err(e.into()),
        }
    }

    /// Removes the file `rel`. A missing file is not an error.
    pub fn remove_file(&self, rel: &RelPath) -> io::Result<()> {
        let Some(name) = rel.components().last() else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "cannot remove the root",
            ));
        };
        let parent = self.open_parent(rel, dir_flags())?;
        match rustix::fs::unlinkat(&parent, name.as_str(), AtFlags::empty()) {
            Ok(()) | Err(Errno::NOENT) => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    /// Opens the directory containing `rel` (the root for a single component).
    fn open_parent(&self, rel: &RelPath, oflags: OFlags) -> io::Result<OwnedFd> {
        self.reachable(rel)?;
        let parents = rel.components().split_last().map_or(&[][..], |(_, p)| p);
        let oflags = oflags | OFlags::DIRECTORY | OFlags::NOFOLLOW;
        if parents.is_empty() {
            return Ok(rustix::fs::openat(&self.dir, ".", oflags, Mode::empty())?);
        }
        self.open_beneath(&RelPath(parents.to_vec()), oflags, Mode::empty())
    }

    /// Opens `rel` beneath the root, refusing symlinks anywhere on the way.
    fn open_beneath(&self, rel: &RelPath, oflags: OFlags, mode: Mode) -> io::Result<OwnedFd> {
        self.reachable(rel)?;
        if self.use_openat2 {
            let resolve =
                ResolveFlags::BENEATH | ResolveFlags::NO_SYMLINKS | ResolveFlags::NO_MAGICLINKS;
            match rustix::fs::openat2(&self.dir, rel.to_slash_string(), oflags, mode, resolve) {
                // Kernel older than 5.6, or a seccomp filter hiding the syscall: fall back to the walk.
                Err(Errno::NOSYS) => {}
                other => return other.map_err(Into::into),
            }
        }
        self.open_by_walk(rel, oflags, mode)
    }

    fn open_by_walk(&self, rel: &RelPath, oflags: OFlags, mode: Mode) -> io::Result<OwnedFd> {
        let Some((last, parents)) = rel.components().split_last() else {
            return Err(io::ErrorKind::InvalidInput.into());
        };
        let mut cur: Option<OwnedFd> = None;
        for name in parents {
            let parent: BorrowedFd<'_> = cur.as_ref().map_or(self.dir.as_fd(), AsFd::as_fd);
            let next = rustix::fs::openat(
                parent,
                name.as_str(),
                dir_flags() | OFlags::NOFOLLOW,
                Mode::empty(),
            )?;
            cur = Some(next);
        }
        let parent = cur.as_ref().map_or(self.dir.as_fd(), AsFd::as_fd);
        Ok(rustix::fs::openat(
            parent,
            last.as_str(),
            oflags | OFlags::NOFOLLOW,
            mode,
        )?)
    }
}

/// A directory handle usable only as a base for further *at() calls.
fn dir_flags() -> OFlags {
    OFlags::PATH | OFlags::DIRECTORY | OFlags::CLOEXEC
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::os::unix::fs::symlink;

    fn rel(s: &str) -> RelPath {
        RelPath::parse(s).expect("valid rel path")
    }

    /// Runs `f` once with openat2 and once with the fallback walk.
    fn both_modes(f: impl Fn(&LocalRoot, &Path)) {
        for use_openat2 in [true, false] {
            let dir = tempfile::tempdir().expect("tempdir");
            let mut root = LocalRoot::open(dir.path()).expect("open root");
            root.use_openat2 = use_openat2;
            f(&root, dir.path());
        }
    }

    #[test]
    fn writes_and_reads_nested_file() {
        both_modes(|root, dir| {
            root.create_dir_all(&rel("Show/Season 1")).expect("mkdir");
            let mut f = root
                .open_file(&rel("Show/Season 1/E01.mkv"), OpenMode::WriteCreate)
                .expect("create");
            f.write_all(b"data").expect("write");
            assert_eq!(
                std::fs::read(dir.join("Show/Season 1/E01.mkv")).expect("read back"),
                b"data"
            );
            let mut s = String::new();
            root.open_file(&rel("Show/Season 1/E01.mkv"), OpenMode::Read)
                .expect("open")
                .read_to_string(&mut s)
                .expect("read");
            assert_eq!(s, "data");
        });
    }

    #[test]
    fn write_create_does_not_truncate() {
        both_modes(|root, dir| {
            std::fs::write(dir.join("part"), b"0123456789").expect("seed");
            let mut f = root
                .open_file(&rel("part"), OpenMode::WriteCreate)
                .expect("open");
            f.write_all(b"ab").expect("write");
            assert_eq!(
                std::fs::read(dir.join("part")).expect("read"),
                b"ab23456789"
            );
        });
    }

    #[test]
    fn refuses_symlinked_directory_escaping_root() {
        both_modes(|root, dir| {
            let outside = tempfile::tempdir().expect("outside");
            std::fs::write(outside.path().join("secret"), b"x").expect("seed");
            symlink(outside.path(), dir.join("link")).expect("symlink");
            assert!(root.open_file(&rel("link/secret"), OpenMode::Read).is_err());
            assert!(
                root.open_file(&rel("link/new"), OpenMode::WriteCreate)
                    .is_err()
            );
            assert!(
                !outside.path().join("new").exists(),
                "nothing may be created outside the root"
            );
        });
    }

    #[test]
    fn refuses_final_symlink_even_inside_root() {
        both_modes(|root, dir| {
            std::fs::write(dir.join("real"), b"x").expect("seed");
            symlink(dir.join("real"), dir.join("alias")).expect("symlink");
            assert!(root.open_file(&rel("alias"), OpenMode::Read).is_err());
            assert!(
                root.open_file(&rel("alias"), OpenMode::WriteCreate)
                    .is_err()
            );
        });
    }

    #[test]
    fn create_dir_all_refuses_symlinked_intermediate() {
        both_modes(|root, dir| {
            let outside = tempfile::tempdir().expect("outside");
            symlink(outside.path(), dir.join("link")).expect("symlink");
            assert!(root.create_dir_all(&rel("link/sub")).is_err());
            assert!(
                !outside.path().join("sub").exists(),
                "nothing may be created outside the root"
            );
        });
    }

    #[test]
    fn create_dir_all_is_idempotent() {
        both_modes(|root, dir| {
            root.create_dir_all(&rel("a/b")).expect("first");
            root.create_dir_all(&rel("a/b")).expect("second");
            root.create_dir_all(&RelPath::default())
                .expect("root itself");
            assert!(dir.join("a/b").is_dir());
        });
    }

    #[test]
    fn open_file_rejects_the_root_itself() {
        both_modes(|root, _| {
            let err = root
                .open_file(&RelPath::default(), OpenMode::Read)
                .expect_err("root is not a file");
            assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        });
    }

    #[test]
    fn root_may_be_reached_through_a_symlink() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir(dir.path().join("real")).expect("mkdir");
        symlink(dir.path().join("real"), dir.path().join("via")).expect("symlink");
        let root =
            LocalRoot::open(&dir.path().join("via")).expect("user-chosen root may be a symlink");
        root.open_file(&rel("f"), OpenMode::WriteCreate)
            .expect("create");
        assert!(dir.path().join("real/f").exists());
    }

    #[test]
    fn metadata_does_not_follow_symlinks() {
        both_modes(|root, dir| {
            std::fs::write(dir.join("f"), b"abc").expect("write");
            symlink("/etc/hostname", dir.join("link")).expect("symlink");
            let m = root.metadata(&rel("f")).expect("file");
            assert!(m.is_file() && m.len() == 3);
            assert!(
                root.metadata(&rel("link"))
                    .expect("link")
                    .file_type()
                    .is_symlink()
            );
            let err = root.metadata(&rel("missing")).expect_err("missing");
            assert_eq!(err.kind(), io::ErrorKind::NotFound);
            assert!(root.metadata(&RelPath::default()).expect("root").is_dir());
        });
    }

    #[test]
    fn rename_with_and_without_replace() {
        both_modes(|root, dir| {
            std::fs::create_dir(dir.join("d")).expect("mkdir");
            std::fs::write(dir.join("d/a"), b"new").expect("write");
            std::fs::write(dir.join("d/b"), b"old").expect("write");
            let err = root
                .rename(&rel("d/a"), &rel("d/b"), false)
                .expect_err("exists");
            assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
            assert_eq!(std::fs::read(dir.join("d/b")).expect("read"), b"old");
            root.rename(&rel("d/a"), &rel("d/b"), true)
                .expect("replace");
            assert_eq!(std::fs::read(dir.join("d/b")).expect("read"), b"new");
            root.rename(&rel("d/b"), &rel("c"), false).expect("move up");
            assert_eq!(std::fs::read(dir.join("c")).expect("read"), b"new");
            root.sync_parent(&rel("c")).expect("sync root dir");
            root.sync_parent(&rel("d/x")).expect("sync subdir");
        });
    }

    #[test]
    fn rename_refuses_symlinked_parent() {
        both_modes(|root, dir| {
            let outside = tempfile::tempdir().expect("outside");
            symlink(outside.path(), dir.join("esc")).expect("symlink");
            std::fs::write(dir.join("a"), b"x").expect("write");
            assert!(root.rename(&rel("a"), &rel("esc/a"), false).is_err());
            assert!(!outside.path().join("a").exists());
        });
    }

    #[test]
    fn read_dir_lists_kinds_without_following() {
        both_modes(|root, dir| {
            std::fs::create_dir_all(dir.join("d/sub")).expect("mkdir");
            std::fs::write(dir.join("d/f"), b"x").expect("write");
            symlink("/etc", dir.join("d/link")).expect("symlink");
            let mut got: Vec<(String, EntryKind)> = root
                .read_dir(&rel("d"))
                .expect("read_dir")
                .into_iter()
                .map(|e| (e.name.to_string_lossy().into_owned(), e.kind))
                .collect();
            got.sort_by(|a, b| a.0.cmp(&b.0));
            assert_eq!(
                got,
                vec![
                    ("f".into(), EntryKind::File),
                    ("link".into(), EntryKind::Symlink),
                    ("sub".into(), EntryKind::Dir),
                ]
            );
            assert_eq!(root.read_dir(&RelPath::default()).expect("root").len(), 1);
            symlink(dir.join("d"), dir.join("dlink")).expect("symlink");
            assert!(
                root.read_dir(&rel("dlink")).is_err(),
                "must not list through a symlink"
            );
        });
    }

    #[test]
    fn create_dir_reports_created_or_existing() {
        both_modes(|root, dir| {
            assert!(root.create_dir(&rel("a")).expect("create"));
            assert!(!root.create_dir(&rel("a")).expect("exists"));
            assert!(root.create_dir(&rel("a/b")).expect("nested"));
            std::fs::write(dir.join("file"), b"").expect("write");
            assert!(
                root.create_dir(&rel("file")).is_err(),
                "a file is in the way"
            );
            symlink("/tmp", dir.join("l")).expect("symlink");
            assert!(
                root.create_dir(&rel("l")).is_err(),
                "a symlink is not a directory"
            );
            assert!(dir.join("a/b").is_dir());
        });
    }

    #[test]
    fn try_clone_shares_the_root() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = LocalRoot::open(dir.path()).expect("open");
        let copy = root.try_clone().expect("clone");
        drop(root);
        assert!(copy.create_dir(&rel("x")).expect("create"));
        assert!(dir.path().join("x").is_dir());
    }

    fn hiding(hidden: Hidden) -> (tempfile::TempDir, LocalRoot) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join(".ssh")).unwrap();
        std::fs::write(dir.path().join(".ssh/authorized_keys"), b"key").unwrap();
        std::fs::write(dir.path().join(".bashrc"), b"rc").unwrap();
        std::fs::create_dir(dir.path().join("media")).unwrap();
        std::fs::write(dir.path().join("media/.keep"), b"k").unwrap();
        std::fs::create_dir(dir.path().join("proc")).unwrap();
        let root = LocalRoot::open_hiding(dir.path(), hidden).unwrap();
        (dir, root)
    }

    fn names(root: &LocalRoot, rel: &RelPath) -> Vec<String> {
        let mut n: Vec<String> = root
            .read_dir(rel)
            .unwrap()
            .into_iter()
            .map(|e| e.name.to_string_lossy().into_owned())
            .collect();
        n.sort();
        n
    }

    #[test]
    fn a_hidden_entry_is_unlisted_and_refused_for_every_operation() {
        let (dir, root) = hiding(Hidden::DotEntries);
        assert_eq!(names(&root, &RelPath::default()), ["media", "proc"]);
        let denied = |r: io::Result<()>| {
            assert_eq!(r.unwrap_err().kind(), io::ErrorKind::PermissionDenied);
        };
        let keys = RelPath::parse(".ssh/authorized_keys").unwrap();
        let rc = RelPath::parse(".bashrc").unwrap();
        let elsewhere = RelPath::parse("media/x").unwrap();
        denied(root.open_file(&keys, OpenMode::Read).map(drop));
        denied(root.open_file(&keys, OpenMode::WriteCreate).map(drop));
        denied(root.open_file(&rc, OpenMode::WriteCreate).map(drop));
        denied(root.metadata(&keys).map(drop));
        denied(root.read_dir(&RelPath::parse(".ssh").unwrap()).map(drop));
        denied(
            root.create_dir(&RelPath::parse(".config").unwrap())
                .map(drop),
        );
        denied(root.create_dir_all(&RelPath::parse(".config/systemd").unwrap()));
        denied(root.rename(&elsewhere, &rc, true));
        denied(root.rename(&rc, &elsewhere, true));
        denied(
            root.try_clone()
                .unwrap()
                .open_file(&keys, OpenMode::Read)
                .map(drop),
        );
        assert_eq!(std::fs::read(dir.path().join(".bashrc")).unwrap(), b"rc");
    }

    #[test]
    fn only_the_top_level_is_hidden() {
        let (_dir, root) = hiding(Hidden::DotEntries);
        let media = RelPath::parse("media").unwrap();
        assert_eq!(names(&root, &media), [".keep"]);
        root.open_file(&RelPath::parse("media/.keep").unwrap(), OpenMode::Read)
            .unwrap();
    }

    #[test]
    fn a_root_of_slash_hides_kernel_filesystems_only() {
        let (_dir, root) = hiding(Hidden::KernelFilesystems);
        assert_eq!(
            names(&root, &RelPath::default()),
            [".bashrc", ".ssh", "media"]
        );
        assert!(root.read_dir(&RelPath::parse("proc").unwrap()).is_err());
        assert_eq!(
            Hidden::for_full_access(Path::new("/")),
            Hidden::KernelFilesystems
        );
        assert_eq!(
            Hidden::for_full_access(Path::new("/home/a")),
            Hidden::DotEntries
        );
    }

    #[test]
    fn free_space_is_read_where_the_destination_will_be() {
        let (_dir, root) = hiding(Hidden::Nothing);
        let absent = RelPath::parse("media/not/yet").unwrap();
        assert!(root.available_bytes(&absent).unwrap() > 0);
        root.sync_fs(&absent).unwrap();
    }
}
