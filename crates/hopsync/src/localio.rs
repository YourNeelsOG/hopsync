//! Local positional I/O with page-cache discipline (plan.md §6.3, CLAUDE.md §4).
//!
//! - Reads use `pread`, then `fadvise(DONTNEED)` on the range just read, so a 50 GB push does not
//!   evict the seedbox's working set (for example the torrent client's hot pieces).
//! - Writes go through `ChunkWriter`, which paces write-back: every `WINDOW` bytes it starts
//!   write-back of that window (`sync_file_range(WRITE)`), waits for the window before it, and drops
//!   its pages. Dirty memory per writer stays near two windows instead of growing with chunk size.
//! - `ChunkWriter::finish` returns once the chunk's data has been written back to the device. That is
//!   not durability: it persists neither the file size nor the drive's write cache. The resume
//!   journal calls `LocalWriter::sync_data` (`fdatasync`) at checkpoints for that.
//! - `CachePolicy::Keep` keeps the pacing but never drops pages, for files a torrent client is
//!   seeding (dropping them would cost it a re-read).
//!
//! `fadvise` is advisory, so its errors are ignored; write-back errors (EIO, ENOSPC) are returned.
//! These calls block; the engine runs them on blocking threads.

use std::fs::File;
use std::io;
use std::num::NonZeroU64;
use std::os::fd::AsFd;
use std::os::unix::fs::FileExt;
use std::sync::Arc;

use rustix::fs::{Advice, fadvise};

/// Write-back pacing window.
pub const WINDOW: u64 = 8 << 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CachePolicy {
    #[default]
    Drop,
    Keep,
}

pub struct LocalReader {
    file: File,
    cache: CachePolicy,
}

impl LocalReader {
    pub fn new(file: File, cache: CachePolicy) -> Self {
        Self { file, cache }
    }

    pub fn size(&self) -> io::Result<u64> {
        Ok(self.file.metadata()?.len())
    }

    /// The device the file lives on (`st_dev`).
    pub fn device(&self) -> io::Result<u64> {
        use std::os::unix::fs::MetadataExt;
        Ok(self.file.metadata()?.dev())
    }

    /// Tells the kernel the file is read front to back, so it reads further ahead. Advisory.
    pub fn advise_sequential(&self) {
        let _ = fadvise(self.file.as_fd(), 0, None, Advice::Sequential);
    }

    /// Fills `buf` from `offset` unless the file ends first. Returns bytes read; 0 at or past the end.
    pub fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
        let mut done = 0;
        while done < buf.len() {
            match self.file.read_at(&mut buf[done..], offset + done as u64) {
                Ok(0) => break,
                Ok(n) => done += n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        if self.cache == CachePolicy::Drop {
            drop_pages(&self.file, offset, done as u64);
        }
        Ok(done)
    }
}

impl Drop for LocalReader {
    /// Read-ahead from one lane can bring back pages another lane has already read and dropped, so
    /// ranges alone leave tens of MiB cached after a large file. One pass over the whole file ends it.
    fn drop(&mut self) {
        if self.cache == CachePolicy::Drop {
            let _ = fadvise(self.file.as_fd(), 0, None, Advice::DontNeed);
        }
    }
}

pub struct LocalWriter {
    file: Arc<File>,
    cache: CachePolicy,
}

impl LocalWriter {
    /// Reserves `len` bytes for the file without changing its size, so a hard drive can lay it out
    /// in few extents however the chunks arrive. Advisory: a filesystem without `fallocate` (or
    /// one that is full) is left to fail, or not, on the writes themselves.
    pub fn preallocate(&self, len: u64) {
        if len > 0 {
            let _ =
                rustix::fs::fallocate(&*self.file, rustix::fs::FallocateFlags::KEEP_SIZE, 0, len);
        }
    }

    pub fn new(file: File, cache: CachePolicy) -> Self {
        Self {
            file: Arc::new(file),
            cache,
        }
    }

    /// A sequential writer for one chunk starting at `offset`. Lanes may hold several at once, and
    /// each can move to a blocking thread on its own.
    pub fn chunk(&self, offset: u64) -> ChunkWriter {
        ChunkWriter {
            file: self.file.clone(),
            cache: self.cache,
            pos: offset,
            kicked: offset,
            settled: offset,
        }
    }

    /// Persists data and the file size (`fdatasync`). For journal checkpoints.
    pub fn sync_data(&self) -> io::Result<()> {
        self.file.sync_data()
    }

    /// The underlying file, for finishing touches (size, mtime) after all chunks are written.
    pub fn file(&self) -> &File {
        &self.file
    }
}

/// Writes one chunk sequentially. Invariant: `settled <= kicked <= pos`, where `[settled, kicked)`
/// is under write-back and `[kicked, pos)` is dirty and not yet started.
pub struct ChunkWriter {
    file: Arc<File>,
    cache: CachePolicy,
    pos: u64,
    kicked: u64,
    settled: u64,
}

impl ChunkWriter {
    pub fn write(&mut self, data: &[u8]) -> io::Result<()> {
        self.file.write_all_at(data, self.pos)?;
        self.pos += data.len() as u64;
        if self.pos - self.kicked >= WINDOW {
            sync_range(&self.file, self.kicked, self.pos - self.kicked, START)?;
            self.settle(self.kicked)?;
            self.kicked = self.pos;
        }
        Ok(())
    }

    /// Returns once everything this writer wrote has been written back to the device.
    pub fn finish(mut self) -> io::Result<()> {
        self.settle(self.pos)
    }

    /// Waits for write-back of `[settled, end)` and drops those pages.
    fn settle(&mut self, end: u64) -> io::Result<()> {
        let len = end - self.settled;
        sync_range(&self.file, self.settled, len, WAIT)?;
        if self.cache == CachePolicy::Drop {
            drop_pages(&self.file, self.settled, len);
        }
        self.settled = end;
        Ok(())
    }
}

fn drop_pages(file: &File, offset: u64, len: u64) {
    if let Some(len) = NonZeroU64::new(len) {
        let _ = fadvise(file.as_fd(), offset, Some(len), Advice::DontNeed);
    }
}

const START: libc::c_uint = libc::SYNC_FILE_RANGE_WRITE;
const WAIT: libc::c_uint = libc::SYNC_FILE_RANGE_WAIT_BEFORE
    | libc::SYNC_FILE_RANGE_WRITE
    | libc::SYNC_FILE_RANGE_WAIT_AFTER;

/// `sync_file_range(2)` on `[offset, offset + len)`. A zero `len` is a no-op here (the syscall would
/// treat it as "to end of file").
#[allow(unsafe_code)]
fn sync_range(file: &File, offset: u64, len: u64, flags: libc::c_uint) -> io::Result<()> {
    use std::os::fd::AsRawFd;
    if len == 0 {
        return Ok(());
    }
    let to_off = |v: u64| libc::off64_t::try_from(v).map_err(|_| io::ErrorKind::InvalidInput);
    let (offset, len) = (to_off(offset)?, to_off(len)?);
    loop {
        // SAFETY: the fd is open for the borrow of `file`; the call reads no user memory and only
        // takes integer arguments, so no Rust invariant depends on its outcome.
        let rc = unsafe { libc::sync_file_range(file.as_raw_fd(), offset, len, flags) };
        if rc == 0 {
            return Ok(());
        }
        let err = io::Error::last_os_error();
        if err.kind() != io::ErrorKind::Interrupted {
            return Err(err);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Bytes of `path` in the page cache, or None where `fincore` or a real page cache is missing.
    fn cached_bytes(path: &std::path::Path) -> Option<u64> {
        let out = std::process::Command::new("fincore")
            .args(["--bytes", "--noheadings", "--output", "RES"])
            .arg(path)
            .output()
            .ok()?;
        String::from_utf8(out.stdout).ok()?.trim().parse().ok()
    }

    #[test]
    fn closing_a_reader_leaves_none_of_the_file_cached() {
        // Under target/, because a tmpfs temp folder is all page cache and cannot show this.
        let dir = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
        let path = dir.path().join("file");
        let size = 32 << 20;
        std::fs::write(&path, vec![7u8; size]).unwrap();
        let file = File::open(&path).unwrap();
        file.sync_all().unwrap();
        std::fs::read(&path).unwrap();
        let Some(before) = cached_bytes(&path) else {
            return;
        };
        if before < (size as u64) / 2 {
            return;
        }

        // Pages outside what this reader reads are what read-ahead leaves behind.
        let reader = LocalReader::new(file, CachePolicy::Drop);
        let mut one = [0u8; 4096];
        reader.read_at(0, &mut one).unwrap();
        drop(reader);

        let after = cached_bytes(&path).unwrap();
        assert!(after < 1 << 20, "{after} of {before} bytes still cached");
    }
}
