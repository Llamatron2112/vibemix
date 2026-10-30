//! Minimal LV2 atom writers for the atom `Sequence` input buffers we hand to
//! plugins: MIDI events (`midi:MidiEvent`) and the host `time:Position` object.

/// URIDs needed to forge atoms, resolved once per instance.
#[derive(Clone, Copy, Default)]
pub struct AtomUrids {
    pub object: u32,
    pub midi_event: u32,
    pub time_position: u32,
    pub time_frame: u32,
    pub time_beat: u32,
    pub time_bar: u32,
    pub time_bar_beat: u32,
    pub time_beats_per_minute: u32,
    pub time_beats_per_bar: u32,
    pub time_beat_unit: u32,
    pub time_speed: u32,
    pub atom_long: u32,
    pub atom_double: u32,
    pub atom_float: u32,
    pub atom_int: u32,
}

/// Beat unit used in the host `time:Position`: quarter notes (4), matching the
/// quarter-note `beats` derived from the incoming 24 PPQN clock.
pub const TIME_BEAT_UNIT_QUARTER: u32 = 4;

/// `atom:eventTransfer` — the `format` used when transferring individual events
/// to/from an LV2 UI (`LV2UI_Write_Function` / `LV2UI_Port_Event`).
pub const EVENT_TRANSFER_URI: &str = "http://lv2plug.in/ns/ext/atom#eventTransfer";

/// A single incoming MIDI message with its frame offset within the block.
/// Fixed-size so the audio thread never allocates.
#[derive(Clone, Copy, Default)]
pub struct MidiEvent {
    pub time: i64,
    pub len: u8,
    pub bytes: [u8; 4],
}

impl MidiEvent {
    pub fn new(time: i64, bytes: &[u8]) -> Self {
        let mut event = MidiEvent {
            time,
            len: bytes.len().min(4) as u8,
            bytes: [0; 4],
        };
        event.bytes[..event.len as usize].copy_from_slice(&bytes[..event.len as usize]);
        event
    }

    pub fn as_slice(&self) -> &[u8] {
        &self.bytes[..self.len as usize]
    }
}

/// Transport information published to plugins as `time:Position` each block.
#[derive(Clone, Copy, Default)]
pub struct TimePosition {
    pub frame: i64,
    pub beat: f64,
    pub bar: i64,
    pub bar_beat: f32,
    pub beats_per_minute: f32,
    pub beats_per_bar: f32,
    pub speed: f32,
}

/// Size of `LV2_Atom_Sequence_Body` (`unit` + `pad`), which precedes the events.
pub const SEQUENCE_BODY: usize = 8;

fn align8(value: usize) -> usize {
    (value + 7) & !7
}

/// Reset a sequence buffer to "no events": the body is just the sequence header.
pub fn clear_sequence(words: &mut [u64]) {
    let bytes =
        unsafe { std::slice::from_raw_parts_mut(words.as_mut_ptr() as *mut u8, words.len() * 8) };
    bytes[0..4].copy_from_slice(&(SEQUENCE_BODY as u32).to_ne_bytes());
    bytes[8..16].fill(0);
}

/// Calls `f` with each event's `LV2_Atom` (header + body, without the 8-byte
/// timestamp) in an `LV2_Atom_Sequence` serialized as `[size][type][unit][pad]…`.
pub fn for_each_event(sequence: &[u8], mut f: impl FnMut(&[u8])) {
    if sequence.len() < 16 {
        return;
    }
    let size = u32::from_ne_bytes(sequence[0..4].try_into().unwrap()) as usize;
    let end = (SEQUENCE_BODY + size).min(sequence.len());
    let mut offset = SEQUENCE_BODY + 8; // skip the atom header and the sequence body head
    while offset + 16 <= end {
        let atom_size =
            u32::from_ne_bytes(sequence[offset + 8..offset + 12].try_into().unwrap()) as usize;
        let atom_end = offset + 16 + atom_size;
        if atom_end > end {
            break;
        }
        f(&sequence[offset + 8..atom_end]);
        offset += align8(16 + atom_size);
    }
}

/// Appends `LV2_Atom_Event`s to a `LV2_Atom_Sequence` buffer in place.
///
/// The buffer's first two `u32` words are the atom size and type; the current
/// size is read on construction and written back after each append.
pub struct SequenceWriter<'a> {
    bytes: &'a mut [u8],
    size: usize,
}

impl<'a> SequenceWriter<'a> {
    pub fn new(words: &'a mut [u64]) -> Self {
        // SAFETY: a sequence buffer is a header (size + type) followed by body.
        let bytes = unsafe {
            std::slice::from_raw_parts_mut(words.as_mut_ptr() as *mut u8, words.len() * 8)
        };
        let size = bytes
            .get(0..4)
            .and_then(|head| head.try_into().ok())
            .map(u32::from_ne_bytes)
            .unwrap_or(0) as usize;
        Self {
            bytes,
            size: size.max(SEQUENCE_BODY),
        }
    }

    /// Body capacity in bytes (everything after the atom header).
    fn capacity(&self) -> usize {
        self.bytes.len().saturating_sub(8)
    }

    fn write_event_header(&mut self, offset: usize, value_size: usize, value_type: u32) -> usize {
        let body = &mut self.bytes[8 + offset..];
        body[0..8].copy_from_slice(&0i64.to_ne_bytes()); // time filled by caller
        body[8..12].copy_from_slice(&(value_size as u32).to_ne_bytes());
        body[12..16].copy_from_slice(&value_type.to_ne_bytes());
        16
    }

    /// Append a raw MIDI message at `time` frames into the block.
    pub fn append_midi(&mut self, time: i64, bytes: &[u8], urids: &AtomUrids) -> bool {
        let len = bytes.len();
        let total = align8(16 + len);
        if self.size + total > self.capacity() {
            return false;
        }
        let offset = self.size;
        self.write_event_header(offset, len, urids.midi_event);
        let body = &mut self.bytes[8 + offset..];
        body[0..8].copy_from_slice(&time.to_ne_bytes());
        body[16..16 + len].copy_from_slice(bytes);
        self.size += total;
        true
    }

    /// Append a `time:Position` object at `time` frames into the block.
    /// Allocation-free (uses a fixed stack buffer for the object body).
    pub fn append_position(
        &mut self,
        time: i64,
        position: &TimePosition,
        urids: &AtomUrids,
    ) -> bool {
        let mut body = [0u8; 256];
        let mut len = 8usize; // object id (0) + otype
        body[4..8].copy_from_slice(&urids.time_position.to_ne_bytes());

        push_property(
            &mut body,
            &mut len,
            urids.time_frame,
            urids.atom_long,
            &position.frame.to_ne_bytes(),
        );
        push_property(
            &mut body,
            &mut len,
            urids.time_beat,
            urids.atom_double,
            &position.beat.to_ne_bytes(),
        );
        push_property(
            &mut body,
            &mut len,
            urids.time_bar,
            urids.atom_long,
            &position.bar.to_ne_bytes(),
        );
        push_property(
            &mut body,
            &mut len,
            urids.time_bar_beat,
            urids.atom_float,
            &position.bar_beat.to_ne_bytes(),
        );
        push_property(
            &mut body,
            &mut len,
            urids.time_beats_per_minute,
            urids.atom_float,
            &position.beats_per_minute.to_ne_bytes(),
        );
        push_property(
            &mut body,
            &mut len,
            urids.time_beats_per_bar,
            urids.atom_float,
            &position.beats_per_bar.to_ne_bytes(),
        );
        push_property(
            &mut body,
            &mut len,
            urids.time_speed,
            urids.atom_float,
            &position.speed.to_ne_bytes(),
        );
        push_property(
            &mut body,
            &mut len,
            urids.time_beat_unit,
            urids.atom_int,
            &TIME_BEAT_UNIT_QUARTER.to_ne_bytes(),
        );

        if len > body.len() {
            return false;
        }
        let total = align8(16 + len);
        if self.size + total > self.capacity() {
            return false;
        }
        let offset = self.size;
        self.write_event_header(offset, len, urids.object);
        let dst = &mut self.bytes[8 + offset..];
        dst[0..8].copy_from_slice(&time.to_ne_bytes());
        dst[16..16 + len].copy_from_slice(&body[..len]);
        self.size += total;
        true
    }

    /// Commit the accumulated size back into the sequence header.
    pub fn finish(self) {
        let size = self.size as u32;
        self.bytes[0..4].copy_from_slice(&size.to_ne_bytes());
    }
}

/// Appends a complete `LV2_Atom` (header + body) as an event at `time`. Used to
/// forward UI→DSP events (already an atom) into a plugin's input sequence.
pub fn append_atom_event(writer: &mut SequenceWriter, time: i64, atom: &[u8]) -> bool {
    if atom.len() < 8 {
        return false;
    }
    let size = u32::from_ne_bytes(atom[0..4].try_into().unwrap()) as usize;
    let type_ = u32::from_ne_bytes(atom[4..8].try_into().unwrap());
    if atom.len() < 8 + size {
        return false;
    }
    let body = &atom[8..8 + size];
    let total = align8(16 + body.len());
    if writer.size + total > writer.capacity() {
        return false;
    }
    let offset = writer.size;
    writer.write_event_header(offset, body.len(), type_);
    let dst = &mut writer.bytes[8 + offset..];
    dst[0..8].copy_from_slice(&time.to_ne_bytes());
    dst[16..16 + body.len()].copy_from_slice(body);
    writer.size += total;
    true
}

fn push_property(buf: &mut [u8], len: &mut usize, key: u32, value_type: u32, value: &[u8]) {
    // LV2_Atom_Property_Body { key; context; value } — the value atom follows a
    // 4-byte `context` field, which the LV2 iterator skips when advancing.
    let needed = *len + 16 + value.len();
    if needed > buf.len() {
        *len = buf.len() + 1; // signal overflow
        return;
    }
    buf[*len..*len + 4].copy_from_slice(&key.to_ne_bytes());
    buf[*len + 4..*len + 8].copy_from_slice(&0u32.to_ne_bytes()); // context
    buf[*len + 8..*len + 12].copy_from_slice(&(value.len() as u32).to_ne_bytes());
    buf[*len + 12..*len + 16].copy_from_slice(&value_type.to_ne_bytes());
    buf[*len + 16..*len + 16 + value.len()].copy_from_slice(value);
    *len += 16 + value.len();
    while *len % 8 != 0 {
        buf[*len] = 0;
        *len += 1;
    }
}

/// A tiny reader used by the atom tests (a starting point for a future MIDI
/// output path).
#[cfg(test)]
pub fn read_events(words: &[u64]) -> Vec<(i64, u32, Vec<u8>)> {
    let bytes = unsafe { std::slice::from_raw_parts(words.as_ptr() as *const u8, words.len() * 8) };
    let mut size = u32::from_ne_bytes(bytes[0..4].try_into().unwrap()) as usize;
    let capacity = bytes.len().saturating_sub(8);
    size = size.min(capacity);
    let mut offset = SEQUENCE_BODY;
    let mut events = Vec::new();
    while offset + 16 <= size {
        let body = &bytes[8 + offset..];
        let time = i64::from_ne_bytes(body[0..8].try_into().unwrap());
        let value_size = u32::from_ne_bytes(body[8..12].try_into().unwrap()) as usize;
        let value_type = u32::from_ne_bytes(body[12..16].try_into().unwrap());
        let value = body[16..16 + value_size].to_vec();
        events.push((time, value_type, value));
        offset += align8(16 + value_size);
    }
    events
}

#[cfg(test)]
extern "C" {
    fn vibemix_test_position_bpm(
        buffer: *const std::os::raw::c_void,
        urid_object: u32,
        urid_position: u32,
        urid_float: u32,
        urid_bpm: u32,
    ) -> f32;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sequence_parses_with_real_lv2_iterators() {
        let mut buffer = vec![0u64; 128].into_boxed_slice();
        let position = TimePosition {
            frame: 4800,
            beat: 2.5,
            beats_per_minute: 135.0,
            beats_per_bar: 4.0,
            speed: 1.0,
            ..Default::default()
        };
        {
            let mut writer = SequenceWriter::new(&mut buffer);
            assert!(writer.append_position(0, &position, &urids()));
            writer.finish();
        }
        let u = urids();
        let bpm = unsafe {
            vibemix_test_position_bpm(
                buffer.as_ptr() as *const std::os::raw::c_void,
                u.object,
                u.time_position,
                u.atom_float,
                u.time_beats_per_minute,
            )
        };
        assert!((bpm - 135.0).abs() < 1e-6, "bpm={bpm}");
    }

    fn urids() -> AtomUrids {
        AtomUrids {
            object: 10,
            midi_event: 20,
            time_position: 30,
            time_frame: 31,
            time_beat: 32,
            time_bar: 33,
            time_bar_beat: 34,
            time_beats_per_minute: 35,
            time_beats_per_bar: 36,
            time_beat_unit: 38,
            time_speed: 37,
            atom_long: 40,
            atom_double: 41,
            atom_float: 42,
            atom_int: 43,
        }
    }

    #[test]
    fn midi_round_trip() {
        let mut buffer = vec![0u64; 64].into_boxed_slice();
        {
            let mut writer = SequenceWriter::new(&mut buffer);
            assert!(writer.append_midi(0, &[0x90, 60, 100], &urids()));
            assert!(writer.append_midi(128, &[0x80, 60, 0], &urids()));
            writer.finish();
        }
        let events = read_events(&buffer);
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].0, 0);
        assert_eq!(events[0].1, 20);
        assert_eq!(events[0].2, vec![0x90, 60, 100]);
        assert_eq!(events[1].0, 128);
        assert_eq!(events[1].2, vec![0x80, 60, 0]);
    }

    /// Parse the object body the way LV2's `lv2_atom_object_next` does, to make
    /// sure each property has the required `key; context; value` layout.
    fn find_float_property(body: &[u8], key: u32) -> Option<f32> {
        let mut offset = 8; // id + otype
        while offset + 16 <= body.len() {
            let prop_key = u32::from_ne_bytes(body[offset..offset + 4].try_into().unwrap());
            let _context = u32::from_ne_bytes(body[offset + 4..offset + 8].try_into().unwrap());
            let size =
                u32::from_ne_bytes(body[offset + 8..offset + 12].try_into().unwrap()) as usize;
            let _type = u32::from_ne_bytes(body[offset + 12..offset + 16].try_into().unwrap());
            if prop_key == key && size == 4 {
                return Some(f32::from_ne_bytes(
                    body[offset + 16..offset + 20].try_into().unwrap(),
                ));
            }
            // Property stride: key + context + atom header + body, padded to 8.
            let stride = align8(16 + size);
            offset += stride;
        }
        None
    }

    fn find_int_property(body: &[u8], key: u32) -> Option<u32> {
        let mut offset = 8; // id + otype
        while offset + 16 <= body.len() {
            let prop_key = u32::from_ne_bytes(body[offset..offset + 4].try_into().unwrap());
            let size =
                u32::from_ne_bytes(body[offset + 8..offset + 12].try_into().unwrap()) as usize;
            if prop_key == key && size == 4 {
                return Some(u32::from_ne_bytes(
                    body[offset + 16..offset + 20].try_into().unwrap(),
                ));
            }
            let stride = align8(16 + size);
            offset += stride;
        }
        None
    }

    #[test]
    fn position_properties_use_lv2_layout() {
        let mut buffer = vec![0u64; 128].into_boxed_slice();
        let position = TimePosition {
            frame: 4800,
            beat: 2.5,
            bar: 1,
            bar_beat: 2.5,
            beats_per_minute: 135.0,
            beats_per_bar: 4.0,
            speed: 1.0,
        };
        {
            let mut writer = SequenceWriter::new(&mut buffer);
            assert!(writer.append_position(0, &position, &urids()));
            writer.finish();
        }
        let events = read_events(&buffer);
        assert_eq!(events.len(), 1);
        let body = &events[0].2;
        let bpm = find_float_property(body, urids().time_beats_per_minute).expect("bpm property");
        assert!((bpm - 135.0).abs() < 1e-6, "bpm={bpm}");
        let beat_unit = find_int_property(body, urids().time_beat_unit).expect("beatUnit property");
        assert_eq!(beat_unit, TIME_BEAT_UNIT_QUARTER);
    }

    #[test]
    fn position_round_trip() {
        let mut buffer = vec![0u64; 64].into_boxed_slice();
        let position = TimePosition {
            frame: 4800,
            beat: 2.5,
            bar: 1,
            bar_beat: 2.5,
            beats_per_minute: 120.0,
            beats_per_bar: 4.0,
            speed: 1.0,
        };
        {
            let mut writer = SequenceWriter::new(&mut buffer);
            assert!(writer.append_position(0, &position, &urids()));
            writer.finish();
        }
        let events = read_events(&buffer);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].0, 0);
        assert_eq!(events[0].1, urids().object);
        // Body starts with id + otype.
        assert_eq!(
            u32::from_ne_bytes(events[0].2[4..8].try_into().unwrap()),
            30
        );
        assert!(events[0].2.len() > 8);
    }
}
