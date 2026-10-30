use vibemix_lv2::Host;

fn main() {
    let uri = match std::env::args().nth(1) {
        Some(uri) => uri,
        None => {
            eprintln!("usage: run_plugin <lv2-uri> [input_amplitude]");
            std::process::exit(1);
        }
    };
    let amplitude: f32 = std::env::args()
        .nth(2)
        .and_then(|value| value.parse().ok())
        .unwrap_or(0.5);

    let frames: usize = std::env::var("VIBEMIX_FRAMES")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(512);
    let sample_rate: f64 = std::env::var("VIBEMIX_RATE")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(48000.0);

    let host = Host::new();
    let mut instance = match host.instantiate(&uri, sample_rate, frames, 8192) {
        Some(instance) => instance,
        None => {
            eprintln!("could not instantiate {uri}");
            std::process::exit(1);
        }
    };

    if let (Some(index), Some(value)) = (
        std::env::args()
            .nth(3)
            .and_then(|value| value.parse::<usize>().ok()),
        std::env::args()
            .nth(4)
            .and_then(|value| value.parse::<f32>().ok()),
    ) {
        instance.set_control(index, value);
        println!("set control[{index}] = {value}");
    }

    println!(
        "instantiated: inputs={} outputs={} controls={}",
        instance.audio_input_count(),
        instance.audio_output_count(),
        instance.control_count()
    );
    for param in 0..instance.control_count().min(20) {
        println!(
            "  control[{param}] {} = {} (min {} max {})",
            instance.control_name(param),
            instance.control_value(param),
            instance.control_min(param),
            instance.control_max(param)
        );
    }

    let mut peak_in = 0.0f32;
    let mut peak_out = 0.0f32;

    for _ in 0..20 {
        for channel in 0..instance.audio_input_count() {
            let buffer = instance.input_buffer_mut(channel);
            for sample in buffer[..frames].iter_mut() {
                *sample = amplitude;
                peak_in = peak_in.max(sample.abs());
            }
        }
        instance.run(frames);
        for channel in 0..instance.audio_output_count() {
            let buffer = instance.output_buffer(channel);
            for sample in &buffer[..frames] {
                peak_out = peak_out.max(sample.abs());
            }
        }
    }

    println!("input_peak={peak_in} output_peak={peak_out}");
}
