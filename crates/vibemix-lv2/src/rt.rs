//! Thread-local marker for code running inside the JACK audio callback.
//!
//! The engine's RT guard sets this before invoking plugin code. The LV2 URID
//! map uses it to avoid blocking or allocating in the callback.

use std::cell::Cell;

thread_local! {
    static AUDIO_THREAD: Cell<bool> = const { Cell::new(false) };
}

pub fn set_audio_thread(active: bool) {
    AUDIO_THREAD.with(|flag| flag.set(active));
}

pub fn in_audio_thread() -> bool {
    AUDIO_THREAD.with(Cell::get)
}
