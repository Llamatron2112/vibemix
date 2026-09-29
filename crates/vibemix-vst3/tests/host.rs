use std::path::Path;

use vibemix_vst3::{Host, PluginInstance};

#[test]
fn discovers_and_hosts_the_test_effect() {
    let bundle = env!("VIBEMIX_VST3_TEST_BUNDLE");
    let dir = Path::new(bundle).parent().expect("bundle parent");

    let infos = Host::scan_dir(dir.to_str().expect("utf-8 dir"));
    let info = infos
        .iter()
        .find(|info| info.name.contains("Spike"))
        .expect("test effect discovered");
    assert_eq!(info.class_id.len(), 32);

    let host = Host::new();
    let mut instance = host
        .instantiate(
            &info.bundle,
            &info.class_id,
            48_000.0,
            512,
            PluginInstance::max_frames(),
        )
        .expect("instantiate");

    assert_eq!(instance.audio_input_count(), 2);
    assert_eq!(instance.audio_output_count(), 2);
    assert_eq!(instance.control_count(), 1);

    let control = &instance.controls()[0];
    assert_eq!(control.name, "Gain");
    assert_eq!(control.index, 0);
    assert_eq!(control.step_count, 0);
    assert!((control.default - 0.5).abs() < 1e-6);

    for channel in 0..instance.audio_input_count() {
        for sample in instance.input_buffer_mut(channel).iter_mut() {
            *sample = 0.5;
        }
    }
    instance.set_control(0, 0.25);
    instance.run(512);

    let peak = (0..instance.audio_output_count())
        .flat_map(|channel| instance.output_buffer(channel).iter().copied())
        .fold(0.0f32, |acc, sample| acc.max(sample.abs()));
    assert!((peak - 0.125).abs() < 1e-3, "output peak was {peak}");

    let state = instance.save_state();
    assert_eq!(state.len(), 4);
    instance.load_state(&state);

    let handle = instance.instance_handle();
    assert_eq!(handle.save(), state);
    instance.set_control(0, 0.9);
    handle.restore(&state);
}

#[test]
fn blob_round_trips() {
    let bytes: Vec<u8> = (0..=255u8).collect();
    let text = vibemix_vst3::encode_blob(&bytes);
    assert_eq!(vibemix_vst3::decode_blob(&text), bytes);
    assert!(vibemix_vst3::decode_blob("zz").is_empty());
}
