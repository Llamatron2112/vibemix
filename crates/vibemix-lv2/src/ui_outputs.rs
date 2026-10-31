use std::cell::UnsafeCell;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::Mutex;

/// Lock-free snapshot of a plugin instance's output ports, shared between the
/// audio thread (which publishes after every `run`) and the control thread
/// (which forwards the values to the plugin's native UI via `ui:portEvent`).
///
/// Publishing is gated on `active`, so the audio thread does no work at all
/// until a UI for this plugin is open.
pub struct SharedOutputs {
    active: AtomicBool,
    control_ports: Box<[u32]>,
    control_values: Box<[AtomicU32]>,
    atom_ports: Box<[u32]>,
    atoms: Box<[AtomSlot]>,
}

struct AtomSlot {
    seq: AtomicUsize,
    len: AtomicUsize,
    format: AtomicU32,
    buffer: UnsafeCell<Box<[u8]>>,
}

// SAFETY: access to `buffer` is guarded by the `seq` seqlock (single writer on
// the audio thread, readers retry on a torn read).
unsafe impl Sync for AtomSlot {}

impl SharedOutputs {
    pub fn new(control_ports: &[usize], atom_ports: &[usize], atom_capacities: &[usize]) -> Self {
        Self {
            active: AtomicBool::new(false),
            control_ports: control_ports.iter().map(|&port| port as u32).collect(),
            control_values: (0..control_ports.len())
                .map(|_| AtomicU32::new(0))
                .collect(),
            atom_ports: atom_ports.iter().map(|&port| port as u32).collect(),
            atoms: atom_ports
                .iter()
                .enumerate()
                .map(|(index, _)| {
                    let capacity = atom_capacities.get(index).copied().unwrap_or(0);
                    AtomSlot {
                        seq: AtomicUsize::new(0),
                        len: AtomicUsize::new(0),
                        format: AtomicU32::new(0),
                        buffer: UnsafeCell::new(vec![0u8; capacity].into_boxed_slice()),
                    }
                })
                .collect(),
        }
    }

    pub fn set_active(&self, active: bool) {
        self.active.store(active, Ordering::Release);
    }

    pub fn is_active(&self) -> bool {
        self.active.load(Ordering::Acquire)
    }

    pub fn control_port_count(&self) -> usize {
        self.control_ports.len()
    }

    pub fn control_port(&self, index: usize) -> u32 {
        self.control_ports.get(index).copied().unwrap_or(0)
    }

    pub fn control_value(&self, index: usize) -> f32 {
        self.control_values
            .get(index)
            .map(|value| f32::from_bits(value.load(Ordering::Relaxed)))
            .unwrap_or(0.0)
    }

    pub fn atom_port_count(&self) -> usize {
        self.atom_ports.len()
    }

    pub fn atom_port(&self, index: usize) -> u32 {
        self.atom_ports.get(index).copied().unwrap_or(0)
    }

    pub fn atom_seq(&self, index: usize) -> usize {
        self.atoms
            .get(index)
            .map(|slot| slot.seq.load(Ordering::Acquire))
            .unwrap_or(0)
    }

    /// Returns the most recent atom payload (including its `LV2_Atom` header)
    /// and its type URID, or `None` if it is being written right now.
    pub fn read_atom(&self, index: usize) -> Option<(Box<[u8]>, u32)> {
        let slot = self.atoms.get(index)?;
        for _ in 0..256 {
            let before = slot.seq.load(Ordering::Acquire);
            if before & 1 != 0 {
                std::hint::spin_loop();
                continue;
            }
            let len = slot.len.load(Ordering::Relaxed).min(slot_buffer_len(slot));
            let format = slot.format.load(Ordering::Relaxed);
            let mut out = vec![0u8; len].into_boxed_slice();
            // SAFETY: the seqlock sequence above is even, and we re-check it
            // after copying; a torn read is detected and retried.
            unsafe {
                let buffer = &*slot.buffer.get();
                out.copy_from_slice(&buffer[..len]);
            }
            if slot.seq.load(Ordering::Acquire) == before {
                return Some((out, format));
            }
        }
        None
    }

    pub fn publish_control(&self, values: &[f32]) {
        if !self.is_active() {
            return;
        }
        for (slot, value) in self.control_values.iter().zip(values) {
            slot.store(value.to_bits(), Ordering::Relaxed);
        }
    }

    pub fn publish_atom(&self, index: usize, bytes: &[u8], format: u32) {
        if !self.is_active() {
            return;
        }
        let Some(slot) = self.atoms.get(index) else {
            return;
        };
        let seq = slot.seq.load(Ordering::Relaxed);
        slot.seq.store(seq.wrapping_add(1), Ordering::Release);
        // SAFETY: single writer (audio thread); readers use the seqlock.
        unsafe {
            let buffer = &mut *slot.buffer.get();
            let len = bytes.len().min(buffer.len());
            buffer[..len].copy_from_slice(&bytes[..len]);
            slot.len.store(len, Ordering::Relaxed);
            slot.format.store(format, Ordering::Relaxed);
        }
        slot.seq.store(seq.wrapping_add(2), Ordering::Release);
    }
}

fn slot_buffer_len(slot: &AtomSlot) -> usize {
    // SAFETY: only read for its length; the pointer is always valid.
    unsafe { (&*slot.buffer.get()).len() }
}

/// Mailbox for UI→DSP atom events, from the control thread (UI writes) to the
/// audio thread. The audio thread only ever `try_lock`s, so it never blocks,
/// and buffers are recycled so it never allocates or frees either: the `free`
/// list is reserved for the queue limit up front, so the audio thread's push
/// into it only ever uses capacity that already exists.
pub struct SharedUiInputs {
    inner: Mutex<UiInputInner>,
    /// Events dropped by `push` because the queue was already at its limit.
    dropped: AtomicU64,
}

/// Maximum number of atom events queued from UIs at once. Buffers circulate
/// between `queue` and `free`, so this bounds the shared allocation; a UI that
/// floods the mailbox faster than the audio thread drains it loses events
/// (counted by [`SharedUiInputs::dropped`]) instead of growing unbounded.
pub const UI_INPUT_QUEUE_LIMIT: usize = 64;

#[derive(Default)]
struct UiInputInner {
    queue: VecDeque<(u32, Vec<u8>)>,
    free: Vec<Vec<u8>>,
}

impl SharedUiInputs {
    pub fn new() -> Self {
        let mut free = Vec::new();
        // Reserved once so `drain` on the audio thread never reallocates: the
        // number of circulating buffers never exceeds the queue limit.
        free.reserve_exact(UI_INPUT_QUEUE_LIMIT);
        Self {
            inner: Mutex::new(UiInputInner {
                queue: VecDeque::new(),
                free,
            }),
            dropped: AtomicU64::new(0),
        }
    }

    /// Control thread: queue one atom event for plugin port `port`.
    pub fn push(&self, port: u32, bytes: &[u8]) {
        let Ok(mut inner) = self.inner.lock() else {
            self.drop_event(port, bytes.len(), "could not lock the UI input mailbox");
            return;
        };
        let mut buffer = if let Some(free) = inner.free.pop() {
            free
        } else {
            if inner.queue.len() >= UI_INPUT_QUEUE_LIMIT {
                drop(inner);
                self.drop_event(port, bytes.len(), "UI input queue full");
                return;
            }
            // Allocation happens here, on the control thread.
            Vec::with_capacity(bytes.len().max(64))
        };
        buffer.clear();
        buffer.extend_from_slice(bytes);
        inner.queue.push_back((port, buffer));
    }

    fn drop_event(&self, port: u32, len: usize, reason: &'static str) {
        let count = self.dropped.fetch_add(1, Ordering::Relaxed) + 1;
        tracing::debug!(
            dropped = count,
            port,
            len,
            reason,
            "plugin UI event dropped"
        );
    }

    /// Audio thread: drain the queued events, recycling their buffers.
    pub fn drain(&self, mut f: impl FnMut(u32, &[u8])) {
        let Ok(mut inner) = self.inner.try_lock() else {
            return;
        };
        while let Some((port, bytes)) = inner.queue.pop_front() {
            f(port, &bytes);
            inner.free.push(bytes);
        }
    }

    /// How many events `push` has dropped because the queue was full.
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

impl Default for SharedUiInputs {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inactive_outputs_are_not_published() {
        let outputs = SharedOutputs::new(&[7], &[], &[]);
        outputs.publish_control(&[42.0]);
        assert!(!outputs.is_active());
        assert_eq!(outputs.control_value(0), 0.0);
    }

    #[test]
    fn control_values_round_trip() {
        let outputs = SharedOutputs::new(&[5, 9], &[], &[]);
        outputs.set_active(true);
        assert!(outputs.is_active());
        assert_eq!(outputs.control_port_count(), 2);
        assert_eq!(outputs.control_port(1), 9);

        outputs.publish_control(&[1.5, -0.25]);
        assert_eq!(outputs.control_value(0), 1.5);
        assert_eq!(outputs.control_value(1), -0.25);
    }

    #[test]
    fn atom_payloads_round_trip() {
        let outputs = SharedOutputs::new(&[], &[3], &[16]);
        outputs.set_active(true);
        assert_eq!(outputs.atom_seq(0), 0);

        outputs.publish_atom(0, &[1, 2, 3, 4], 0xfeed);
        let seq = outputs.atom_seq(0);
        assert_ne!(seq, 0);

        let (bytes, format) = outputs.read_atom(0).expect("published atom is readable");
        assert_eq!(&bytes[..], &[1, 2, 3, 4]);
        assert_eq!(format, 0xfeed);

        outputs.publish_atom(0, &[9, 8], 0xbeef);
        assert_ne!(outputs.atom_seq(0), seq);
        let (bytes, format) = outputs.read_atom(0).unwrap();
        assert_eq!(&bytes[..], &[9, 8]);
        assert_eq!(format, 0xbeef);
    }

    #[test]
    fn ui_input_queue_drops_beyond_limit() {
        let inputs = SharedUiInputs::new();
        for index in 0..(UI_INPUT_QUEUE_LIMIT + 8) {
            inputs.push(3, &[index as u8]);
        }
        assert_eq!(inputs.dropped(), 8);

        let mut drained = 0usize;
        inputs.drain(|port, bytes| {
            assert_eq!(port, 3);
            assert_eq!(bytes.len(), 1);
            drained += 1;
        });
        assert_eq!(drained, UI_INPUT_QUEUE_LIMIT);
        // The dropped count is cumulative, not reset by draining.
        assert_eq!(inputs.dropped(), 8);

        // After draining, the recycled buffer is reused and the free list
        // stays within its reserved capacity.
        inputs.push(4, &[9, 9]);
        inputs.drain(|port, bytes| {
            assert_eq!(port, 4);
            assert_eq!(bytes, &[9, 9]);
        });
        assert_eq!(inputs.dropped(), 8);
    }
}
