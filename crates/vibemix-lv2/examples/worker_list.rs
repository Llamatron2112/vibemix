use vibemix_lv2::Host;

const WORKER: &str = "http://lv2plug.in/ns/ext/worker#schedule";

fn main() {
    let host = Host::new();
    let mut required = 0;
    let mut optional = 0;

    for info in host.discover() {
        let has = |features: &[String]| features.iter().any(|uri| uri == WORKER);
        let channels = format!("{}/{}", info.audio_inputs.len(), info.audio_outputs.len());
        if has(&info.required_features) {
            required += 1;
            println!("REQ [{channels}] {}", info.name);
        } else if has(&info.optional_features) {
            optional += 1;
            if optional <= 15 {
                println!("OPT [{channels}] {}", info.name);
            }
        }
    }

    println!("all plugins requiring worker: {required}, opting in optionally: {optional}");
}
