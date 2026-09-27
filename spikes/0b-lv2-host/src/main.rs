use lilv::plugin::Plugin;
use lilv::World;

const SAMPLE_RATE: f64 = 48000.0;
const NFRAMES: usize = 64;

fn main() {
    let world = World::new();
    world.load_all();

    let audio = world.new_uri("http://lv2plug.in/ns/lv2core#AudioPort");
    let input = world.new_uri("http://lv2plug.in/ns/lv2core#InputPort");
    let output = world.new_uri("http://lv2plug.in/ns/lv2core#OutputPort");

    println!("discovered {} plugins", world.plugins().count());

    let plugin = match world
        .plugins()
        .iter()
        .filter(Plugin::verify)
        .find(|plugin| {
            let ins = plugin
                .iter_ports()
                .filter(|port| port.is_a(&audio) && port.is_a(&input))
                .count();
            let outs = plugin
                .iter_ports()
                .filter(|port| port.is_a(&audio) && port.is_a(&output))
                .count();
            ins >= 2 && outs >= 2
        }) {
        Some(plugin) => plugin,
        None => {
            eprintln!("no stereo-audio LV2 plugin found; install some plugins");
            std::process::exit(1);
        }
    };

    let uri = plugin.uri();
    let name = plugin.name();
    println!(
        "selected: {} ({})",
        uri.as_uri().unwrap_or("?"),
        name.as_str().unwrap_or("?")
    );

    let mut instance = match unsafe { plugin.instantiate(SAMPLE_RATE, []) } {
        Some(instance) => instance,
        None => {
            eprintln!("instantiate failed (plugin may require host features)");
            std::process::exit(1);
        }
    };

    let mut buffers: Vec<Vec<f32>> = (0..plugin.ports_count())
        .map(|_| vec![0.0f32; NFRAMES])
        .collect();

    for (index, buffer) in buffers.iter_mut().enumerate() {
        unsafe { instance.connect_port_mut(index, buffer.as_mut_ptr()) };
    }

    let mut active = unsafe { instance.activate() };
    unsafe { active.run(NFRAMES) };

    println!("ran one block of {NFRAMES} frames at {SAMPLE_RATE} Hz");

    let _ = unsafe { active.deactivate() };
}
