use jack::{AudioOut, Client, ClientOptions, Control, ProcessHandler, ProcessScope};

struct Tone {
    out_l: jack::Port<AudioOut>,
    out_r: jack::Port<AudioOut>,
    phase: f32,
    step: f32,
}

impl ProcessHandler for Tone {
    fn process(&mut self, _client: &Client, scope: &ProcessScope) -> Control {
        let left = self.out_l.as_mut_slice(scope);
        let right = self.out_r.as_mut_slice(scope);
        for index in 0..left.len() {
            let sample = (self.phase * std::f32::consts::TAU).sin() * 0.3;
            left[index] = sample;
            right[index] = sample;
            self.phase += self.step;
            if self.phase >= 1.0 {
                self.phase -= 1.0;
            }
        }
        Control::Continue
    }
}

fn main() {
    let (client, _status) =
        Client::new("vibemix-tone", ClientOptions::NO_START_SERVER).expect("create client");
    let out_l = client
        .register_port("out_l", AudioOut::default())
        .expect("register port");
    let out_r = client
        .register_port("out_r", AudioOut::default())
        .expect("register port");

    let step = 440.0 / client.sample_rate() as f32;
    let active = client
        .activate_async(
            (),
            Tone {
                out_l,
                out_r,
                phase: 0.0,
                step,
            },
        )
        .expect("activate");

    // The engine's JACK client name (default "vibemix"). Test harnesses pass a
    // private name so the tone can never be linked into a real session.
    let target = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "vibemix".to_string());
    let _ = active
        .as_client()
        .connect_ports_by_name("vibemix-tone:out_l", &format!("{target}:ch_1_in_l"));
    let _ = active
        .as_client()
        .connect_ports_by_name("vibemix-tone:out_r", &format!("{target}:ch_1_in_r"));

    println!("440 Hz tone -> {target}:ch_1 (strip 0); running until killed");
    loop {
        std::thread::sleep(std::time::Duration::from_secs(3600));
    }
}
