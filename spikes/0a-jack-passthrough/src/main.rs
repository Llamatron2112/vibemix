use anyhow::Result;
use jack::{AudioIn, AudioOut, Client, ClientOptions, Control, ProcessHandler, ProcessScope};

struct Passthrough {
    in_l: jack::Port<AudioIn>,
    in_r: jack::Port<AudioIn>,
    out_l: jack::Port<AudioOut>,
    out_r: jack::Port<AudioOut>,
}

impl ProcessHandler for Passthrough {
    fn process(&mut self, _client: &Client, ps: &ProcessScope) -> Control {
        let in_l = self.in_l.as_slice(ps);
        let in_r = self.in_r.as_slice(ps);
        self.out_l.as_mut_slice(ps).copy_from_slice(in_l);
        self.out_r.as_mut_slice(ps).copy_from_slice(in_r);
        Control::Continue
    }
}

fn main() -> Result<()> {
    let name = "vibemix-spike-0a";

    let (client, _status) = Client::new(name, ClientOptions::NO_START_SERVER)?;

    let in_l = client.register_port("in_l", AudioIn::default())?;
    let in_r = client.register_port("in_r", AudioIn::default())?;
    let out_l = client.register_port("out_l", AudioOut::default())?;
    let out_r = client.register_port("out_r", AudioOut::default())?;

    let handler = Passthrough {
        in_l,
        in_r,
        out_l,
        out_r,
    };

    let active = client.activate_async((), handler)?;

    println!("backend: jack (via pipewire-jack)");
    println!("sample_rate: {}", active.as_client().sample_rate());
    println!("buffer_size: {}", active.as_client().buffer_size());

    let pairs = [
        (format!("{name}:out_l"), "system:playback_1".to_string()),
        (format!("{name}:out_r"), "system:playback_2".to_string()),
        ("system:capture_1".to_string(), format!("{name}:in_l")),
        ("system:capture_2".to_string(), format!("{name}:in_r")),
    ];

    for (source, destination) in &pairs {
        match active
            .as_client()
            .connect_ports_by_name(source, destination)
        {
            Ok(()) => println!("connected {source} -> {destination}"),
            Err(error) => eprintln!("connect failed {source} -> {destination}: {error}"),
        }
    }

    println!("passthrough running; press Enter to quit");
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;

    Ok(())
}
