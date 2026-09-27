use std::cell::UnsafeCell;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};

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
    pub fn new(control_ports: &[usize], atom_ports: &[usize], atom_capacity: usize) -> Self {
        Self {
            active: AtomicBool::new(false),
            control_ports: control_ports.iter().map(|&port| port as u32).collect(),
            control_values: (0..control_ports.len())
                .map(|_| AtomicU32::new(0))
                .collect(),
            atom_ports: atom_ports.iter().map(|&port| port as u32).collect(),
            atoms: (0..atom_ports.len())
                .map(|_| AtomSlot {
                    seq: AtomicUsize::new(0),
                    len: AtomicUsize::new(0),
                    format: AtomicU32::new(0),
                    buffer: UnsafeCell::new(vec![0u8; atom_capacity].into_boxed_slice()),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inactive_outputs_are_not_published() {
        let outputs = SharedOutputs::new(&[7], &[], 16);
        outputs.publish_control(&[42.0]);
        assert!(!outputs.is_active());
        assert_eq!(outputs.control_value(0), 0.0);
    }

    #[test]
    fn control_values_round_trip() {
        let outputs = SharedOutputs::new(&[5, 9], &[], 16);
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
        let outputs = SharedOutputs::new(&[], &[3], 16);
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
}
