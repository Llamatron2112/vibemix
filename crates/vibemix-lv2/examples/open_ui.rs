use std::ffi::c_void;

use vibemix_lv2::{find_ui, Host};

extern "C" fn write(
    _user: *mut c_void,
    _port_index: u32,
    _size: u32,
    _format: u32,
    _buffer: *const c_void,
) {
}

fn main() {
    let uri = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "http://gareus.org/oss/lv2/darc#mono".to_string());

    let host = Host::new();
    let Some(candidate) = find_ui(&uri) else {
        eprintln!("no X11 UI for {uri}");
        std::process::exit(1);
    };
    println!("ui: {} ({})", candidate.ui_uri, candidate.binary);

    let Some(instance) = host.instantiate(&uri, 48000.0, 512, 8192) else {
        eprintln!("instantiate failed");
        std::process::exit(1);
    };
    let handle = instance.handle();

    let Some(window) = (unsafe {
        host.open_ui(&uri, &candidate, handle, write, std::ptr::null_mut(), false)
    }) else {
        eprintln!("open_ui failed");
        std::process::exit(1);
    };
    println!("window open; pumping for 3s");

    for _ in 0..150 {
        window.pump();
        std::thread::sleep(std::time::Duration::from_millis(20));
    }

    drop(window);
    drop(instance);
    println!("closed cleanly");
}
