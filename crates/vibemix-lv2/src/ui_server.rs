//! Out-of-process LV2 UI host.
//!
//! The engine spawns `vibemix-engine --ui-host <socket>`; this connects back to
//! the engine's Unix socket and hosts plugin editors here, in a process that is
//! not running the PipeWire data-loop. Building a toolkit UI here therefore
//! cannot stall audio.
//!
//! Run on a single thread (the "main" thread for GTK): messages are drained,
//! then every open window is pumped.

use std::collections::HashMap;
use std::io::Read;
use std::os::raw::c_void;
use std::os::unix::io::{AsRawFd, RawFd};
use std::os::unix::net::UnixStream;

use crate::ui::{self, UiCandidate, UiKind, UiWindow};
use crate::ui_ipc::{self, FrameBuffer, UiMessage};
use crate::urid::UridMap;

struct HostedUi {
    id: u32,
    fd: RawFd,
    window: Option<UiWindow>,
}

fn poll_fd(fd: RawFd, events: libc::c_short, timeout_ms: i32) -> bool {
    let mut descriptor = libc::pollfd {
        fd,
        events,
        revents: 0,
    };
    let ready = unsafe { libc::poll(&mut descriptor, 1, timeout_ms) };
    ready > 0 && (descriptor.revents & events) != 0
}

/// Writes a framed message, waiting briefly for the socket to drain.
fn send(fd: RawFd, message: &UiMessage) -> std::io::Result<()> {
    let data = ui_ipc::frame(message);
    let mut offset = 0;
    while offset < data.len() {
        let written = unsafe {
            libc::write(
                fd,
                data[offset..].as_ptr() as *const c_void,
                data.len() - offset,
            )
        };
        if written < 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::WouldBlock {
                poll_fd(fd, libc::POLLOUT, 100);
                continue;
            }
            return Err(error);
        }
        offset += written as usize;
    }
    Ok(())
}

extern "C" fn host_write(
    user: *mut c_void,
    port: u32,
    size: u32,
    format: u32,
    buffer: *const c_void,
) {
    if user.is_null() {
        return;
    }
    let ui = unsafe { &*(user as *const HostedUi) };
    let bytes = if buffer.is_null() || size == 0 {
        Vec::new()
    } else {
        unsafe { std::slice::from_raw_parts(buffer as *const u8, size as usize).to_vec() }
    };
    let _ = send(
        ui.fd,
        &UiMessage::Write {
            id: ui.id,
            port,
            format,
            bytes,
        },
    );
}

#[allow(clippy::too_many_arguments)]
fn open_ui(
    id: u32,
    plugin_uri: String,
    ui_uri: String,
    ui_type: String,
    bundle: String,
    binary: String,
    title: String,
    scale: f32,
    update_rate: f32,
    fd: RawFd,
    uis: &mut HashMap<u32, Box<HostedUi>>,
) {
    let _ = (scale, update_rate);
    let Some(kind) = UiKind::from_ui_type(&ui_type) else {
        let _ = send(
            fd,
            &UiMessage::OpenFailed {
                id,
                reason: format!("unknown UI type {ui_type}"),
            },
        );
        return;
    };
    let candidate = UiCandidate {
        ui_uri,
        bundle,
        binary,
        kind,
        requires_instance_access: false,
    };
    // The write callback needs a stable address before `open` returns, so the
    // window is filled in after instantiation.
    let mut hosted = Box::new(HostedUi {
        id,
        fd,
        window: None,
    });
    let user = &mut *hosted as *mut HostedUi as *mut c_void;
    let window = unsafe {
        ui::open(
            UridMap::shared(),
            &plugin_uri,
            &candidate,
            std::ptr::null_mut(),
            host_write,
            user,
            false,
            &title,
        )
    };
    match window {
        Some(window) => {
            hosted.window = Some(window);
            uis.insert(id, hosted);
            let _ = send(fd, &UiMessage::Opened { id });
        }
        None => {
            let _ = send(
                fd,
                &UiMessage::OpenFailed {
                    id,
                    reason: "UI instantiation failed".to_string(),
                },
            );
        }
    }
}

fn handle(message: UiMessage, fd: RawFd, uis: &mut HashMap<u32, Box<HostedUi>>, running: &mut bool) {
    match message {
        UiMessage::Open {
            id,
            plugin_uri,
            ui_uri,
            ui_type,
            bundle,
            binary,
            title,
            scale,
            update_rate,
        } => {
            uis.remove(&id);
            open_ui(
                id,
                plugin_uri,
                ui_uri,
                ui_type,
                bundle,
                binary,
                title,
                scale,
                update_rate,
                fd,
                uis,
            );
        }
        UiMessage::PortEvent {
            id,
            port,
            format,
            type_uri,
            mut bytes,
        } => {
            let Some(hosted) = uis.get(&id) else {
                return;
            };
            let mut format = format;
            if let Some(uri) = &type_uri {
                // The engine's URIDs are process-local; remap the atom type into
                // this process and rewrite the payload header to match.
                let urid = UridMap::shared().map(uri);
                ui_ipc::rewrite_atom_type(&mut bytes, urid);
                format = urid;
            }
            if let Some(window) = &hosted.window {
                window.port_event(port, bytes.len() as u32, format, &bytes);
            }
        }
        UiMessage::Close { id } => {
            uis.remove(&id);
        }
        UiMessage::Shutdown => *running = false,
        UiMessage::Opened { .. }
        | UiMessage::OpenFailed { .. }
        | UiMessage::Write { .. }
        | UiMessage::CloseRequest { .. }
        | UiMessage::Title { .. } => {}
    }
}

/// Connects to `path` and serves editor windows until the engine shuts down.
pub fn run_ui_host(path: &str) -> std::io::Result<()> {
    ui::init();
    let stream = UnixStream::connect(path)?;
    stream.set_nonblocking(true)?;
    let fd = stream.as_raw_fd();
    let mut frames = FrameBuffer::new();
    let mut uis: HashMap<u32, Box<HostedUi>> = HashMap::new();
    let mut running = true;
    let mut read_buf = [0u8; 65536];

    while running {
        if poll_fd(fd, libc::POLLIN, 16) {
            loop {
                match (&stream).read(&mut read_buf) {
                    Ok(0) => {
                        running = false;
                        break;
                    }
                    Ok(count) => frames.push(&read_buf[..count]),
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                    Err(_) => {
                        running = false;
                        break;
                    }
                }
            }
        }
        while let Some(message) = frames.pop() {
            handle(message, fd, &mut uis, &mut running);
        }

        for hosted in uis.values_mut() {
            if let Some(window) = &hosted.window {
                window.pump();
            }
        }

        let closed: Vec<u32> = uis
            .iter()
            .filter(|(_, hosted)| {
                hosted
                    .window
                    .as_ref()
                    .map(|window| window.wants_close())
                    .unwrap_or(true)
            })
            .map(|(id, _)| *id)
            .collect();
        for id in closed {
            uis.remove(&id);
            let _ = send(fd, &UiMessage::CloseRequest { id });
        }
    }

    Ok(())
}
