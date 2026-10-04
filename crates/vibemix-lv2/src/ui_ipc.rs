//! Protocol between the engine and the out-of-process LV2 UI host.
//!
//! Framing: each message is a `u32` little-endian payload length followed by
//! that many payload bytes. The payload starts with a one-byte tag. Integers
//! and floats are little-endian; strings are `u32` length + UTF-8 bytes;
//! byte blobs are `u32` length + bytes.
//!
//! The UI host runs in its own process so constructing a toolkit UI cannot
//! stall the engine's in-process PipeWire data-loop. All messages are
//! control-rate; audio never crosses this boundary.

/// Trailing byte length of an `LV2_Atom` header (`size` + `type`).
const ATOM_HEADER: usize = 8;

#[derive(Debug, Clone, PartialEq)]
pub enum UiMessage {
    /// engine -> host: build an editor window for `id`.
    Open {
        id: u32,
        plugin_uri: String,
        ui_uri: String,
        ui_type: String,
        bundle: String,
        binary: String,
        title: String,
        scale: f32,
        update_rate: f32,
    },
    /// host -> engine: the editor was created.
    Opened { id: u32 },
    /// host -> engine: the editor could not be created.
    OpenFailed { id: u32, reason: String },
    /// engine -> host: deliver a port value to the editor. `type_uri` is set
    /// only for atom payloads and is remapped to the host's URID there.
    PortEvent {
        id: u32,
        port: u32,
        format: u32,
        type_uri: Option<String>,
        bytes: Vec<u8>,
    },
    /// host -> engine: the editor changed a control port (parameter).
    Write {
        id: u32,
        port: u32,
        format: u32,
        bytes: Vec<u8>,
    },
    /// engine -> host: destroy the editor window.
    Close { id: u32 },
    /// host -> engine: the user closed the window.
    CloseRequest { id: u32 },
    /// host -> engine: the plugin proposed a window title (`ui:windowTitle`).
    Title { id: u32, title: String },
    /// engine -> host: exit.
    Shutdown,
    /// engine -> host: this host's complete URID table, so the UI process uses
    /// the same numbering as the plugins.
    UridTable { entries: Vec<(String, u32)> },
    /// host -> engine: request the engine's URID for `uri` (table miss).
    UridRequest { uri: String },
    /// engine -> host: the URID for `uri`.
    UridReply { uri: String, urid: u32 },
}

const TAG_OPEN: u8 = 1;
const TAG_OPENED: u8 = 2;
const TAG_OPEN_FAILED: u8 = 3;
const TAG_PORT_EVENT: u8 = 4;
const TAG_WRITE: u8 = 5;
const TAG_CLOSE: u8 = 6;
const TAG_CLOSE_REQUEST: u8 = 7;
const TAG_TITLE: u8 = 8;
const TAG_SHUTDOWN: u8 = 9;
const TAG_URID_TABLE: u8 = 10;
const TAG_URID_REQUEST: u8 = 11;
const TAG_URID_REPLY: u8 = 12;

fn put_u32(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn put_f32(out: &mut Vec<u8>, value: f32) {
    out.extend_from_slice(&value.to_bits().to_le_bytes());
}

fn put_str(out: &mut Vec<u8>, value: &str) {
    put_u32(out, value.len() as u32);
    out.extend_from_slice(value.as_bytes());
}

fn put_bytes(out: &mut Vec<u8>, value: &[u8]) {
    put_u32(out, value.len() as u32);
    out.extend_from_slice(value);
}

struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    fn u8(&mut self) -> Option<u8> {
        let value = *self.data.get(self.pos)?;
        self.pos += 1;
        Some(value)
    }

    fn u32(&mut self) -> Option<u32> {
        let end = self.pos.checked_add(4)?;
        let slice = self.data.get(self.pos..end)?;
        self.pos = end;
        Some(u32::from_le_bytes(slice.try_into().ok()?))
    }

    fn f32(&mut self) -> Option<f32> {
        Some(f32::from_bits(self.u32()?))
    }

    fn string(&mut self) -> Option<String> {
        let len = self.u32()? as usize;
        let end = self.pos.checked_add(len)?;
        let slice = self.data.get(self.pos..end)?;
        self.pos = end;
        Some(String::from_utf8_lossy(slice).into_owned())
    }

    fn bytes(&mut self) -> Option<Vec<u8>> {
        let len = self.u32()? as usize;
        let end = self.pos.checked_add(len)?;
        let slice = self.data.get(self.pos..end)?;
        self.pos = end;
        Some(slice.to_vec())
    }
}

/// Encodes the payload of `message` (without the length prefix).
pub fn encode(message: &UiMessage) -> Vec<u8> {
    let mut out = Vec::new();
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
            out.push(TAG_OPEN);
            put_u32(&mut out, *id);
            put_str(&mut out, plugin_uri);
            put_str(&mut out, ui_uri);
            put_str(&mut out, ui_type);
            put_str(&mut out, bundle);
            put_str(&mut out, binary);
            put_str(&mut out, title);
            put_f32(&mut out, *scale);
            put_f32(&mut out, *update_rate);
        }
        UiMessage::Opened { id } => {
            out.push(TAG_OPENED);
            put_u32(&mut out, *id);
        }
        UiMessage::OpenFailed { id, reason } => {
            out.push(TAG_OPEN_FAILED);
            put_u32(&mut out, *id);
            put_str(&mut out, reason);
        }
        UiMessage::PortEvent {
            id,
            port,
            format,
            type_uri,
            bytes,
        } => {
            out.push(TAG_PORT_EVENT);
            put_u32(&mut out, *id);
            put_u32(&mut out, *port);
            put_u32(&mut out, *format);
            put_u32(&mut out, type_uri.as_ref().map_or(0, |_| 1));
            if let Some(uri) = type_uri {
                put_str(&mut out, uri);
            }
            put_bytes(&mut out, bytes);
        }
        UiMessage::Write {
            id,
            port,
            format,
            bytes,
        } => {
            out.push(TAG_WRITE);
            put_u32(&mut out, *id);
            put_u32(&mut out, *port);
            put_u32(&mut out, *format);
            put_bytes(&mut out, bytes);
        }
        UiMessage::Close { id } => {
            out.push(TAG_CLOSE);
            put_u32(&mut out, *id);
        }
        UiMessage::CloseRequest { id } => {
            out.push(TAG_CLOSE_REQUEST);
            put_u32(&mut out, *id);
        }
        UiMessage::Title { id, title } => {
            out.push(TAG_TITLE);
            put_u32(&mut out, *id);
            put_str(&mut out, title);
        }
        UiMessage::Shutdown => out.push(TAG_SHUTDOWN),
        UiMessage::UridTable { entries } => {
            out.push(TAG_URID_TABLE);
            put_u32(&mut out, entries.len() as u32);
            for (uri, urid) in entries {
                put_str(&mut out, uri);
                put_u32(&mut out, *urid);
            }
        }
        UiMessage::UridRequest { uri } => {
            out.push(TAG_URID_REQUEST);
            put_str(&mut out, uri);
        }
        UiMessage::UridReply { uri, urid } => {
            out.push(TAG_URID_REPLY);
            put_str(&mut out, uri);
            put_u32(&mut out, *urid);
        }
    }
    out
}

/// Decodes one payload. Returns `None` for a truncated or unknown message.
pub fn decode(payload: &[u8]) -> Option<UiMessage> {
    let mut reader = Reader::new(payload);
    match reader.u8()? {
        TAG_OPEN => Some(UiMessage::Open {
            id: reader.u32()?,
            plugin_uri: reader.string()?,
            ui_uri: reader.string()?,
            ui_type: reader.string()?,
            bundle: reader.string()?,
            binary: reader.string()?,
            title: reader.string()?,
            scale: reader.f32()?,
            update_rate: reader.f32()?,
        }),
        TAG_OPENED => Some(UiMessage::Opened { id: reader.u32()? }),
        TAG_OPEN_FAILED => Some(UiMessage::OpenFailed {
            id: reader.u32()?,
            reason: reader.string()?,
        }),
        TAG_PORT_EVENT => {
            let id = reader.u32()?;
            let port = reader.u32()?;
            let format = reader.u32()?;
            let has_type = reader.u32()? != 0;
            let type_uri = if has_type {
                Some(reader.string()?)
            } else {
                None
            };
            Some(UiMessage::PortEvent {
                id,
                port,
                format,
                type_uri,
                bytes: reader.bytes()?,
            })
        }
        TAG_WRITE => Some(UiMessage::Write {
            id: reader.u32()?,
            port: reader.u32()?,
            format: reader.u32()?,
            bytes: reader.bytes()?,
        }),
        TAG_CLOSE => Some(UiMessage::Close { id: reader.u32()? }),
        TAG_CLOSE_REQUEST => Some(UiMessage::CloseRequest { id: reader.u32()? }),
        TAG_TITLE => Some(UiMessage::Title {
            id: reader.u32()?,
            title: reader.string()?,
        }),
        TAG_SHUTDOWN => Some(UiMessage::Shutdown),
        TAG_URID_TABLE => {
            let count = reader.u32()? as usize;
            let mut entries = Vec::with_capacity(count.min(4096));
            for _ in 0..count {
                let uri = reader.string()?;
                let urid = reader.u32()?;
                entries.push((uri, urid));
            }
            Some(UiMessage::UridTable { entries })
        }
        TAG_URID_REQUEST => Some(UiMessage::UridRequest {
            uri: reader.string()?,
        }),
        TAG_URID_REPLY => Some(UiMessage::UridReply {
            uri: reader.string()?,
            urid: reader.u32()?,
        }),
        _ => None,
    }
}

/// Rewrites an atom payload's `LV2_Atom.type` header field to `urid`, in place.
/// Returns `false` if `bytes` is too short to be an atom.
pub fn rewrite_atom_type(bytes: &mut [u8], urid: u32) -> bool {
    if bytes.len() < ATOM_HEADER {
        return false;
    }
    bytes[4..8].copy_from_slice(&urid.to_le_bytes());
    true
}

/// Incrementally reassembles length-prefixed frames from a byte stream.
#[derive(Default)]
pub struct FrameBuffer {
    buf: Vec<u8>,
}

impl FrameBuffer {
    pub fn new() -> Self {
        Self { buf: Vec::new() }
    }

    pub fn push(&mut self, data: &[u8]) {
        self.buf.extend_from_slice(data);
    }

    /// True when no complete frame is buffered (a partial frame counts as
    /// empty, since it cannot be popped yet).
    pub fn is_empty(&self) -> bool {
        if self.buf.len() < 4 {
            return true;
        }
        let len = u32::from_le_bytes(self.buf[0..4].try_into().unwrap()) as usize;
        self.buf.len() < 4 + len
    }

    /// Pops the next complete message, or `None` if the buffer holds only a
    /// partial frame.
    pub fn pop(&mut self) -> Option<UiMessage> {
        if self.buf.len() < 4 {
            return None;
        }
        let len = u32::from_le_bytes(self.buf[0..4].try_into().ok()?) as usize;
        if self.buf.len() < 4 + len {
            return None;
        }
        let message = decode(&self.buf[4..4 + len]);
        self.buf.drain(0..4 + len);
        message
    }
}

/// Encodes `message` with its length prefix.
pub fn frame(message: &UiMessage) -> Vec<u8> {
    let payload = encode(message);
    let mut out = Vec::with_capacity(4 + payload.len());
    put_u32(&mut out, payload.len() as u32);
    out.extend_from_slice(&payload);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip(message: UiMessage) {
        let bytes = encode(&message);
        assert_eq!(decode(&bytes), Some(message));
    }

    #[test]
    fn open_round_trip() {
        round_trip(UiMessage::Open {
            id: 7,
            plugin_uri: "http://example.com/plug".into(),
            ui_uri: "http://example.com/ui".into(),
            ui_type: "http://lv2plug.in/ns/extensions/ui#GtkUI".into(),
            bundle: "/b".into(),
            binary: "/b/ui.so".into(),
            title: "My UI".into(),
            scale: 1.5,
            update_rate: 30.0,
        });
    }

    #[test]
    fn port_event_round_trip_with_and_without_type() {
        round_trip(UiMessage::PortEvent {
            id: 1,
            port: 3,
            format: 0,
            type_uri: None,
            bytes: 0.5f32.to_ne_bytes().to_vec(),
        });
        round_trip(UiMessage::PortEvent {
            id: 1,
            port: 9,
            format: 42,
            type_uri: Some("http://lv2plug.in/ns/ext/atom#Sequence".into()),
            bytes: {
                let mut v = vec![0u8; 12];
                v[0..4].copy_from_slice(&4u32.to_le_bytes());
                v[4..8].copy_from_slice(&42u32.to_le_bytes());
                v
            },
        });
    }

    #[test]
    fn write_close_title_shutdown_round_trip() {
        round_trip(UiMessage::Write {
            id: 2,
            port: 5,
            format: 0,
            bytes: 1.0f32.to_ne_bytes().to_vec(),
        });
        round_trip(UiMessage::Close { id: 2 });
        round_trip(UiMessage::CloseRequest { id: 2 });
        round_trip(UiMessage::Title {
            id: 2,
            title: "Renamed".into(),
        });
        round_trip(UiMessage::Shutdown);
    }

    #[test]
    fn urid_round_trip() {
        round_trip(UiMessage::UridTable {
            entries: vec![
                ("http://example.com/a".into(), 1),
                ("http://example.com/b".into(), 7),
            ],
        });
        round_trip(UiMessage::UridRequest {
            uri: "http://example.com/c".into(),
        });
        round_trip(UiMessage::UridReply {
            uri: "http://example.com/c".into(),
            urid: 9,
        });
    }

    #[test]
    fn decode_rejects_garbage_and_truncation() {
        assert_eq!(decode(&[]), None);
        assert_eq!(decode(&[99]), None);
        let bytes = encode(&UiMessage::Open {
            id: 1,
            plugin_uri: "p".into(),
            ui_uri: "u".into(),
            ui_type: "t".into(),
            bundle: "b".into(),
            binary: "x".into(),
            title: "n".into(),
            scale: 1.0,
            update_rate: 30.0,
        });
        for cut in 1..bytes.len() {
            assert_eq!(decode(&bytes[..cut]), None, "cut {cut}");
        }
    }

    #[test]
    fn frame_buffer_reassembles_partial_frames() {
        let mut buffer = FrameBuffer::new();
        let a = frame(&UiMessage::Opened { id: 1 });
        let b = frame(&UiMessage::Close { id: 2 });
        buffer.push(&a[..2]);
        assert_eq!(buffer.pop(), None);
        buffer.push(&a[2..]);
        assert_eq!(buffer.pop(), Some(UiMessage::Opened { id: 1 }));
        buffer.push(&b);
        assert_eq!(buffer.pop(), Some(UiMessage::Close { id: 2 }));
        assert_eq!(buffer.pop(), None);
    }

    #[test]
    fn rewrite_atom_type_writes_header() {
        let mut bytes = vec![0u8; 12];
        assert!(rewrite_atom_type(&mut bytes, 0xdead_beef));
        assert_eq!(&bytes[4..8], &0xdead_beefu32.to_le_bytes());
        assert!(!rewrite_atom_type(&mut [0u8; 4], 1));
    }
}
