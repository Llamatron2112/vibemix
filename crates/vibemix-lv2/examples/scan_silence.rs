use vibemix_lv2::Host;

use std::io::Write;

fn main() {
    let host = Host::new();
    let frames = 256usize;
    let mut silent = 0;
    let mut audible = 0;

    for info in host.discover() {
        if !info.hostable || info.audio_inputs.len() != 2 || info.audio_outputs.len() != 2 {
            continue;
        }
        let Some(mut instance) = host.instantiate(&info.uri, 48000.0, frames, 8192) else {
            continue;
        };

        print!("TRY  {}\r", info.name);
        let _ = std::io::stdout().flush();

        let mut peak_out = 0.0f32;
        for _ in 0..40 {
            for channel in 0..instance.audio_input_count() {
                for sample in instance.input_buffer_mut(channel)[..frames].iter_mut() {
                    *sample = 0.5;
                }
            }
            instance.run(frames);
            for channel in 0..instance.audio_output_count() {
                for sample in &instance.output_buffer(channel)[..frames] {
                    peak_out = peak_out.max(sample.abs());
                }
            }
        }

        if peak_out < 1.0e-4 {
            silent += 1;
            let controls: Vec<String> = (0..instance.control_count().min(4))
                .map(|param| {
                    format!(
                        "{}={}",
                        instance.control_name(param),
                        instance.control_value(param)
                    )
                })
                .collect();
            println!("SILENT {:<40} [{}]", info.name, controls.join(", "));
        } else {
            audible += 1;
        }
    }

    println!("audible={audible} silent={silent}");
}
