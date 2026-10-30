//! Out-of-process LV2 UI host.
//!
//! The engine spawns `vibemix-engine --ui-host <socket>`; this connects back to
//! the engine's Unix socket and hosts plugin editors here, in a process that is
//! not running the PipeWire data-loop. Building a toolkit UI here therefore
//! cannot stall audio.
//!
//! The host shares the engine's URID map: it is seeded with the engine's whole
//! table when it connects, and asks the engine to map any URI it has not seen
//! (see `UiMessage::UridRequest`). A UI and its plugin must agree on URIDs to
//! exchange atoms.
//!
//! Run on a single thread (the "main" thread for GTK): messages are drained,
//! then every open window is pumped.

use std::collections::HashMap;
use std::ffi::{CStr, CString};
use std::io::Read;
use std::os::raw::{c_char, c_void};
use std::os::unix::io::{AsRawFd, RawFd};
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

use crate::ui::{self, UiCandidate, UiKind, UiWindow};
use crate::ui_ipc::{self, FrameBuffer, UiMessage};

struct HostedUi {
    id: u32,
    fd: RawFd,
    window: Option<UiWindow>,
}

struct Host {
    fd: RawFd,
    stream: UnixStream,
    frames: FrameBuffer,
    pending: Vec<UiMessage>,
    uis: HashMap<u32, Box<HostedUi>>,
    /// URI -> URID, seeded from the engine and extended on miss.
    urid_by_uri: HashMap<String, u32>,
    /// URID -> URI string kept alive for `unmap`.
    urid_cstr: HashMap<u32, CString>,
    running: bool,
}

impl Host {
    fn read_available(&mut self, read_buf: &mut [u8]) {
        loop {
            match self.stream.read(read_buf) {
                Ok(0) => {
                    self.running = false;
                    break;
                }
                Ok(count) => self.frames.push(&read_buf[..count]),
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(_) => {
                    self.running = false;
                    break;
                }
            }
        }
    }

    fn remember(&mut self, uri: &str, urid: u32) {
        if let Ok(value) = CString::new(uri) {
            self.urid_cstr.insert(urid, value);
        }
        self.urid_by_uri.insert(uri.to_string(), urid);
    }

    /// The engine's URID for `uri`, asking it (and blocking) on a table miss so
    /// the UI and plugin always agree.
    fn map_uri(&mut self, uri: &str) -> u32 {
        if let Some(&urid) = self.urid_by_uri.get(uri) {
            return urid;
        }
        let _ = send(
            self.fd,
            &UiMessage::UridRequest {
                uri: uri.to_string(),
            },
        );
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            while let Some(message) = self.frames.pop() {
                match message {
                    UiMessage::UridReply { uri: got, urid } if got == uri => {
                        self.remember(&got, urid);
                        return urid;
                    }
                    other => self.pending.push(other),
                }
            }
            if Instant::now() >= deadline {
                return 0;
            }
            let mut read_buf = [0u8; 65536];
            self.read_available(&mut read_buf);
            if self.frames.is_empty() {
                poll_fd(self.fd, libc::POLLIN, 20);
            }
        }
    }

    fn unmap_uri(&self, urid: u32) -> *const c_char {
        self.urid_cstr
            .get(&urid)
            .map(|value| value.as_ptr())
            .unwrap_or(std::ptr::null())
    }
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

extern "C" fn proxy_urid_map(user: *mut c_void, uri: *const c_char) -> u32 {
    if user.is_null() || uri.is_null() {
        return 0;
    }
    let host = unsafe { &mut *(user as *mut Host) };
    let uri = unsafe { CStr::from_ptr(uri) }
        .to_string_lossy()
        .into_owned();
    host.map_uri(&uri)
}

extern "C" fn proxy_urid_unmap(user: *mut c_void, urid: u32) -> *const c_char {
    if user.is_null() {
        return std::ptr::null();
    }
    let host = unsafe { &*(user as *const Host) };
    host.unmap_uri(urid)
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
    host: *mut Host,
    id: u32,
    plugin_uri: String,
    ui_uri: String,
    ui_type: String,
    bundle: String,
    binary: String,
    title: String,
) {
    let fd = unsafe { (*host).fd };
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
        ui::open_with_urid(
            host as *mut c_void,
            proxy_urid_map,
            proxy_urid_unmap,
            &plugin_uri,
            &candidate,
            std::ptr::null_mut(),
            host_write,
            user,
            false,
            &title,
        )
    };
    let host = unsafe { &mut *host };
    match window {
        Some(window) => {
            hosted.window = Some(window);
            host.uis.insert(id, hosted);
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

fn handle(host: *mut Host, message: UiMessage) {
    match message {
        UiMessage::Open {
            id,
            plugin_uri,
            ui_uri,
            ui_type,
            bundle,
            binary,
            title,
            ..
        } => {
            open_ui(host, id, plugin_uri, ui_uri, ui_type, bundle, binary, title);
        }
        UiMessage::UridTable { entries } => {
            let host = unsafe { &mut *host };
            for (uri, urid) in entries {
                host.remember(&uri, urid);
            }
        }
        UiMessage::PortEvent {
            id,
            port,
            format,
            bytes,
            ..
        } => {
            let host = unsafe { &mut *host };
            if let Some(hosted) = host.uis.get(&id) {
                if let Some(window) = &hosted.window {
                    window.port_event(port, bytes.len() as u32, format, &bytes);
                }
            }
        }
        UiMessage::Close { id } => {
            let host = unsafe { &mut *host };
            host.uis.remove(&id);
        }
        UiMessage::Shutdown => unsafe { (*host).running = false },
        UiMessage::Opened { .. }
        | UiMessage::OpenFailed { .. }
        | UiMessage::Write { .. }
        | UiMessage::UridRequest { .. }
        | UiMessage::UridReply { .. }
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
    let mut host = Box::new(Host {
        fd,
        stream,
        frames: FrameBuffer::new(),
        pending: Vec::new(),
        uis: HashMap::new(),
        urid_by_uri: HashMap::new(),
        urid_cstr: HashMap::new(),
        running: true,
    });
    let mut read_buf = [0u8; 65536];

    while host.running {
        if poll_fd(fd, libc::POLLIN, 16) {
            host.read_available(&mut read_buf);
        }
        let mut messages = std::mem::take(&mut host.pending);
        while let Some(message) = host.frames.pop() {
            messages.push(message);
        }
        for message in messages {
            handle(&mut *host as *mut Host, message);
        }

        for hosted in host.uis.values_mut() {
            if let Some(window) = &hosted.window {
                window.pump();
            }
        }

        let closed: Vec<u32> = host
            .uis
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
            host.uis.remove(&id);
            let _ = send(fd, &UiMessage::CloseRequest { id });
        }
    }

    Ok(())
}
