use std::io::Write;

use vibemix_lv2::Host;

fn main() {
    let host = Host::new();
    let infos = host.discover();
    println!("discovered {} plugins", infos.len());

    let mut stereo = 0;
    let mut instantiated = 0;

    for info in &infos {
        if info.audio_inputs.len() < 2 || info.audio_outputs.len() < 2 {
            continue;
        }
        stereo += 1;
        print!(
            "TRY  {} [in={} out={} ctrl={} atom={}/{} h={}]",
            info.name,
            info.audio_inputs.len(),
            info.audio_outputs.len(),
            info.controls.len(),
            info.atom_inputs.len(),
            info.atom_outputs.len(),
            info.hostable
        );
        let _ = std::io::stdout().flush();

        if info.name.contains("Reverse Delay") {
            println!(
                "     req={:?} opt={:?}",
                info.required_features, info.optional_features
            );
        }
        match host.instantiate(&info.uri, 48000.0, 64, 8192) {
            Some(mut instance) => {
                print!(" inst");
                let _ = std::io::stdout().flush();
                instance.run(64);
                instantiated += 1;
                println!(" ran");
            }
            None => println!(" skip"),
        }
    }

    println!("stereo plugins: {stereo}, instantiated: {instantiated}");
}
