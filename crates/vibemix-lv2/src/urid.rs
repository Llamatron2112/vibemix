use std::collections::HashMap;
use std::ffi::{CStr, CString};
use std::os::raw::c_char;
use std::ptr;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Mutex;

use libc::c_void;
use lv2_raw::{LV2Feature, LV2Urid, LV2UridMap, LV2UridMapHandle, LV2_URID__MAP, LV2_URID__UNMAP};

#[repr(C)]
struct Lv2UridUnmap {
    handle: LV2UridMapHandle,
    unmap: extern "C" fn(LV2UridMapHandle, LV2Urid) -> *const c_char,
}

struct Inner {
    next: AtomicU32,
    by_uri: HashMap<String, u32>,
    by_id: Vec<CString>,
}

pub struct UridMap {
    inner: Mutex<Inner>,
    map_uri: CString,
    unmap_uri: CString,
    map_struct: LV2UridMap,
    unmap_struct: Lv2UridUnmap,
    map_feature: LV2Feature,
    unmap_feature: LV2Feature,
}

unsafe impl Send for UridMap {}
unsafe impl Sync for UridMap {}

impl UridMap {
    pub fn shared() -> &'static UridMap {
        let mut map = Box::new(UridMap {
            inner: Mutex::new(Inner {
                next: AtomicU32::new(1),
                by_uri: HashMap::new(),
                by_id: Vec::new(),
            }),
            map_uri: CString::new(LV2_URID__MAP).expect("urid map uri"),
            unmap_uri: CString::new(LV2_URID__UNMAP).expect("urid unmap uri"),
            map_struct: LV2UridMap {
                handle: ptr::null_mut(),
                map: urid_map,
            },
            unmap_struct: Lv2UridUnmap {
                handle: ptr::null_mut(),
                unmap: urid_unmap,
            },
            map_feature: LV2Feature {
                uri: ptr::null(),
                data: ptr::null_mut(),
            },
            unmap_feature: LV2Feature {
                uri: ptr::null(),
                data: ptr::null_mut(),
            },
        });

        let handle = &*map as *const UridMap as *mut c_void;
        let map_uri = map.map_uri.as_ptr();
        let unmap_uri = map.unmap_uri.as_ptr();
        let map_data = &map.map_struct as *const LV2UridMap as *mut c_void;
        let unmap_data = &map.unmap_struct as *const Lv2UridUnmap as *mut c_void;

        map.map_struct.handle = handle;
        map.unmap_struct.handle = handle;
        map.map_feature = LV2Feature {
            uri: map_uri,
            data: map_data,
        };
        map.unmap_feature = LV2Feature {
            uri: unmap_uri,
            data: unmap_data,
        };

        Box::leak(map)
    }

    pub fn features(&self) -> [&LV2Feature; 2] {
        [&self.map_feature, &self.unmap_feature]
    }

    pub fn map(&self, uri: &str) -> u32 {
        self.intern(uri)
    }

    pub fn unmap(&self, urid: u32) -> Option<String> {
        if urid == 0 {
            return None;
        }
        let guard = self.inner.lock().unwrap_or_else(|error| error.into_inner());
        guard
            .by_id
            .get((urid - 1) as usize)
            .map(|value| value.to_string_lossy().into_owned())
    }

    pub fn parts(
        &'static self,
    ) -> (
        *mut c_void,
        extern "C" fn(LV2UridMapHandle, *const c_char) -> LV2Urid,
        extern "C" fn(LV2UridMapHandle, LV2Urid) -> *const c_char,
    ) {
        (
            self as *const UridMap as *mut c_void,
            urid_map,
            urid_unmap,
        )
    }

    fn intern(&self, uri: &str) -> u32 {
        let mut guard = self.inner.lock().unwrap_or_else(|error| error.into_inner());
        if let Some(id) = guard.by_uri.get(uri) {
            return *id;
        }
        let id = guard.next.fetch_add(1, Ordering::Relaxed);
        guard.by_uri.insert(uri.to_string(), id);
        guard.by_id.push(CString::new(uri).unwrap_or_default());
        id
    }

    fn lookup(&self, urid: u32) -> *const c_char {
        if urid == 0 {
            return ptr::null();
        }
        let guard = self.inner.lock().unwrap_or_else(|error| error.into_inner());
        match guard.by_id.get((urid - 1) as usize) {
            Some(value) => value.as_ptr(),
            None => ptr::null(),
        }
    }
}

extern "C" fn urid_map(handle: LV2UridMapHandle, uri: *const c_char) -> LV2Urid {
    if handle.is_null() || uri.is_null() {
        return 0;
    }
    let host = unsafe { &*(handle as *const UridMap) };
    match unsafe { CStr::from_ptr(uri) }.to_str() {
        Ok(value) => host.intern(value),
        Err(_) => 0,
    }
}

extern "C" fn urid_unmap(handle: LV2UridMapHandle, urid: LV2Urid) -> *const c_char {
    if handle.is_null() {
        return ptr::null();
    }
    let host = unsafe { &*(handle as *const UridMap) };
    host.lookup(urid)
}
