//! The control socket: a Unix datagram socket inside a private directory.
//!
//! Access control is the filesystem: the socket lives in a directory owned by
//! the current user with mode `0700`, so no other user (or sandboxed process
//! without access to that directory) can reach the engine. This replaces the
//! old localhost UDP port, which every local process could talk to.
//!
//! Both ends of a datagram socket must be bound to a path to receive replies,
//! so the GUI and tools bind their own socket next to the engine's.

use std::ffi::OsStr;
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::os::unix::net::{SocketAddr, UnixDatagram};
use std::path::{Path, PathBuf};

/// Longest socket path `sockaddr_un` can hold (Linux: 108 bytes including NUL).
const MAX_PATH: usize = 107;

/// Environment variable overriding the engine's socket path.
pub const SOCKET_ENV: &str = "VIBEMIX_SOCKET";

fn effective_uid() -> u32 {
    // SAFETY: `geteuid` has no preconditions and cannot fail.
    unsafe { libc::geteuid() }
}

/// Creates (if needed) and verifies a private directory: a real directory (not
/// a symlink) owned by us that nobody else can enter.
pub fn ensure_private_dir(dir: &Path) -> io::Result<()> {
    if !dir.exists() {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)?;
    }
    let meta = std::fs::symlink_metadata(dir)?;
    if !meta.is_dir() {
        return Err(io::Error::other(format!(
            "{} is not a directory",
            dir.display()
        )));
    }
    if meta.uid() != effective_uid() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("{} is not owned by the current user", dir.display()),
        ));
    }
    if meta.mode() & 0o077 != 0 {
        // Ours but too open (for example created by an older tool): tighten it.
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

/// `$XDG_RUNTIME_DIR/vibemix`, or `/tmp/vibemix-<uid>` without a runtime dir.
pub fn runtime_dir() -> PathBuf {
    match std::env::var_os("XDG_RUNTIME_DIR") {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir).join("vibemix"),
        _ => PathBuf::from(format!("/tmp/vibemix-{}", effective_uid())),
    }
}

/// The engine's socket: `$VIBEMIX_SOCKET`, else `engine.sock` in [`runtime_dir`].
pub fn default_engine_socket() -> PathBuf {
    match std::env::var_os(SOCKET_ENV) {
        Some(path) if !path.is_empty() => PathBuf::from(path),
        _ => runtime_dir().join("engine.sock"),
    }
}

fn check_path(path: &Path) -> io::Result<()> {
    if path.as_os_str().len() > MAX_PATH {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "socket path is too long ({} bytes, limit {MAX_PATH})",
                path.as_os_str().len()
            ),
        ));
    }
    Ok(())
}

/// Whether a socket is bound at `path` (something is listening there).
pub fn is_alive(path: &Path) -> bool {
    match UnixDatagram::unbound() {
        Ok(probe) => probe.connect(path).is_ok(),
        Err(_) => false,
    }
}

/// Binds the engine's control socket at `path`.
///
/// Fails with [`io::ErrorKind::AddrInUse`] when another engine already owns the
/// path, and replaces a stale socket file left behind by a crashed one.
pub fn bind_engine(path: &Path) -> io::Result<UnixDatagram> {
    check_path(path)?;
    if let Some(parent) = path.parent() {
        ensure_private_dir(parent)?;
    }
    if path.exists() {
        if is_alive(path) {
            return Err(io::Error::new(
                io::ErrorKind::AddrInUse,
                format!(
                    "another vibemix engine is already running on {}",
                    path.display()
                ),
            ));
        }
        std::fs::remove_file(path)?;
    }
    let socket = UnixDatagram::bind(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    enlarge_buffers(&socket, RECV_BUFFER_BYTES);
    Ok(socket)
}

/// Binds a client socket (the GUI, a tool) next to the engine's, removing
/// leftovers of dead clients with the same prefix. Returns the socket and its
/// path (the caller removes it on exit).
pub fn bind_client(directory: &Path, prefix: &str) -> io::Result<(UnixDatagram, PathBuf)> {
    ensure_private_dir(directory)?;
    // Sockets of clients whose process is gone: `<prefix>-<pid>-<n>.sock`.
    if let Ok(entries) = std::fs::read_dir(directory) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            let Some(rest) = name.strip_prefix(prefix).and_then(|r| r.strip_prefix('-')) else {
                continue;
            };
            let Some(pid) = rest.split('-').next().and_then(|p| p.parse::<u32>().ok()) else {
                continue;
            };
            if !Path::new(&format!("/proc/{pid}")).exists() {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
    let pid = std::process::id();
    for attempt in 0..1000u32 {
        let path = directory.join(format!("{prefix}-{pid}-{attempt}.sock"));
        check_path(&path)?;
        if path.exists() {
            continue;
        }
        let socket = UnixDatagram::bind(&path)?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
        enlarge_buffers(&socket, RECV_BUFFER_BYTES);
        return Ok((socket, path));
    }
    Err(io::Error::new(
        io::ErrorKind::AddrInUse,
        "no free client socket name",
    ))
}

static REMOVE_AT_EXIT: std::sync::Mutex<Vec<PathBuf>> = std::sync::Mutex::new(Vec::new());

extern "C" fn remove_registered_paths() {
    if let Ok(paths) = REMOVE_AT_EXIT.lock() {
        for path in paths.iter() {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// Removes `path` when the process exits normally (a client's socket file).
/// Leftovers of a crash are cleaned up by the next [`bind_client`].
pub fn remove_at_exit(path: PathBuf) {
    if let Ok(mut paths) = REMOVE_AT_EXIT.lock() {
        if paths.is_empty() {
            // SAFETY: `remove_registered_paths` is a plain `extern "C" fn`
            // that only touches a static mutex and the filesystem.
            unsafe { libc::atexit(remove_registered_paths) };
        }
        paths.push(path);
    }
}

/// The filesystem path of a datagram's sender, if it bound one. Unnamed
/// senders cannot be answered and are ignored by the engine.
pub fn peer_path(address: &SocketAddr) -> Option<PathBuf> {
    address.as_pathname().map(Path::to_path_buf)
}

/// Sends `data` to the socket at `to` without ever blocking.
///
/// A blocking `send` on a Unix datagram socket waits when the receiver's queue
/// is full (unlike UDP, which drops), so a stalled GUI could stall the engine's
/// control loop. Here a full queue returns [`io::ErrorKind::WouldBlock`] and the
/// caller drops the datagram; a vanished peer returns `NotFound` or
/// `ConnectionRefused`.
pub fn send_nonblocking(socket: &UnixDatagram, data: &[u8], to: &Path) -> io::Result<()> {
    check_path(to)?;
    // SAFETY: `sockaddr_un` is plain data for which all-zero is valid.
    let mut address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    address.sun_family = libc::AF_UNIX as libc::sa_family_t;
    let bytes = OsStr::new(to).as_bytes();
    for (slot, byte) in address.sun_path.iter_mut().zip(bytes) {
        *slot = *byte as libc::c_char;
    }
    let length = std::mem::size_of::<libc::sa_family_t>() + bytes.len() + 1;
    // SAFETY: `data` is valid for `data.len()` bytes, `address` is a fully
    // initialised sockaddr_un whose path is NUL-terminated within the struct
    // (`check_path` bounds it), and `length` covers exactly those bytes.
    let sent = unsafe {
        libc::sendto(
            socket.as_raw_fd(),
            data.as_ptr().cast(),
            data.len(),
            libc::MSG_DONTWAIT | libc::MSG_NOSIGNAL,
            (&address as *const libc::sockaddr_un).cast(),
            length as libc::socklen_t,
        )
    };
    if sent < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// Asks the kernel for larger socket buffers. Chunked messages (the plugin
/// catalog, big state snapshots) arrive as a burst of ~32 KiB datagrams, which
/// overflows the default buffer if the reader lags even briefly. Best effort:
/// the kernel may clamp the size.
pub fn enlarge_buffers(socket: &impl AsRawFd, bytes: usize) {
    let value = bytes.min(i32::MAX as usize) as libc::c_int;
    for option in [libc::SO_RCVBUF, libc::SO_SNDBUF] {
        // SAFETY: `value` outlives the call and the length matches its type.
        unsafe {
            libc::setsockopt(
                socket.as_raw_fd(),
                libc::SOL_SOCKET,
                option,
                (&value as *const libc::c_int).cast(),
                std::mem::size_of::<libc::c_int>() as libc::socklen_t,
            );
        }
    }
}

/// Socket buffer size requested by the engine and the GUI.
pub const RECV_BUFFER_BYTES: usize = 4 * 1024 * 1024;

/// Whether `error` means the peer is gone for good (as opposed to merely busy).
pub fn peer_is_gone(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn temp_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("vibemix-socket-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn engine_and_client_exchange_datagrams() {
        let dir = temp_dir("exchange");
        let engine_path = dir.join("engine.sock");
        let engine = bind_engine(&engine_path).unwrap();
        engine
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let (client, client_path) = bind_client(&dir, "test").unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();

        send_nonblocking(&client, b"ping", &engine_path).unwrap();
        let mut buffer = [0u8; 64];
        let (length, from) = engine.recv_from(&mut buffer).unwrap();
        assert_eq!(&buffer[..length], b"ping");
        let reply_to = peer_path(&from).expect("the client is bound");
        assert_eq!(reply_to, client_path);

        send_nonblocking(&engine, b"pong", &reply_to).unwrap();
        let (length, _) = client.recv_from(&mut buffer).unwrap();
        assert_eq!(&buffer[..length], b"pong");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_second_engine_is_refused_and_a_stale_socket_is_replaced() {
        let dir = temp_dir("owner");
        let path = dir.join("engine.sock");
        let first = bind_engine(&path).unwrap();
        let second = bind_engine(&path).unwrap_err();
        assert_eq!(second.kind(), io::ErrorKind::AddrInUse);
        // A crashed engine leaves the file behind; the next one takes over.
        drop(first);
        assert!(path.exists());
        assert!(!is_alive(&path));
        bind_engine(&path).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_socket_and_its_directory_are_private() {
        let dir = temp_dir("private");
        let path = dir.join("engine.sock");
        let _engine = bind_engine(&path).unwrap();
        let dir_mode = std::fs::metadata(&dir).unwrap().mode() & 0o777;
        let socket_mode = std::fs::metadata(&path).unwrap().mode() & 0o777;
        assert_eq!(dir_mode, 0o700);
        assert_eq!(socket_mode, 0o600);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn an_open_directory_is_tightened_and_a_foreign_one_refused() {
        let dir = temp_dir("open");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o777)).unwrap();
        ensure_private_dir(&dir).unwrap();
        assert_eq!(std::fs::metadata(&dir).unwrap().mode() & 0o777, 0o700);
        std::fs::remove_dir_all(&dir).unwrap();
        // `/` is owned by root: never ours to use as a control directory.
        if effective_uid() != 0 {
            assert!(ensure_private_dir(Path::new("/")).is_err());
        }
    }

    #[test]
    fn a_full_receive_queue_drops_instead_of_blocking() {
        let dir = temp_dir("full");
        let path = dir.join("slow.sock");
        std::fs::create_dir_all(&dir).unwrap();
        ensure_private_dir(&dir).unwrap();
        let slow = UnixDatagram::bind(&path).unwrap(); // never read
        let sender = UnixDatagram::unbound().unwrap();
        let payload = vec![0u8; 60_000];
        let mut dropped = false;
        for _ in 0..100_000 {
            match send_nonblocking(&sender, &payload, &path) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    dropped = true;
                    break;
                }
                Err(error) => panic!("unexpected error: {error}"),
            }
        }
        assert!(
            dropped,
            "the queue never filled, so the send never reported it"
        );
        drop(slow);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn sending_to_a_vanished_peer_reports_it() {
        let dir = temp_dir("gone");
        std::fs::create_dir_all(&dir).unwrap();
        ensure_private_dir(&dir).unwrap();
        let path = dir.join("gone.sock");
        let sender = UnixDatagram::unbound().unwrap();
        let error = send_nonblocking(&sender, b"x", &path).unwrap_err();
        assert!(peer_is_gone(&error), "{error}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn over_long_paths_are_rejected() {
        let long = PathBuf::from(format!("/tmp/{}", "x".repeat(200)));
        assert!(bind_engine(&long).is_err());
    }
}
