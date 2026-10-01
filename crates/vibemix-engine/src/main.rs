mod backend_jack;
mod catalog;
mod control;
mod dsp;
mod health;
mod midi;
mod midi_map;
mod params;
mod plugin;
mod rt_guard;
mod smoke;
mod ui_service;

use std::sync::Arc;

use anyhow::{bail, Context, Result};
use clap::Parser;
use vibemix_core::model::MixerModel;
use vibemix_lv2::{find_ui, Host};

use crate::backend_jack::JackEngine;
use crate::params::{SharedMeters, SharedParams};

#[global_allocator]
static GLOBAL_ALLOCATOR: rt_guard::GuardAllocator = rt_guard::GuardAllocator;

#[derive(Debug, Parser)]
#[command(name = "vibemix-engine", version, about = "vibemix audio mixer engine")]
struct Args {
    #[arg(long, default_value = "vibemix")]
    client_name: String,

    #[arg(long, default_value_t = 9000)]
    osc_port: u16,

    #[arg(long, default_value_t = 8)]
    strips: usize,

    #[arg(long, default_value_t = 4)]
    buses: usize,

    #[arg(long)]
    session: Option<String>,

    #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
    auto_connect: bool,

    #[arg(long, hide = true)]
    smoke: Option<String>,

    #[arg(long, hide = true)]
    smoke_ui: Option<String>,

    /// Smoke-test a VST3 plugin: `--smoke-vst3 <bundle> <class-id>`.
    #[arg(long, hide = true, num_args = 2, value_names = ["BUNDLE", "CLASS"])]
    smoke_vst3: Vec<String>,

    /// Smoke-test a VST3 editor headlessly: `--smoke-vst3-ui <bundle> <class-id>`.
    #[arg(long, hide = true, num_args = 2, value_names = ["BUNDLE", "CLASS"])]
    smoke_vst3_ui: Vec<String>,

    /// Probe a VST3 plugin's audio path: `--probe-vst3 <bundle> <class-id>`.
    #[arg(long, hide = true, num_args = 2, value_names = ["BUNDLE", "CLASS"])]
    probe_vst3: Vec<String>,

    /// Verbose plugin diagnostics (also via VIBEMIX_DEBUG=1).
    #[arg(long)]
    debug: bool,

    /// Test every plugin in isolation and print JSON verdicts, then exit.
    #[arg(long, hide = true)]
    scan_plugins: bool,

    /// Restrict `--scan-plugins` to this URI (repeatable).
    #[arg(long, hide = true)]
    only: Vec<String>,

    /// Serve LV2 editor windows out of process (internal): `--ui-host <socket>`.
    #[arg(long, hide = true, value_name = "SOCKET")]
    ui_host: Option<String>,
}

fn main() -> Result<()> {
    let args = Args::parse();

    if args.debug {
        std::env::set_var("VIBEMIX_DEBUG", "1");
    }

    vibemix_lv2::ui::init();

    if std::env::var_os("VIBEMIX_RT_GUARD").is_some() {
        rt_guard::enable();
    }

    if let Some(path) = args.ui_host.as_deref() {
        let code = match vibemix_lv2::ui_server::run_ui_host(path) {
            Ok(()) => 0,
            Err(error) => {
                eprintln!("[vibemix-ui-host] {error}");
                1
            }
        };
        std::process::exit(code);
    }
    if let Some(uri) = args.smoke.as_deref() {
        run_smoke(uri);
    }
    if let Some(uri) = args.smoke_ui.as_deref() {
        run_smoke_ui(uri);
    }
    if args.smoke_vst3.len() == 2 {
        run_smoke_vst3(&args.smoke_vst3[0], &args.smoke_vst3[1]);
    }
    if args.smoke_vst3_ui.len() == 2 {
        run_smoke_vst3_ui(&args.smoke_vst3_ui[0], &args.smoke_vst3_ui[1]);
    }
    if args.probe_vst3.len() == 2 {
        run_probe_vst3(&args.probe_vst3[0], &args.probe_vst3[1]);
    }
    if args.scan_plugins {
        let only = (!args.only.is_empty()).then_some(args.only);
        smoke::scan_plugins(only);
    }

    let default_level = if smoke::debug_enabled() { "debug" } else { "info" };
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(default_level));
    tracing_subscriber::fmt().with_env_filter(filter).init();

    if args.strips == 0 || args.buses == 0 {
        bail!("strip and bus capacity must be at least 1");
    }

    let mut model = load_or_default(&args)?;
    control::clamp_to_capacity(&mut model, args.strips, args.buses);

    let params = Arc::new(SharedParams::new(args.strips, args.buses));
    let meters = Arc::new(SharedMeters::new(args.strips, args.buses));
    control::apply_model(&params, &model);

    tracing::info!(
        client_name = %args.client_name,
        osc_port = args.osc_port,
        strips = args.strips,
        buses = args.buses,
        "vibemix-engine starting"
    );

    let host = Host::new();
    let vst3 = vibemix_vst3::Host::new();
    let engine = JackEngine::start(
        &args.client_name,
        Arc::clone(&params),
        Arc::clone(&meters),
        args.auto_connect,
        &host,
        &vst3,
        &mut model,
    )
    .context("start jack engine")?;

    tracing::info!(
        sample_rate = engine.sample_rate(),
        buffer_size = engine.buffer_size(),
        "engine running"
    );

    let mut controller =
        control::Controller::new(args.osc_port, params, meters, model, engine, host, vst3)
            .with_context(|| format!("bind OSC port {}", args.osc_port))?;

    tracing::info!(
        session = %args.session.as_deref().unwrap_or("default"),
        "control ready; press Ctrl-C to quit"
    );

    controller.run();
    Ok(())
}

fn run_smoke(uri: &str) -> ! {
    smoke::disable_core_dumps();
    std::process::exit(smoke::smoke_plugin(&Host::new(), uri));
}

fn run_smoke_vst3(bundle: &str, class_id: &str) -> ! {
    smoke::disable_core_dumps();
    std::process::exit(smoke::smoke_plugin_vst3(bundle, class_id));
}

fn run_smoke_vst3_ui(bundle: &str, class_id: &str) -> ! {
    smoke::disable_core_dumps();
    smoke::lower_priority();
    std::process::exit(smoke::smoke_plugin_vst3_ui(bundle, class_id));
}

/// Feeds a sine through a VST3 plugin and reports the output peak per block, so
/// we can tell whether a plugin is silent or the graph drops its audio.
fn run_probe_vst3(bundle: &str, class_id: &str) -> ! {
    let host = vibemix_vst3::Host::new();
    let sample_rate = 48_000.0f64;
    let block = 512usize;
    let Some(mut instance) = host.instantiate(
        bundle,
        class_id,
        sample_rate,
        block,
        vibemix_vst3::PluginInstance::max_frames(),
    ) else {
        eprintln!("probe: instantiate failed");
        std::process::exit(1);
    };
    let in_ch = instance.audio_input_count();
    let out_ch = instance.audio_output_count();
    let params = instance.control_count();
    let set_all = std::env::var_os("VIBEMIX_PROBE_PARAMS").is_some();
    let (setup, active, processing, _) = instance.status();
    eprintln!(
        "probe: in={in_ch} out={out_ch} params={params} set_all={set_all} \
         setup={setup} active={active} processing={processing} sidechain={} buses={} {}",
        instance.has_sidechain(),
        instance.bus_info(),
        instance.editor_info()
    );

    for param in 0..params {
        eprintln!(
            "probe:   [{param}] {:?} default={} stage={} flags={}",
            instance.control_name(param),
            instance.control_default(param),
            instance.controls()[param].step_count,
            instance.controls()[param].flags,
        );
    }

    let defaults: Vec<f32> = (0..params).map(|p| instance.control_default(p)).collect();
    let mut phase = 0.0f32;
    let step = 2.0 * std::f32::consts::PI * 440.0 / sample_rate as f32;
    let mut last_peak = 0.0f32;
    for block_index in 0..64 {
        if set_all {
            for (param, value) in defaults.iter().enumerate() {
                instance.set_control(param, *value);
            }
        }
        for channel in 0..in_ch {
            for sample in instance.input_buffer_mut(channel)[..block].iter_mut() {
                *sample = phase.sin() * 0.5;
                phase += step;
                if phase > std::f32::consts::TAU {
                    phase -= std::f32::consts::TAU;
                }
            }
        }
        instance.run(block);
        let mut peak = 0.0f32;
        for channel in 0..out_ch {
            for sample in instance.output_buffer(channel)[..block].iter() {
                peak = peak.max(sample.abs());
            }
        }
        last_peak = peak;
        if block_index < 4 || block_index % 16 == 0 {
            eprintln!("probe: block {block_index} out_peak={peak}");
        }
    }
    if std::env::var_os("VIBEMIX_PROBE_SIDECHAIN").is_some() && instance.has_sidechain() {
        let sidechain = vec![1.0f32; block];
        let mut peak = 0.0f32;
        for _ in 0..8 {
            for channel in 0..in_ch {
                instance.input_buffer_mut(channel)[..block].fill(0.0);
            }
            instance.write_sidechain(&sidechain, &sidechain, block);
            instance.run(block);
            for channel in 0..out_ch {
                for sample in instance.output_buffer(channel)[..block].iter() {
                    peak = peak.max(sample.abs());
                }
            }
        }
        eprintln!("probe: sidechain-driven out_peak={peak}");
    }

    let (_, _, _, last) = instance.status();
    eprintln!("probe: final out_peak={last_peak} last_process={last}");
    if std::env::var_os("VIBEMIX_PROBE_VIEW").is_some() {
        match instance.instance_handle().open_view("vibemix probe") {
            Some(view) => {
                for _ in 0..10 {
                    view.pump();
                }
                std::thread::sleep(std::time::Duration::from_millis(300));
                view.pump();
                eprintln!("probe: view opened, wants_close={}", view.wants_close());
            }
            None => eprintln!("probe: view open failed"),
        }
    }
    std::process::exit(0);
}

fn run_smoke_ui(uri: &str) -> ! {
    use std::ffi::c_void;
    use std::time::Duration;

    smoke::disable_core_dumps();
    smoke::lower_priority();

    extern "C" fn no_write(_: *mut c_void, _: u32, _: u32, _: u32, _: *const c_void) {}

    let host = Host::new();
    let Some(mut instance) = host.instantiate(uri, 48000.0, 512, 8192) else {
        std::process::exit(2);
    };
    let Some(candidate) = find_ui(uri) else {
        std::process::exit(2);
    };
    let handle = instance.handle();
    let Some(window) = (unsafe {
        host.open_ui(
            uri,
            &candidate,
            handle,
            no_write,
            std::ptr::null_mut(),
            true,
            uri,
        )
    }) else {
        std::process::exit(2);
    };

    // Exercise the same output-forwarding path the engine uses: run a block,
    // publish the plugin's outputs, and deliver them to the UI.
    let outputs = instance.outputs();
    outputs.set_active(true);
    for _ in 0..75 {
        instance.run(512);
        for index in 0..outputs.control_port_count() {
            let value = outputs.control_value(index);
            window.port_event(
                outputs.control_port(index),
                std::mem::size_of::<f32>() as u32,
                0,
                &value.to_ne_bytes(),
            );
        }
        for index in 0..outputs.atom_port_count() {
            if let Some((bytes, format)) = outputs.read_atom(index) {
                window.port_event(outputs.atom_port(index), bytes.len() as u32, format, &bytes);
            }
        }
        window.pump();
        std::thread::sleep(Duration::from_millis(20));
    }
    outputs.set_active(false);
    drop(window);
    drop(instance);
    std::process::exit(0);
}

fn load_or_default(args: &Args) -> Result<MixerModel> {
    match &args.session {
        Some(path) => {
            let text =
                std::fs::read_to_string(path).with_context(|| format!("read session {path}"))?;
            let model = toml::from_str(&text).with_context(|| format!("parse session {path}"))?;
            Ok(model)
        }
        None => Ok(MixerModel::default_minimal()),
    }
}
