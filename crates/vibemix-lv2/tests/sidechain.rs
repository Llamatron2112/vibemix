//! LV2 sidechain classification and feeding. Uses an installed sidechain
//! plugin when present (Calf or LSP) and skips otherwise, so the suite still
//! passes on systems without one.

use vibemix_lv2::Host;

fn find_sidechain_plugin(host: &Host) -> Option<String> {
    host.discover()
        .into_iter()
        .find(|info| {
            info.main_input_count == 2
                && info.audio_outputs.len() == 2
                && info.audio_inputs.len() > 2
                && (info.uri.contains("Sidechain") || info.uri.contains("sc_"))
        })
        .map(|info| info.uri)
}

#[test]
fn classifies_and_feeds_a_sidechain_input() {
    let host = Host::new();
    let Some(uri) = find_sidechain_plugin(&host) else {
        eprintln!("no sidechain LV2 plugin installed; skipping");
        return;
    };

    let mut instance = host
        .instantiate(&uri, 48_000.0, 512, 8192)
        .expect("instantiate sidechain plugin");
    assert_eq!(instance.main_input_count(), 2, "{uri}");
    assert!(instance.audio_input_count() > 2, "{uri}");
    assert!(instance.has_sidechain(), "{uri}");

    // The main inputs come first; `write_sidechain` targets the rest.
    let offset = instance.main_input_count();
    let sidechain = [0.7f32; 8];
    instance.write_sidechain(&sidechain, &sidechain);
    assert_eq!(instance.input_buffer_mut(offset)[0], 0.7);
    assert_eq!(instance.input_buffer_mut(offset + 1)[0], 0.7);
}
