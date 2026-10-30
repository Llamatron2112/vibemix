//! Mutation fuzzing of the byte parsers that take data from outside this
//! crate: the editor-host IPC, LV2 atom sequences and saved plugin state.
//!
//! They must return `None`/empty for anything malformed and never panic, read
//! out of bounds or allocate without bound. Deterministic (seeded) and
//! dependency-free; set `VIBEMIX_FUZZ_ITERS` to run longer (`just fuzz` does).

use crate::atom::{for_each_event, AtomUrids, SequenceWriter};
use crate::state::{decode_entries, encode_entries, StateEntry};
use crate::ui_ipc::{decode, encode, frame, rewrite_atom_type, FrameBuffer, UiMessage};
use crate::urid::UridMap;

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
    fn bytes(&mut self, max: usize) -> Vec<u8> {
        let length = self.below(max + 1);
        (0..length).map(|_| self.next() as u8).collect()
    }
}

fn iterations(default: usize) -> usize {
    std::env::var("VIBEMIX_FUZZ_ITERS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(default)
}

fn mutate(rng: &mut Rng, mut data: Vec<u8>) -> Vec<u8> {
    for _ in 0..1 + rng.below(6) {
        if data.is_empty() {
            data.push(rng.next() as u8);
            continue;
        }
        let at = rng.below(data.len());
        match rng.below(8) {
            0 => data[at] ^= 1 << rng.below(8),
            1 => data[at] = rng.next() as u8,
            2 => data[at] = 0xFF,
            3 => data[at] = 0,
            4 => data.truncate(at),
            5 => data.extend(rng.bytes(64)),
            6 => {
                let end = (at + rng.below(32)).min(data.len());
                let piece: Vec<u8> = data[at..end].to_vec();
                data.splice(at..at, piece);
            }
            _ => data.insert(at, rng.next() as u8),
        }
    }
    data
}

fn sample_messages() -> Vec<UiMessage> {
    vec![
        UiMessage::Open {
            id: 7,
            plugin_uri: "urn:plugin".into(),
            ui_uri: "urn:plugin#ui".into(),
            ui_type: "http://lv2plug.in/ns/extensions/ui#X11UI".into(),
            bundle: "/usr/lib/lv2/p.lv2/".into(),
            binary: "/usr/lib/lv2/p.lv2/ui.so".into(),
            title: "Plugin".into(),
            scale: 1.5,
            update_rate: 30.0,
        },
        UiMessage::Opened { id: 1 },
        UiMessage::OpenFailed {
            id: 2,
            reason: "no".into(),
        },
        UiMessage::PortEvent {
            id: 3,
            port: 4,
            format: 5,
            type_uri: Some("urn:type".into()),
            bytes: vec![1, 2, 3, 4, 5, 6, 7, 8],
        },
        UiMessage::PortEvent {
            id: 3,
            port: 4,
            format: 0,
            type_uri: None,
            bytes: vec![0; 4],
        },
        UiMessage::Write {
            id: 1,
            port: 2,
            format: 0,
            bytes: vec![0, 0, 128, 63],
        },
        UiMessage::Close { id: 9 },
        UiMessage::CloseRequest { id: 9 },
        UiMessage::Title {
            id: 4,
            title: "t".into(),
        },
        UiMessage::Shutdown,
        UiMessage::UridTable {
            entries: vec![("urn:a".into(), 1), ("urn:b".into(), 2)],
        },
        UiMessage::UridRequest {
            uri: "urn:x".into(),
        },
        UiMessage::UridReply {
            uri: "urn:x".into(),
            urid: 12,
        },
    ]
}

#[test]
fn ui_messages_survive_hostile_bytes() {
    let seeds: Vec<Vec<u8>> = sample_messages().iter().map(encode).collect();
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    for _ in 0..iterations(100_000) {
        let data = match rng.below(3) {
            0 => rng.bytes(300),
            _ => {
                let seed = seeds[rng.below(seeds.len())].clone();
                mutate(&mut rng, seed)
            }
        };
        if let Some(message) = decode(&data) {
            // Whatever decodes must re-encode without trouble.
            let _ = encode(&message);
        }
    }
}

#[test]
fn encoded_messages_round_trip() {
    for message in sample_messages() {
        assert_eq!(decode(&encode(&message)), Some(message));
    }
}

#[test]
fn frame_reassembly_stays_bounded_and_never_panics() {
    let framed: Vec<Vec<u8>> = sample_messages().iter().map(frame).collect();
    let mut rng = Rng(0xD1B5_4A32_D192_ED03);
    for _ in 0..iterations(20_000) {
        let mut buffer = FrameBuffer::new();
        for _ in 0..1 + rng.below(8) {
            let chunk = match rng.below(3) {
                0 => rng.bytes(200),
                1 => {
                    let seed = framed[rng.below(framed.len())].clone();
                    mutate(&mut rng, seed)
                }
                // A hostile length prefix followed by a little data.
                _ => {
                    let mut data = (rng.next() as u32).to_le_bytes().to_vec();
                    data.extend(rng.bytes(16));
                    data
                }
            };
            // Feed in pieces, as a socket would.
            let split = rng.below(chunk.len() + 1);
            buffer.push(&chunk[..split]);
            while buffer.pop().is_some() {}
            buffer.push(&chunk[split..]);
            while buffer.pop().is_some() {}
            let _ = buffer.is_empty();
        }
    }
}

#[test]
fn atom_sequences_survive_hostile_bytes() {
    let map = UridMap::shared();
    let midi = map.map("http://lv2plug.in/ns/ext/midi#MidiEvent");
    let urids = AtomUrids {
        midi_event: midi,
        ..AtomUrids::default()
    };
    let mut rng = Rng(0x0123_4567_89AB_CDEF);
    for _ in 0..iterations(60_000) {
        // A well-formed sequence of a few events, then corrupted.
        let mut words = vec![0u64; 64];
        {
            let mut writer = SequenceWriter::new(&mut words);
            for event in 0..rng.below(6) {
                writer.append_midi(event as i64, &[0x90, 60, 100], &urids);
            }
            writer.finish();
        }
        let mut bytes: Vec<u8> = words.iter().flat_map(|word| word.to_ne_bytes()).collect();
        if rng.below(3) == 0 {
            bytes = rng.bytes(300);
        } else {
            bytes = mutate(&mut rng, bytes);
        }
        let mut seen = 0usize;
        for_each_event(&bytes, |atom| {
            seen += atom.len().min(1);
        });
        assert!(seen <= bytes.len());
        let mut payload = rng.bytes(24);
        let _ = rewrite_atom_type(&mut payload, midi);
    }
}

#[test]
fn saved_plugin_state_survives_hostile_text() {
    let urids = UridMap::shared();
    let entries = vec![
        StateEntry {
            key: urids.map("urn:key"),
            type_: urids.map("urn:type"),
            flags: 3,
            value: vec![1, 2, 3],
        },
        StateEntry {
            key: urids.map("urn:other"),
            type_: urids.map("urn:type"),
            flags: 0,
            value: vec![],
        },
    ];
    let seed = encode_entries(&entries).into_bytes();
    let mut rng = Rng(0xA5A5_5A5A_1234_8765);
    for _ in 0..iterations(60_000) {
        let data = match rng.below(3) {
            0 => rng.bytes(200),
            _ => mutate(&mut rng, seed.clone()),
        };
        let text = String::from_utf8_lossy(&data).into_owned();
        let _ = decode_entries(&text);
    }
}
