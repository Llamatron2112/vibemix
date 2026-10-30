//! Mutation fuzzing of the control-protocol decoder.
//!
//! The decoder runs on whatever arrives at the control socket (and, in the GUI,
//! on whatever the engine sends), so it must return `None` for anything it
//! cannot understand and never panic, abort or allocate without bound.
//!
//! Deterministic (seeded) and dependency-free, so it runs in `cargo test`. Set
//! `VIBEMIX_FUZZ_ITERS` to run longer (`just fuzz` does).

use rosc::{encoder, OscMessage, OscPacket, OscType};
use vibemix_proto::message::ChunkAssembler;
use vibemix_proto::ControlMessage;

/// xorshift64*: small, fast, good enough to drive a fuzzer.
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
    fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[self.below(items.len())]
    }
}

fn iterations(default: usize) -> usize {
    std::env::var("VIBEMIX_FUZZ_ITERS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(default)
}

/// Every address the protocol knows, read from the source so new ones are
/// fuzzed automatically.
fn addresses() -> Vec<String> {
    let source = include_str!("../src/address.rs");
    source
        .split('"')
        .filter(|piece| piece.starts_with("/vibemix/"))
        .map(str::to_string)
        .collect()
}

const INTS: &[i32] = &[
    i32::MIN,
    -1_000_000,
    -65536,
    -1,
    0,
    0,
    0,
    1,
    1,
    1,
    2,
    3,
    5,
    31,
    32,
    63,
    64,
    65,
    100,
    255,
    256,
    4096,
    65535,
    65536,
    1_000_000,
    i32::MAX - 1,
    i32::MAX,
];
const FLOATS: &[f32] = &[
    f32::NAN,
    f32::INFINITY,
    f32::NEG_INFINITY,
    0.0,
    -0.0,
    1.0,
    -1.0,
    0.5,
    2.0,
    4.0,
    1e30,
    -1e30,
    f32::MIN_POSITIVE,
    f32::MAX,
];

fn string(rng: &mut Rng) -> String {
    match rng.below(6) {
        0 => String::new(),
        1 => "a".repeat(rng.below(5000)),
        2 => "\u{e9}\u{1f3b5}\u{0}x".to_string(),
        3 => "../../etc/passwd".to_string(),
        4 => "{\"json\": [1, 2,".to_string(),
        _ => format!("name {}", rng.below(1000)),
    }
}

fn argument(rng: &mut Rng, kind: char) -> OscType {
    match kind {
        'i' => OscType::Int(*rng.pick(INTS)),
        'f' => OscType::Float(*rng.pick(FLOATS)),
        's' => OscType::String(string(rng)),
        'b' => OscType::Blob((0..rng.below(300)).map(|_| rng.next() as u8).collect()),
        'l' => OscType::Long(rng.next() as i64),
        'd' => OscType::Double(f64::from_bits(rng.next())),
        _ => OscType::Nil,
    }
}

fn encode(address: &str, args: Vec<OscType>) -> Vec<u8> {
    encoder::encode(&OscPacket::Message(OscMessage {
        addr: address.to_string(),
        args,
    }))
    .unwrap_or_default()
}

/// A well-formed-looking message: a known address with a plausible argument
/// shape filled with hostile values.
fn plausible(rng: &mut Rng, addresses: &[String]) -> Vec<u8> {
    const SHAPES: &[&str] = &[
        "",
        "i",
        "ii",
        "if",
        "is",
        "s",
        "f",
        "iif",
        "iii",
        "iiii",
        "iiiif",
        "iiiii",
        "iiiiis",
        "iiss",
        "iisi",
        "sii",
        "ib",
        "iiib",
        "iiiiiiii",
        "iiffff",
        "iiiiiiiiiiii",
        "b",
    ];
    let address = rng.pick(addresses).clone();
    let shape = *rng.pick(SHAPES);
    let args = shape.chars().map(|kind| argument(rng, kind)).collect();
    encode(&address, args)
}

/// A meters message: the counts and the float runs are what the decoder trusts.
fn meters(rng: &mut Rng) -> Vec<u8> {
    let mut args = Vec::new();
    args.push(OscType::Int(*rng.pick(INTS)));
    for _ in 0..rng.below(60) {
        args.push(match rng.below(5) {
            0 => OscType::Int(*rng.pick(INTS)),
            _ => OscType::Float(*rng.pick(FLOATS)),
        });
    }
    encode("/vibemix/meters", args)
}

/// Bit flips, byte overwrites, truncation, splicing and appended noise.
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
            5 => data.extend((0..rng.below(64)).map(|_| rng.next() as u8)),
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

#[test]
fn decoding_hostile_messages_never_panics() {
    let addresses = addresses();
    assert!(
        addresses.len() > 50,
        "found only {} addresses",
        addresses.len()
    );
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    for _ in 0..iterations(60_000) {
        let data = match rng.below(4) {
            0 => plausible(&mut rng, &addresses),
            1 => meters(&mut rng),
            2 => {
                let seed = plausible(&mut rng, &addresses);
                mutate(&mut rng, seed)
            }
            _ => {
                let seed = meters(&mut rng);
                mutate(&mut rng, seed)
            }
        };
        let _ = ControlMessage::decode(&data);
    }
}

#[test]
fn decoding_random_bytes_never_panics() {
    let mut rng = Rng(0xD1B5_4A32_D192_ED03);
    for _ in 0..iterations(60_000) {
        let length = rng.below(400);
        let data: Vec<u8> = (0..length).map(|_| rng.next() as u8).collect();
        let _ = ControlMessage::decode(&data);
    }
}

/// A meters message that claims a huge (or negative) number of strips must be
/// rejected cheaply, not allocate gigabytes or overflow.
#[test]
fn meters_with_absurd_counts_are_rejected() {
    for count in [i32::MIN, -1, 0x4000_0000, i32::MAX] {
        let data = encode(
            "/vibemix/meters",
            vec![OscType::Int(count), OscType::Float(0.0), OscType::Int(1)],
        );
        assert!(
            ControlMessage::decode(&data).is_none(),
            "count {count} was accepted"
        );
    }
}

#[test]
fn reassembling_hostile_chunks_stays_bounded() {
    let mut rng = Rng(0x0123_4567_89AB_CDEF);
    let mut assembler = ChunkAssembler::new();
    for _ in 0..iterations(60_000) {
        let message = ControlMessage::Chunk {
            transfer: rng.below(4) as u32,
            index: *rng.pick(&[0u32, 1, 2, 5, 4095, 4096, 100_000, u32::MAX]),
            total: *rng.pick(&[0u32, 1, 2, 3, 4096, 4097, 1_000_000, u32::MAX]),
            data: (0..rng.below(64)).map(|_| rng.next() as u8).collect(),
        };
        let _ = assembler.accept(message);
    }
}
