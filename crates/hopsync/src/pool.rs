//! Fixed-size buffer pool with backpressure (plan.md §6.2, CLAUDE.md §4).
//!
//! At most `count` buffers of `size` bytes ever exist, so transfer memory is capped at
//! `count * size` no matter how many lanes or files are active. `acquire` waits when all are in use.
//! Buffers are allocated on first use (idle memory stays low) and reused afterwards. Contents are not
//! cleared between uses: they only ever hold the same user's file data.

use std::ops::{Deref, DerefMut};
use std::sync::{Arc, Mutex};

use tokio::sync::{OwnedSemaphorePermit, Semaphore};

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
}
