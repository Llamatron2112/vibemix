use std::ffi::{CStr, CString};
use std::os::raw::c_char;
use std::ptr;

use libc::c_void;
use lv2_raw::LV2Feature;

pub const STATE_INTERFACE_URI: &str = "http://lv2plug.in/ns/ext/state#interface";

const STATE_SUCCESS: u32 = 0;
const STATE_ERR_UNKNOWN: u32 = 1;

static MAP_PATH_URI: &[u8] = b"http://lv2plug.in/ns/ext/state#mapPath\0";

pub type Handle = *mut c_void;
pub type StateHandle = *mut c_void;
pub type StoreFn = extern "C" fn(StateHandle, u32, *const c_void, usize, u32, u32) -> u32;
pub type RetrieveFn =
    extern "C" fn(StateHandle, u32, *mut usize, *mut u32, *mut u32) -> *const c_void;

#[repr(C)]
pub struct StateInterface {
    pub save: Option<
        extern "C" fn(Handle, StoreFn, StateHandle, u32, *const *const LV2Feature) -> u32,
    >,
    pub restore: Option<
        extern "C" fn(Handle, RetrieveFn, StateHandle, u32, *const *const LV2Feature) -> u32,
    >,
}

#[repr(C)]
struct StateMapPath {
    handle: *mut c_void,
    abstract_path: extern "C" fn(*mut c_void, *const c_char) -> *mut c_char,
    absolute_path: extern "C" fn(*mut c_void, *const c_char) -> *mut c_char,
}

#[derive(Debug, Clone)]
pub struct StateEntry {
    pub key: u32,
    pub type_: u32,
    pub flags: u32,
    pub value: Vec<u8>,
}

pub struct StateProxy {
    interface: *const StateInterface,
    handle: Handle,
}

unsafe impl Send for StateProxy {}

impl Clone for StateProxy {
    fn clone(&self) -> Self {
        Self {
            interface: self.interface,
            handle: self.handle,
        }
    }
}

impl StateProxy {
    pub fn new(interface: *const StateInterface, handle: Handle) -> Self {
        Self { interface, handle }
    }

    pub fn supports_save(&self) -> bool {
        !self.interface.is_null() && unsafe { (*self.interface).save.is_some() }
    }

    pub fn save(&self) -> Vec<StateEntry> {
        let Some(save) = (unsafe { (*self.interface).save }) else {
            return Vec::new();
        };

        let mut context = StoreContext {
            entries: Vec::new(),
        };
        let map_path = StateMapPath {
            handle: ptr::null_mut(),
            abstract_path: identity_path,
            absolute_path: identity_path,
        };
        let feature = LV2Feature {
            uri: MAP_PATH_URI.as_ptr() as *const c_char,
            data: &map_path as *const StateMapPath as *mut c_void,
        };
        let features = [&feature as *const LV2Feature, ptr::null()];

        save(
            self.handle,
            store_callback,
            &mut context as *mut StoreContext as StateHandle,
            0,
            features.as_ptr(),
        );
        context.entries
    }

    pub fn restore(&self, entries: &[StateEntry]) {
        let Some(restore) = (unsafe { (*self.interface).restore }) else {
            return;
        };

        let mut context = RetrieveContext { entries };
        let map_path = StateMapPath {
            handle: ptr::null_mut(),
            abstract_path: identity_path,
            absolute_path: identity_path,
        };
        let feature = LV2Feature {
            uri: MAP_PATH_URI.as_ptr() as *const c_char,
            data: &map_path as *const StateMapPath as *mut c_void,
        };
        let features = [&feature as *const LV2Feature, ptr::null()];

        restore(
            self.handle,
            retrieve_callback,
            &mut context as *mut RetrieveContext as StateHandle,
            0,
            features.as_ptr(),
        );
    }
}

struct StoreContext {
    entries: Vec<StateEntry>,
}

struct RetrieveContext<'a> {
    entries: &'a [StateEntry],
}

extern "C" fn store_callback(
    handle: StateHandle,
    key: u32,
    value: *const c_void,
    size: usize,
    type_: u32,
    flags: u32,
) -> u32 {
    if handle.is_null() {
        return STATE_ERR_UNKNOWN;
    }
    let context = unsafe { &mut *(handle as *mut StoreContext) };
    let bytes = if value.is_null() || size == 0 {
        Vec::new()
    } else {
        unsafe { std::slice::from_raw_parts(value as *const u8, size).to_vec() }
    };
    context.entries.push(StateEntry {
        key,
        type_,
        flags,
        value: bytes,
    });
    STATE_SUCCESS
}

extern "C" fn retrieve_callback(
    handle: StateHandle,
    key: u32,
    size: *mut usize,
    type_: *mut u32,
    flags: *mut u32,
) -> *const c_void {
    if handle.is_null() {
        return ptr::null();
    }
    let context = unsafe { &*(handle as *const RetrieveContext) };
    let Some(entry) = context.entries.iter().find(|entry| entry.key == key) else {
        return ptr::null();
    };
    if !size.is_null() {
        unsafe { *size = entry.value.len() };
    }
    if !type_.is_null() {
        unsafe { *type_ = entry.type_ };
    }
    if !flags.is_null() {
        unsafe { *flags = entry.flags };
    }
    entry.value.as_ptr() as *const c_void
}

extern "C" fn identity_path(_handle: *mut c_void, path: *const c_char) -> *mut c_char {
    if path.is_null() {
        return ptr::null_mut();
    }
    let source = unsafe { CStr::from_ptr(path) };
    let owned = CString::new(source.to_bytes()).unwrap_or_default();
    let length = owned.as_bytes().len();
    unsafe {
        let out = libc::malloc(length + 1) as *mut c_char;
        if out.is_null() {
            return ptr::null_mut();
        }
        ptr::copy_nonoverlapping(owned.as_ptr(), out, length + 1);
        out
    }
}
