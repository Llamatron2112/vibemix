//! Mutation fuzzing of session files.
//!
//! A session is untrusted input (it can be downloaded or shared), so whatever
//! parses must survive being clamped to the engine's capacity, applied to the
//! shared parameters, reported to the GUI and analysed, without a panic.
//!
//! Deterministic (seeded) and dependency-free; set `VIBEMIX_FUZZ_ITERS` to run
//! longer (`just fuzz` does).

use super::*;
use vibemix_core::ids::PluginId;

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
}

fn iterations(default: usize) -> usize {
    std::env::var("VIBEMIX_FUZZ_ITERS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(default)
}

/// A session with every kind of content: several strips and buses, sends,
/// plugins with state and sidechains, MIDI bindings.
fn rich_model() -> MixerModel {
    let mut model = MixerModel::default_minimal();
    for id in 1..4 {
        model
            .strips
            .push(Strip::new(StripId(id), format!("strip {id}")));
    }
    for id in 1..3 {
        model
            .buses
            .insert(0, Bus::new(BusId(id), format!("bus {id}")));
    }
    model.normalize_buses();
    for strip in &mut model.strips {
        for bus in &model.buses {
            let mut send = Send::new(bus.id);
            send.delay_ms = 12.5;
            send.pre_fader = true;
            strip.sends.push(send);
        }
    }
    let mut plugin = PluginSlot::new(PluginId(1), "urn:test:plugin");
    plugin.params = vec![0.5, 1.0, -3.0];
    plugin.state = "0a1b2c".to_string();
    plugin.sidechain_enabled = true;
    plugin.sidechain.push(SidechainSource::new(StripId(1)));
    model.strips[0].plugins.push(plugin.clone());
    model.strips[2].plugins_post.push(plugin.clone());
    model.buses[0].plugins.push(plugin);
    model
}

fn mutate_text(rng: &mut Rng, text: &str) -> String {
    const HOSTILE: &[&str] = &[
        "0",
        "-1",
        "1",
        "4294967295",
        "4294967296",
        "-2147483648",
        "2147483647",
        "9223372036854775807",
        "18446744073709551615",
        "1e308",
        "-1e308",
        "nan",
        "inf",
        "\"\"",
        "[]",
        "{}",
        "true",
        "false",
        "\"\\u0000\"",
        "999999",
    ];
    let mut bytes = text.as_bytes().to_vec();
    for _ in 0..1 + rng.below(5) {
        if bytes.is_empty() {
            break;
        }
        let at = rng.below(bytes.len());
        match rng.below(6) {
            0 => bytes[at] ^= 1 << rng.below(8),
            1 => bytes[at] = rng.next() as u8,
            2 => bytes.truncate(at),
            3 => {
                // Replace the number-ish token around `at` with a hostile one.
                let is_token = |byte: u8| byte.is_ascii_alphanumeric() || b"-+.".contains(&byte);
                let mut start = at;
                while start > 0 && is_token(bytes[start - 1]) {
                    start -= 1;
                }
                let mut end = at;
                while end < bytes.len() && is_token(bytes[end]) {
                    end += 1;
                }
                let value = HOSTILE[rng.below(HOSTILE.len())];
                bytes.splice(start..end, value.bytes());
            }
            4 => {
                let line_start = bytes[..at]
                    .iter()
                    .rposition(|b| *b == b'\n')
                    .map_or(0, |p| p + 1);
                let line_end = bytes[at..]
                    .iter()
                    .position(|b| *b == b'\n')
                    .map_or(bytes.len(), |p| at + p);
                let line = bytes[line_start..line_end].to_vec();
                bytes.splice(line_start..line_start, line.into_iter().chain(*b"\n"));
            }
            _ => {
                let line_start = bytes[..at]
                    .iter()
                    .rposition(|b| *b == b'\n')
                    .map_or(0, |p| p + 1);
                let line_end = bytes[at..]
                    .iter()
                    .position(|b| *b == b'\n')
                    .map_or(bytes.len(), |p| at + p);
                bytes.drain(line_start..line_end);
            }
        }
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

/// Everything the engine does with a freshly loaded session that needs no audio
/// hardware.
fn exercise(mut model: MixerModel) {
    clamp_to_capacity(&mut model, 8, 4);
    trust_catalog_bundles(&mut model, &[]);
    let params = SharedParams::new(8, 4);
    apply_model(&params, &model);
    sync_model_from_params(&params, &mut model);
    let json = build_state_json(&model, &params, &[], 48_000, 0, (0.0, 0.0), 0, 0, 0);
    assert!(serde_json::from_str::<serde_json::Value>(&json).is_ok());
    let _ = compute_strip_order(&model, 8);
    let _ = sidechain_signature(&model);
    let copy = model.clone();
    assert!(structure_eq(&model, &copy));
    let _ = model_bytes(&model);
    let _ = toml::to_string_pretty(&model);
}

#[test]
fn hostile_sessions_never_panic() {
    let seed = toml::to_string_pretty(&rich_model()).expect("serialize the seed session");
    // The seed itself must be fine and survive a round trip.
    exercise(toml::from_str(&seed).expect("the seed parses"));

    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    let mut parsed = 0usize;
    let total = iterations(2_000);
    for _ in 0..total {
        let text = mutate_text(&mut rng, &seed);
        if let Ok(model) = toml::from_str::<MixerModel>(&text) {
            parsed += 1;
            exercise(model);
        }
    }
    // Mutations that stay valid TOML are the interesting ones; make sure the
    // harness really exercises them rather than failing every parse.
    assert!(
        parsed * 21 > total,
        "only {parsed} of {total} mutated sessions parsed"
    );
}

#[test]
fn hostile_sweep_output_never_panics() {
    use crate::health::parse_scan_line;
    let seeds = [
        r#"{"scan":"start","total":3}"#,
        r#"{"scan":"plugin","uri":"u","name":"n","status":"failed","reason":"crash SIGSEGV","inputs":2}"#,
        r#"{"scan":"done","total":3,"failed":1}"#,
    ];
    let mut rng = Rng(0xD1B5_4A32_D192_ED03);
    for _ in 0..iterations(60_000) {
        let seed = seeds[rng.below(seeds.len())];
        let text = mutate_text(&mut rng, seed);
        let _ = parse_scan_line(&text);
    }
}
