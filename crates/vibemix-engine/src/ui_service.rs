//! Engine-side client for the out-of-process LV2 UI host.
//!
//! Owns one UI-host process (spawned lazily on the first out-of-process editor
//! open) and the framed socket to it. All calls run on the control thread; a
//! dead host is detected and its editors are dropped without affecting audio.

use std::io::{Read, Write};
use std::os::unix::io::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use vibemix_lv2::ui_ipc::{self, FrameBuffer, UiMessage};
use vibemix_lv2::UiCandidate;

/// How long to wait for the host to connect after spawning it.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// How long to wait for `Opened`/`OpenFailed` after sending `Open`.
const OPEN_TIMEOUT: Duration = Duration::from_secs(10);

pub struct UiService {
    path: PathBuf,
    stream: Option<UnixStream>,
    child: Option<Child>,
    frames: FrameBuffer,
    /// Messages received while waiting for a specific reply (e.g. `Opened`).
    pending: Vec<UiMessage>,
    next_id: u32,
}

impl UiService {
    pub fn new() -> Self {
        let dir = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".to_string());
        let path = PathBuf::from(format!("{dir}/vibemix-ui-{}.sock", std::process::id()));
        Self {
            path,
            stream: None,
            child: None,
            frames: FrameBuffer::new(),
            pending: Vec::new(),
            next_id: 1,
        }
    }

    pub fn alloc_id(&mut self) -> u32 {
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1).max(1);
        id
    }

    /// Spawns the host and connects to it if it is not already running.
    pub fn ensure(&mut self) -> std::io::Result<()> {
        if self.is_alive() {
            return Ok(());
        }
        self.stop();
        let _ = std::fs::remove_file(&self.path);
        let listener = UnixListener::bind(&self.path)?;
        listener.set_nonblocking(true)?;

        let executable = std::env::current_exe()?;
        let mut command = Command::new(executable);
        command
            .arg("--ui-host")
            .arg(&self.path)
            .stdin(Stdio::null());
        if crate::smoke::debug_enabled() {
            command.stdout(Stdio::inherit()).stderr(Stdio::inherit());
        } else {
            command.stdout(Stdio::null()).stderr(Stdio::null());
        }
        // The host picks the same GTK major version the engine chose, without
        // rescanning the plugin world in its own process.
        match vibemix_lv2::ui::gtk_version() {
            2 => {
                command.env("VIBEMIX_GTK", "gtk2");
            }
            3 => {
                command.env("VIBEMIX_GTK", "gtk3");
            }
            _ => {
                command.env("VIBEMIX_GTK", "none");
            }
        }
        let child = command.spawn()?;

        let deadline = Instant::now() + CONNECT_TIMEOUT;
        let stream = loop {
            match listener.accept() {
                Ok((stream, _)) => break Some(stream),
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    if Instant::now() >= deadline {
                        break None;
                    }
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(error) => {
                    let _ = child;
                    return Err(error);
                }
            }
        };
        let Some(stream) = stream else {
            let mut child = child;
            let _ = child.kill();
            let _ = child.wait();
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "UI host did not connect",
            ));
        };
        stream.set_nonblocking(true)?;
        self.stream = Some(stream);
        self.child = Some(child);
        Ok(())
    }

    pub fn is_alive(&mut self) -> bool {
        if self.stream.is_none() {
            return false;
        }
        if let Some(child) = self.child.as_mut() {
            match child.try_wait() {
                Ok(Some(_)) | Err(_) => return false,
                Ok(None) => {}
            }
        }
        true
    }

    /// Builds an editor window on the host, waiting for the result.
    pub fn open(
        &mut self,
        id: u32,
        candidate: &UiCandidate,
        plugin_uri: &str,
        title: &str,
        scale: f32,
        update_rate: f32,
    ) -> bool {
        if self.ensure().is_err() {
            return false;
        }
        let message = UiMessage::Open {
            id,
            plugin_uri: plugin_uri.to_string(),
            ui_uri: candidate.ui_uri.clone(),
            ui_type: candidate.kind.ui_type().to_string(),
            bundle: candidate.bundle.clone(),
            binary: candidate.binary.clone(),
            title: title.to_string(),
            scale,
            update_rate,
        };
        if !self.send(&message) {
            return false;
        }
        let deadline = Instant::now() + OPEN_TIMEOUT;
        while Instant::now() < deadline {
            for message in self.recv() {
                match message {
                    UiMessage::Opened { id: got } if got == id => return true,
                    UiMessage::OpenFailed { id: got, reason } if got == id => {
                        tracing::warn!(id, %reason, "UI host failed to open editor");
                        return false;
                    }
                    other => self.pending.push(other),
                }
            }
            if !self.is_alive() {
                return false;
            }
            if let Some(stream) = &self.stream {
                poll_readable(stream.as_raw_fd(), 20);
            }
        }
        tracing::warn!(id, "UI host timed out opening editor");
        false
    }

    pub fn send_port_event(
        &mut self,
        id: u32,
        port: u32,
        format: u32,
        type_uri: Option<String>,
        bytes: Vec<u8>,
    ) {
        self.send(&UiMessage::PortEvent {
            id,
            port,
            format,
            type_uri,
            bytes,
        });
    }

    pub fn close(&mut self, id: u32) {
        self.send(&UiMessage::Close { id });
    }

    /// Returns every message the host has sent since the last call.
    pub fn recv(&mut self) -> Vec<UiMessage> {
        let mut messages = std::mem::take(&mut self.pending);
        let Some(stream) = self.stream.as_ref() else {
            return messages;
        };
        let fd = stream.as_raw_fd();
        if poll_readable(fd, 0) {
            let mut buffer = [0u8; 65536];
            loop {
                match (&mut self.stream.as_ref().unwrap()).read(&mut buffer) {
                    Ok(0) => {
                        // Host closed: drop the connection and report nothing.
                        self.stream = None;
                        break;
                    }
                    Ok(count) => self.frames.push(&buffer[..count]),
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                    Err(_) => {
                        self.stream = None;
                        break;
                    }
                }
            }
        }
        while let Some(message) = self.frames.pop() {
            messages.push(message);
        }
        messages
    }

    fn send(&mut self, message: &UiMessage) -> bool {
        let Some(stream) = self.stream.as_mut() else {
            return false;
        };
        let data = ui_ipc::frame(message);
        let fd = stream.as_raw_fd();
        let mut offset = 0;
        let deadline = Instant::now() + Duration::from_millis(200);
        while offset < data.len() {
            match stream.write(&data[offset..]) {
                Ok(0) => return false,
                Ok(count) => offset += count,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    if Instant::now() >= deadline || !poll_writable(fd, 20) {
                        tracing::warn!("UI host not draining; dropping message");
                        return false;
                    }
                }
                Err(_) => {
                    self.stream = None;
                    return false;
                }
            }
        }
        true
    }

    fn stop(&mut self) {
        if self.stream.is_some() {
            self.send(&UiMessage::Shutdown);
        }
        self.stream = None;
        self.frames = FrameBuffer::new();
        if let Some(mut child) = self.child.take() {
            let deadline = Instant::now() + Duration::from_millis(500);
            while Instant::now() < deadline {
                if let Ok(Some(_)) = child.try_wait() {
                    return;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

impl Drop for UiService {
    fn drop(&mut self) {
        self.stop();
        let _ = std::fs::remove_file(&self.path);
    }
}

fn poll_fd(fd: std::os::unix::io::RawFd, events: libc::c_short, timeout_ms: i32) -> bool {
    let mut descriptor = libc::pollfd {
        fd,
        events,
        revents: 0,
    };
    let ready = unsafe { libc::poll(&mut descriptor, 1, timeout_ms) };
    ready > 0 && (descriptor.revents & events) != 0
}

fn poll_readable(fd: std::os::unix::io::RawFd, timeout_ms: i32) -> bool {
    poll_fd(fd, libc::POLLIN, timeout_ms)
}

fn poll_writable(fd: std::os::unix::io::RawFd, timeout_ms: i32) -> bool {
    poll_fd(fd, libc::POLLOUT, timeout_ms)
}
