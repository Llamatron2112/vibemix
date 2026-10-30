//! Test helper: a JACK client that sends a 120 BPM MIDI clock (24 PPQN) with a
//! START at the beginning, plus a steady note stream, into `<client>:midi_in` (default client `vibemix`).
//! Used by `just rt-count` to exercise the clock, transport and MIDI paths.

use jack::{Client, ClientOptions, Control, MidiOut, ProcessHandler, ProcessScope, RawMidi};

struct ClockSender {
    port: jack::Port<MidiOut>,
    /// Frames until the next clock pulse (can be fractional).
    until_pulse: f64,
    pulse_frames: f64,
    started: bool,
    note_phase: u32,
}

impl ProcessHandler for ClockSender {
    fn process(&mut self, _client: &Client, scope: &ProcessScope) -> Control {
        let frames = scope.n_frames() as f64;
        let mut writer = self.port.writer(scope);
        let mut events: Vec<(u32, [u8; 3], usize)> = Vec::new();
        if !self.started {
            events.push((0, [0xFA, 0, 0], 1));
            self.started = true;
        }
        let mut position = self.until_pulse;
        while position < frames {
            events.push((position as u32, [0xF8, 0, 0], 1));
            position += self.pulse_frames;
        }
        self.until_pulse = position - frames;
        // A note on/off pair every 48 pulses' worth of blocks keeps notes flowing.
        self.note_phase += 1;
        if self.note_phase % 64 == 0 {
            events.push((1, [0x90, 60, 100], 3));
        } else if self.note_phase % 64 == 32 {
            events.push((1, [0x80, 60, 0], 3));
        }
        events.sort_by_key(|event| event.0);
        for (time, bytes, len) in events {
            let _ = writer.write(&RawMidi {
                time,
                bytes: &bytes[..len],
            });
        }
        Control::Continue
    }
}

fn main() {
    let (client, _status) =
        Client::new("vibemix-clock", ClientOptions::NO_START_SERVER).expect("create client");
    let port = client
        .register_port("out", MidiOut::default())
        .expect("register port");
    let pulse_frames = client.sample_rate() as f64 * 60.0 / (120.0 * 24.0);
    let active = client
        .activate_async(
            (),
            ClockSender {
                port,
                until_pulse: 0.0,
                pulse_frames,
                started: false,
                note_phase: 0,
            },
        )
        .expect("activate");
    // The engine's JACK client name (default "vibemix"); see `tone.rs`.
    let target = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "vibemix".to_string());
    let _ = active
        .as_client()
        .connect_ports_by_name("vibemix-clock:out", &format!("{target}:midi_in"));
    println!("120 BPM MIDI clock + notes -> {target}:midi_in; running until killed");
    loop {
        std::thread::sleep(std::time::Duration::from_secs(3600));
    }
}
