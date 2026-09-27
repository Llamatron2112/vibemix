mod backend_jack;
mod control;
mod dsp;
mod params;

use std::sync::Arc;

use anyhow::{bail, Context, Result};
use clap::Parser;
use vibemix_core::model::MixerModel;
use vibemix_lv2::{find_ui, Host};

use crate::backend_jack::JackEngine;
use crate::params::{SharedMeters, SharedParams};

#[derive(Debug, Parser)]
#[command(name = "vibemix-engine", about = "vibemix audio mixer engine")]
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
}

fn main() -> Result<()> {
    let args = Args::parse();

    if let Some(uri) = args.smoke.as_deref() {
        run_smoke(uri);
    }
    if let Some(uri) = args.smoke_ui.as_deref() {
        run_smoke_ui(uri);
    }

    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
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
    let engine = JackEngine::start(
        &args.client_name,
        Arc::clone(&params),
        Arc::clone(&meters),
        args.auto_connect,
        &host,
        &mut model,
    )
    .context("start jack engine")?;

    tracing::info!(
        sample_rate = engine.sample_rate(),
        buffer_size = engine.buffer_size(),
        "engine running"
    );

    let mut controller = control::Controller::new(args.osc_port, params, meters, model, engine, host)
        .with_context(|| format!("bind OSC port {}", args.osc_port))?;

    tracing::info!(
        session = %args.session.as_deref().unwrap_or("default"),
        "control ready; press Ctrl-C to quit"
    );

    controller.run();
    Ok(())
}

fn run_smoke(uri: &str) -> ! {
    let host = Host::new();
    let sample_rate = 48000.0;
    let frames = 512usize;
    let Some(mut instance) = host.instantiate(uri, sample_rate, frames, 8192) else {
        std::process::exit(2);
    };

    for _ in 0..64 {
        for channel in 0..instance.audio_input_count() {
            for sample in instance.input_buffer_mut(channel)[..frames].iter_mut() {
                *sample = 0.1;
            }
        }
        instance.run(frames);
        instance.deliver_responses();
    }
    std::process::exit(0);
}

fn run_smoke_ui(uri: &str) -> ! {
    use std::ffi::c_void;
    use std::time::Duration;

    extern "C" fn no_write(_: *mut c_void, _: u32, _: u32, _: u32, _: *const c_void) {}

    let host = Host::new();
    let Some(instance) = host.instantiate(uri, 48000.0, 512, 8192) else {
        std::process::exit(2);
    };
    let Some(candidate) = find_ui(uri) else {
        std::process::exit(2);
    };
    let handle = instance.handle();
    let Some(window) = (unsafe {
        host.open_ui(uri, &candidate, handle, no_write, std::ptr::null_mut(), true)
    }) else {
        std::process::exit(2);
    };

    for _ in 0..75 {
        window.pump();
        std::thread::sleep(Duration::from_millis(20));
    }
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
