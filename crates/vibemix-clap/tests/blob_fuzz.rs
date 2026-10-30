//! Fuzzing of the hex state-blob codec: saved plugin state comes from session
//! files, which may come from anywhere, so decoding must never panic.

use vibemix_clap::{decode_blob, encode_blob};

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
}

fn iterations() -> usize {
    std::env::var("VIBEMIX_FUZZ_ITERS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(100_000)
}

#[test]
fn blobs_round_trip() {
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    for length in [0usize, 1, 2, 3, 255, 4096] {
        let bytes: Vec<u8> = (0..length).map(|_| rng.next() as u8).collect();
        assert_eq!(decode_blob(&encode_blob(&bytes)), bytes);
    }
}

#[test]
fn decoding_hostile_text_never_panics() {
    let mut rng = Rng(0xD1B5_4A32_D192_ED03);
    let alphabet: Vec<char> = "0123456789abcdefABCDEFxyz \n\u{e9}\u{1f3b5}\u{0}-"
        .chars()
        .collect();
    for _ in 0..iterations() {
        let length = (rng.next() % 300) as usize;
        let text: String = (0..length)
            .map(|_| alphabet[(rng.next() % alphabet.len() as u64) as usize])
            .collect();
        let decoded = decode_blob(&text);
        assert!(decoded.len() <= text.len() / 2 + 1);
    }
}
