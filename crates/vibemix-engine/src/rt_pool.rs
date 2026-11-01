//! Real-time safe memory pool for the audio thread.
//!
//! This module provides a lock-free memory pool that pre-allocates buffers
//! to avoid heap allocations in the audio thread. All allocations are done
//! up-front, and the pool uses a simple bump allocator pattern.

use std::sync::atomic::{AtomicUsize, Ordering};

/// A fixed-size memory pool for real-time safe allocations.
///
/// The pool pre-allocates a buffer and uses a bump allocator pattern.
/// All allocations are atomic and lock-free.
pub struct RtPool {
    /// The pre-allocated buffer
    buffer: Box<[u8]>,
    /// Current allocation offset
    offset: AtomicUsize,
    /// Total capacity in bytes
    capacity: usize,
}

impl RtPool {
    /// Create a new memory pool with the given capacity.
    pub fn new(capacity: usize) -> Self {
        Self {
            buffer: vec![0u8; capacity].into_boxed_slice(),
            offset: AtomicUsize::new(0),
            capacity,
        }
    }

    /// Allocate a buffer of the given size in the pool.
    /// Returns None if there's not enough space.
    ///
    /// # Safety
    /// This must only be called from the audio thread to maintain real-time safety.
    pub fn alloc(&self, size: usize) -> Option<std::ptr::NonNull<[u8]>> {
        let offset = self.offset.fetch_add(size, Ordering::SeqCst);

        if offset + size <= self.capacity {
            unsafe {
                let ptr = self.buffer.as_ptr().add(offset) as *mut u8;
                Some(std::ptr::NonNull::new(std::slice::from_raw_parts_mut(
                    ptr, size,
                ))?)
            }
        } else {
            None
        }
    }

    /// Reset the pool to allow reusing the buffer.
    /// This is not thread-safe and should only be called when no allocations are in progress.
    pub fn reset(&self) {
        self.offset.store(0, Ordering::Release);
    }

    /// Get the current used capacity.
    pub fn used(&self) -> usize {
        self.offset.load(Ordering::Acquire)
    }

    /// Get the remaining capacity.
    pub fn remaining(&self) -> usize {
        self.capacity.saturating_sub(self.used())
    }
}

impl Default for RtPool {
    fn default() -> Self {
        // Default to 1MB pool (sufficient for most audio processing)
        Self::new(1024 * 1024)
    }
}

/// RAII guard for temporary allocations within a scope.
/// Automatically resets the pool when dropped.
pub struct PoolGuard<'a> {
    pool: &'a RtPool,
    marker: std::marker::PhantomData<&'a ()>,
}

impl<'a> PoolGuard<'a> {
    /// Create a new pool guard from an existing pool.
    pub fn new(pool: &'a RtPool) -> Self {
        pool.reset();
        Self {
            pool,
            marker: std::marker::PhantomData,
        }
    }

    /// Allocate from the pool.
    pub fn alloc(&self, size: usize) -> Option<std::ptr::NonNull<[u8]>> {
        self.pool.alloc(size)
    }

    /// Get the underlying pool.
    pub fn pool(&self) -> &'a RtPool {
        self.pool
    }
}

impl<'a> Drop for PoolGuard<'a> {
    fn drop(&mut self) {
        // Reset on drop to ensure clean state for next iteration
        self.pool.reset();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pool_allocations_work() {
        let pool = RtPool::new(1024);

        // First allocation
        let buf1 = pool.alloc(100).unwrap();
        assert_eq!(buf1.len(), 100);

        // Second allocation
        let buf2 = pool.alloc(50).unwrap();
        assert_eq!(buf2.len(), 50);

        // Check offset
        assert_eq!(pool.used(), 150);
        assert_eq!(pool.remaining(), 874);
    }

    #[test]
    fn pool_rejects_overflow() {
        let pool = RtPool::new(100);

        // Large allocation should fail
        assert!(pool.alloc(200).is_none());
    }

    #[test]
    fn pool_guard_auto_resets() {
        let pool = RtPool::new(1024);

        {
            let _guard = PoolGuard::new(&pool);
            let _buf = pool.alloc(100).unwrap();
            assert_eq!(pool.used(), 100);
        }

        // Pool should be reset after guard drops
        assert_eq!(pool.used(), 0);
    }
}
