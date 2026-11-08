//! Fixed-size buffer pool with backpressure (plan.md §6.2, CLAUDE.md §4).
//!
//! At most `count` buffers of `size` bytes ever exist, so transfer memory is capped at
//! `count * size` no matter how many lanes or files are active. `acquire` waits when all are in use.
//! Buffers are allocated on first use (idle memory stays low) and reused afterwards. Contents are not
//! cleared between uses: they only ever hold the same user's file data.

use std::ops::{Deref, DerefMut};
use std::sync::{Arc, Mutex};

use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// Hands freed memory back to the kernel. Glibc keeps it in per-thread arenas otherwise, so a daemon
/// that has finished a transfer would sit idle holding the size of its last buffer pool.
pub fn release_free_memory() {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    #[allow(unsafe_code)]
    // SAFETY: malloc_trim takes an integer, touches only the allocator's own state under its locks,
    // and may be called from any thread at any time.
    unsafe {
        libc::malloc_trim(0);
    }
}

#[derive(Clone)]
pub struct BufferPool {
    inner: Arc<Inner>,
}

struct Inner {
    free: Mutex<Vec<Box<[u8]>>>,
    permits: Arc<Semaphore>,
    size: usize,
    count: usize,
    allocated: Mutex<usize>,
}

impl BufferPool {
    /// `count` and `size` are raised to at least 1.
    pub fn new(count: usize, size: usize) -> Self {
        let count = count.max(1);
        Self {
            inner: Arc::new(Inner {
                free: Mutex::new(Vec::with_capacity(count)),
                permits: Arc::new(Semaphore::new(count)),
                size: size.max(1),
                count,
                allocated: Mutex::new(0),
            }),
        }
    }

    /// Waits until a buffer is free.
    pub async fn acquire(&self) -> Buffer {
        // The pool owns the semaphore and never closes it, so acquiring cannot fail.
        let permit = match self.inner.permits.clone().acquire_owned().await {
            Ok(p) => p,
            Err(_) => unreachable!("buffer pool semaphore is never closed"),
        };
        self.take(permit)
    }

    pub fn try_acquire(&self) -> Option<Buffer> {
        let permit = self.inner.permits.clone().try_acquire_owned().ok()?;
        Some(self.take(permit))
    }

    /// Holding a permit guarantees a free buffer exists or may be allocated.
    fn take(&self, permit: OwnedSemaphorePermit) -> Buffer {
        let reused = self.inner.free.lock().ok().and_then(|mut f| f.pop());
        let buf = reused.unwrap_or_else(|| {
            if let Ok(mut n) = self.inner.allocated.lock() {
                *n += 1;
            }
            vec![0u8; self.inner.size].into_boxed_slice()
        });
        Buffer {
            buf: Some(buf),
            pool: self.inner.clone(),
            _permit: permit,
        }
    }

    pub fn buffer_size(&self) -> usize {
        self.inner.size
    }

    pub fn capacity(&self) -> usize {
        self.inner.count
    }

    /// Buffers allocated so far; never more than `capacity`.
    pub fn allocated(&self) -> usize {
        self.inner.allocated.lock().map_or(0, |a| *a)
    }
}

/// A pooled buffer of exactly `buffer_size` bytes. Returned to the pool on drop.
pub struct Buffer {
    buf: Option<Box<[u8]>>,
    pool: Arc<Inner>,
    // Dropped after `Drop::drop` has put the buffer back, so a waiter never finds the list empty.
    _permit: OwnedSemaphorePermit,
}

impl Deref for Buffer {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        self.buf.as_deref().unwrap_or(&[])
    }
}

impl DerefMut for Buffer {
    fn deref_mut(&mut self) -> &mut [u8] {
        self.buf.as_deref_mut().unwrap_or(&mut [])
    }
}

impl Drop for Buffer {
    fn drop(&mut self) {
        if let (Some(buf), Ok(mut free)) = (self.buf.take(), self.pool.free.lock()) {
            free.push(buf);
        }
    }
}

/// Caps the bytes held in variable-size buffers, for data read ahead of the network. A request
/// larger than the whole budget takes all of it, so one big block never waits forever. Released
/// buffers are kept and reused: allocating a fresh 16 MiB block per chunk let freed blocks pile up
/// in the allocator's per-thread arenas (247 MB peak RSS against a 128 MiB budget, 2026-11-09).
#[derive(Clone)]
pub struct ByteBudget {
    permits: Arc<Semaphore>,
    /// Permits are KiB, so a budget of many GiB fits the semaphore's `u32` acquire count.
    total: u32,
    free: Arc<Mutex<Vec<Vec<u8>>>>,
    allocated: Arc<Mutex<usize>>,
}

/// Bytes owned under a `ByteBudget`; the buffer and the budget are given back on drop.
pub struct Budgeted {
    pub data: Vec<u8>,
    free: Arc<Mutex<Vec<Vec<u8>>>>,
    // Dropped after `Drop::drop` has put the buffer back.
    _permit: OwnedSemaphorePermit,
}

impl Drop for Budgeted {
    fn drop(&mut self) {
        if let Ok(mut free) = self.free.lock() {
            free.push(std::mem::take(&mut self.data));
        }
    }
}

impl ByteBudget {
    /// `bytes` is raised to at least 1 KiB.
    pub fn new(bytes: u64) -> Self {
        let total = u32::try_from(bytes.div_ceil(1024))
            .unwrap_or(u32::MAX)
            .max(1);
        Self {
            permits: Arc::new(Semaphore::new(total as usize)),
            total,
            free: Arc::default(),
            allocated: Arc::default(),
        }
    }

    /// Waits until `len` bytes fit, then returns a buffer of that length. Its contents are
    /// whatever an earlier holder left (the same transfer's data) or zeros; callers overwrite it.
    pub async fn acquire(&self, len: usize) -> Budgeted {
        let kib = u32::try_from(len.div_ceil(1024))
            .unwrap_or(u32::MAX)
            .clamp(1, self.total);
        // The budget owns the semaphore and never closes it.
        let Ok(permit) = self.permits.clone().acquire_many_owned(kib).await else {
            unreachable!("byte budget semaphore is never closed")
        };
        // Largest free buffer first, so a full-size block rarely has to grow a small one.
        let reused = self.free.lock().ok().and_then(|mut f| {
            let i = (0..f.len()).max_by_key(|&i| f[i].capacity())?;
            Some(f.swap_remove(i))
        });
        let mut data = reused.unwrap_or_else(|| {
            if let Ok(mut n) = self.allocated.lock() {
                *n += 1;
            }
            Vec::new()
        });
        data.resize(len, 0);
        Budgeted {
            data,
            free: self.free.clone(),
            _permit: permit,
        }
    }

    /// KiB not currently held.
    pub fn available_kib(&self) -> usize {
        self.permits.available_permits()
    }

    /// Buffers created so far; reuse keeps this near the number held at once.
    pub fn allocated(&self) -> usize {
        self.allocated.lock().map_or(0, |n| *n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn buffers_have_the_configured_size() {
        let pool = BufferPool::new(2, 4096);
        let b = pool.acquire().await;
        assert_eq!(b.len(), 4096);
        assert_eq!(pool.buffer_size(), 4096);
    }

    #[tokio::test]
    async fn acquire_waits_when_exhausted_and_wakes_on_release() {
        let pool = BufferPool::new(2, 16);
        let a = pool.acquire().await;
        let _b = pool.acquire().await;
        assert!(pool.try_acquire().is_none());
        let waiter = tokio::spawn({
            let pool = pool.clone();
            async move { pool.acquire().await.len() }
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            !waiter.is_finished(),
            "must wait while both buffers are held"
        );
        drop(a);
        let len = tokio::time::timeout(Duration::from_secs(2), waiter)
            .await
            .expect("release must wake the waiter")
            .expect("task");
        assert_eq!(len, 16);
    }

    #[tokio::test]
    async fn released_buffers_are_reused_not_reallocated() {
        let pool = BufferPool::new(1, 64);
        let first = pool.acquire().await.as_ptr();
        let second = pool.acquire().await.as_ptr();
        assert_eq!(first, second);
        assert_eq!(pool.allocated(), 1);
    }

    #[tokio::test]
    async fn allocation_is_lazy() {
        let pool = BufferPool::new(8, 1 << 20);
        assert_eq!(pool.allocated(), 0, "an idle pool holds no memory");
        let _b = pool.acquire().await;
        assert_eq!(pool.allocated(), 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn never_allocates_past_capacity_under_contention() {
        let pool = BufferPool::new(3, 1024);
        let mut tasks = tokio::task::JoinSet::new();
        for i in 0..200u32 {
            let pool = pool.clone();
            tasks.spawn(async move {
                let mut b = pool.acquire().await;
                b[0] = i as u8;
                tokio::task::yield_now().await;
                b[0]
            });
        }
        let mut done = 0;
        while let Some(r) = tasks.join_next().await {
            r.expect("task");
            done += 1;
        }
        assert_eq!(done, 200);
        assert!(pool.allocated() <= 3, "allocated {}", pool.allocated());
    }

    #[tokio::test]
    async fn zero_count_or_size_is_raised_to_one() {
        let pool = BufferPool::new(0, 0);
        assert_eq!(pool.capacity(), 1);
        assert_eq!(pool.acquire().await.len(), 1);
    }

    #[tokio::test]
    async fn byte_budget_bounds_bytes_held_and_wakes_waiters() {
        let budget = ByteBudget::new(10 << 10);
        let a = budget.acquire(6 << 10).await;
        assert_eq!(a.data.len(), 6 << 10);
        let waiter = tokio::spawn({
            let budget = budget.clone();
            async move { budget.acquire(6 << 10).await.data.len() }
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!waiter.is_finished(), "12 KiB must not fit a 10 KiB budget");
        drop(a);
        assert_eq!(waiter.await.expect("waiter"), 6 << 10);
        assert_eq!(budget.available_kib(), 10);
    }

    #[tokio::test]
    async fn a_request_above_the_budget_takes_all_of_it() {
        let budget = ByteBudget::new(4 << 10);
        let big = budget.acquire(1 << 20).await;
        assert_eq!(big.data.len(), 1 << 20);
        assert_eq!(budget.available_kib(), 0);
        drop(big);
        assert_eq!(budget.available_kib(), 4);
    }

    #[tokio::test]
    async fn byte_budget_reuses_released_buffers() {
        let budget = ByteBudget::new(64 << 10);
        for _ in 0..50 {
            let a = budget.acquire(16 << 10).await;
            let b = budget.acquire(16 << 10).await;
            drop((a, b));
        }
        assert_eq!(budget.allocated(), 2, "two buffers serve every round");
        let small = budget.acquire(100).await;
        assert_eq!(
            small.data.len(),
            100,
            "a reused buffer is cut to the request"
        );
    }
}
