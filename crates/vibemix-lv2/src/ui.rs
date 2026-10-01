use std::ffi::CString;
use std::os::raw::{c_char, c_int, c_void};

use crate::urid::UridMap;

pub const X11_UI_TYPE: &str = "http://lv2plug.in/ns/extensions/ui#X11UI";
pub const GTK2_UI_TYPE: &str = "http://lv2plug.in/ns/extensions/ui#GtkUI";
pub const GTK3_UI_TYPE: &str = "http://lv2plug.in/ns/extensions/ui#Gtk3UI";

/// Which toolkit a hostable UI uses. X11 is preferred when a plugin offers
/// both (it is the most predictable to embed).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UiKind {
    X11,
    Gtk2,
    Gtk3,
}

impl UiKind {
    pub fn ui_type(self) -> &'static str {
        match self {
            UiKind::X11 => X11_UI_TYPE,
            UiKind::Gtk2 => GTK2_UI_TYPE,
            UiKind::Gtk3 => GTK3_UI_TYPE,
        }
    }

    pub fn from_ui_type(ui_type: &str) -> Option<Self> {
        if ui_type == X11_UI_TYPE {
            Some(UiKind::X11)
        } else if ui_type == GTK2_UI_TYPE {
            Some(UiKind::Gtk2)
        } else if ui_type == GTK3_UI_TYPE {
            Some(UiKind::Gtk3)
        } else {
            None
        }
    }
}

pub type PortWrite = extern "C" fn(*mut c_void, u32, u32, u32, *const c_void);

#[repr(C)]
struct Lv2UridMap {
    handle: *mut c_void,
    map: extern "C" fn(*mut c_void, *const c_char) -> u32,
}

#[repr(C)]
struct Lv2UridUnmap {
    handle: *mut c_void,
    unmap: extern "C" fn(*mut c_void, u32) -> *const c_char,
}

extern "C" {
    fn vibemix_ui_init();

    /// Chosen GTK major version: 0 (none), 2 or 3. Decided once per process.
    fn vibemix_gtk_version() -> c_int;

    fn vibemix_ui_find(
        plugin_uri: *const c_char,
        want_type: *const c_char,
        ui_uri: *mut c_char,
        ui_uri_cap: c_int,
        bundle: *mut c_char,
        bundle_cap: c_int,
        binary: *mut c_char,
        binary_cap: c_int,
        requires_instance_access: *mut c_int,
    ) -> c_int;

    fn vibemix_ui_open(
        plugin_uri: *const c_char,
        ui_uri: *const c_char,
        ui_type: *const c_char,
        bundle_path: *const c_char,
        binary_path: *const c_char,
        plugin_handle: *mut c_void,
        urid_map: *mut c_void,
        urid_unmap: *mut c_void,
        write_cb: PortWrite,
        write_user: *mut c_void,
        headless: c_int,
        title: *const c_char,
    ) -> *mut c_void;

    fn vibemix_ui_pump(handle: *mut c_void);
    fn vibemix_ui_port_event(
        handle: *mut c_void,
        port_index: u32,
        size: u32,
        format: u32,
        buffer: *const c_void,
    );
    fn vibemix_ui_wants_close(handle: *mut c_void) -> c_int;
    fn vibemix_ui_close(handle: *mut c_void);
}

/// Initialise Xlib threading support. Must run before any other Xlib call in
/// the process; call once at engine startup.
pub fn init() {
    unsafe { vibemix_ui_init() };
}

/// The GTK major version chosen at runtime: 0 (none), 2 or 3. Used to tell the
/// out-of-process UI host which toolkit to load.
pub fn gtk_version() -> c_int {
    unsafe { vibemix_gtk_version() }
}

#[derive(Debug, Clone)]
pub struct UiCandidate {
    pub ui_uri: String,
    pub bundle: String,
    pub binary: String,
    pub kind: UiKind,
    /// The chosen UI reads the DSP instance directly, so it can only be hosted
    /// in the engine process (not the out-of-process UI host).
    pub requires_instance_access: bool,
}

fn find_ui_of_type(plugin_uri: &str, want: &str, kind: UiKind) -> Option<UiCandidate> {
    let plugin = CString::new(plugin_uri).ok()?;
    let want = CString::new(want).ok()?;

    let mut ui_uri = [0 as c_char; 2048];
    let mut bundle = [0 as c_char; 2048];
    let mut binary = [0 as c_char; 2048];
    let mut requires_instance_access: c_int = 0;

    let found = unsafe {
        vibemix_ui_find(
            plugin.as_ptr(),
            want.as_ptr(),
            ui_uri.as_mut_ptr(),
            ui_uri.len() as c_int,
            bundle.as_mut_ptr(),
            bundle.len() as c_int,
            binary.as_mut_ptr(),
            binary.len() as c_int,
            &mut requires_instance_access,
        )
    };
    if found == 0 {
        return None;
    }

    let to_string = |buffer: &[c_char]| unsafe {
        std::ffi::CStr::from_ptr(buffer.as_ptr())
            .to_string_lossy()
            .into_owned()
    };

    Some(UiCandidate {
        ui_uri: to_string(&ui_uri),
        bundle: to_string(&bundle),
        binary: to_string(&binary),
        kind,
        requires_instance_access: requires_instance_access != 0,
    })
}

/// Finds a hostable UI for a plugin, preferring X11, then the GTK major
/// version selected at runtime (GTK is optional; see `VIBEMIX_GTK`).
pub fn find_ui(plugin_uri: &str) -> Option<UiCandidate> {
    if let Some(candidate) = find_ui_of_type(plugin_uri, X11_UI_TYPE, UiKind::X11) {
        return Some(candidate);
    }
    match unsafe { vibemix_gtk_version() } {
        2 => find_ui_of_type(plugin_uri, GTK2_UI_TYPE, UiKind::Gtk2),
        3 => find_ui_of_type(plugin_uri, GTK3_UI_TYPE, UiKind::Gtk3),
        _ => None,
    }
}

pub struct UiWindow {
    handle: *mut c_void,
    _map: Box<Lv2UridMap>,
    _unmap: Box<Lv2UridUnmap>,
}

impl UiWindow {
    pub fn pump(&self) {
        unsafe { vibemix_ui_pump(self.handle) };
    }

    pub fn port_event(&self, port_index: u32, size: u32, format: u32, buffer: &[u8]) {
        unsafe {
            vibemix_ui_port_event(
                self.handle,
                port_index,
                size,
                format,
                buffer.as_ptr() as *const c_void,
            )
        };
    }

    pub fn wants_close(&self) -> bool {
        unsafe { vibemix_ui_wants_close(self.handle) != 0 }
    }
}

impl Drop for UiWindow {
    fn drop(&mut self) {
        unsafe { vibemix_ui_close(self.handle) };
    }
}

/// # Safety
/// `plugin_handle` must be a valid handle to a live plugin instance, and
/// `write_user` must stay valid for as long as the returned window lives.
#[allow(clippy::too_many_arguments)]
pub unsafe fn open(
    urid: &'static UridMap,
    plugin_uri: &str,
    candidate: &UiCandidate,
    plugin_handle: *mut c_void,
    write_cb: PortWrite,
    write_user: *mut c_void,
    headless: bool,
    title: &str,
) -> Option<UiWindow> {
    let plugin = CString::new(plugin_uri).ok()?;
    let ui_uri = CString::new(candidate.ui_uri.as_str()).ok()?;
    let ui_type = CString::new(candidate.kind.ui_type()).ok()?;
    let bundle = CString::new(candidate.bundle.as_str()).ok()?;
    let binary = CString::new(candidate.binary.as_str()).ok()?;
    let title = CString::new(title).unwrap_or_default();

    let (handle, map_fn, unmap_fn) = urid.parts();
    let mut map = Box::new(Lv2UridMap {
        handle,
        map: map_fn,
    });
    let mut unmap = Box::new(Lv2UridUnmap {
        handle,
        unmap: unmap_fn,
    });

    let window = unsafe {
        vibemix_ui_open(
            plugin.as_ptr(),
            ui_uri.as_ptr(),
            ui_type.as_ptr(),
            bundle.as_ptr(),
            binary.as_ptr(),
            plugin_handle,
            &mut *map as *mut Lv2UridMap as *mut c_void,
            &mut *unmap as *mut Lv2UridUnmap as *mut c_void,
            write_cb,
            write_user,
            headless as c_int,
            title.as_ptr(),
        )
    };
    if window.is_null() {
        return None;
    }

    Some(UiWindow {
        handle: window,
        _map: map,
        _unmap: unmap,
    })
}
