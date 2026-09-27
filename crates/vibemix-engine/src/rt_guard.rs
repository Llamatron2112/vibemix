//! Optional debug guard that aborts if the audio thread allocates or frees.
//!
//! Enabled by setting `VIBEMIX_RT_GUARD=1` in the environment. The process
//! callback brackets itself with [`AudioScope`], and the custom global
//! allocator aborts if an allocation happens while a scope is active.
//!
//! The check itself must not allocate, so the "in audio" flag is a
//! const-initialised thread-local and the abort message is written with
//! `libc::write` (never `eprintln!`).

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::atomic::{AtomicBool, Ordering};

static ENABLED: AtomicBool = AtomicBool::new(false);

thread_local! {
    static IN_AUDIO: Cell<bool> = const { Cell::new(false) };
}

pub fn enable() {
    ENABLED.store(true, Ordering::SeqCst);
}

/// Marks the current thread as running the audio callback for its lifetime.
pub struct AudioScope;

impl AudioScope {
    #[inline]
    pub fn enter() -> Self {
        IN_AUDIO.with(|flag| flag.set(true));
        AudioScope
    }
}

impl Drop for AudioScope {
    #[inline]
    fn drop(&mut self) {
        IN_AUDIO.with(|flag| flag.set(false));
    }
}

pub struct GuardAllocator;

unsafe impl GlobalAlloc for GuardAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        check();
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        check();
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        check();
        unsafe { System.realloc(ptr, layout, new_size) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        check();
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[inline]
fn check() {
    if !ENABLED.load(Ordering::Relaxed) {
        return;
    }
    if IN_AUDIO.try_with(Cell::get).unwrap_or(false) {
        const MESSAGE: &[u8] = b"vibemix RT guard: heap operation on the audio thread\n";
        unsafe {
            libc::write(2, MESSAGE.as_ptr().cast(), MESSAGE.len());
        }
        std::process::abort();
    }
}
