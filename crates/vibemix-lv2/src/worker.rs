use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::thread::{self, JoinHandle, Thread};
use std::time::Duration;

use libc::c_void;
use rtrb::{Consumer, Producer};

pub const WORKER_SCHEDULE_URI: &str = "http://lv2plug.in/ns/ext/worker#schedule";
pub const WORKER_INTERFACE_URI: &str = "http://lv2plug.in/ns/ext/worker#interface";

const STATUS_SUCCESS: u32 = 0;
const STATUS_ERR_UNKNOWN: u32 = 1;
const STATUS_ERR_NO_SPACE: u32 = 2;
const WORK_MAX: usize = 4096;
const QUEUE_CAPACITY: usize = 8;
const PARK_TIMEOUT: Duration = Duration::from_millis(50);

pub type Handle = *mut c_void;
pub type RespondHandle = *mut c_void;
pub type RespondFn = extern "C" fn(RespondHandle, u32, *const c_void) -> u32;
pub type ScheduleHandle = *mut c_void;
pub type ScheduleFn = extern "C" fn(ScheduleHandle, u32, *const c_void) -> u32;

#[repr(C)]
pub struct WorkerSchedule {
    pub handle: ScheduleHandle,
    pub schedule: ScheduleFn,
}

#[repr(C)]
pub struct WorkerInterface {
    pub work:
        Option<unsafe extern "C" fn(Handle, RespondFn, RespondHandle, u32, *const c_void) -> u32>,
    pub work_response: Option<unsafe extern "C" fn(Handle, u32, *const c_void) -> u32>,
    pub end_run: Option<unsafe extern "C" fn(Handle) -> u32>,
}

struct WorkMessage {
    len: u32,
    data: [u8; WORK_MAX],
}

impl WorkMessage {
    fn empty() -> Self {
        Self {
            len: 0,
            data: [0u8; WORK_MAX],
        }
    }

    fn from_raw(size: u32, data: *const c_void) -> Option<Self> {
        if size as usize > WORK_MAX {
            return None;
        }
        let mut message = Self::empty();
        message.len = size;
        if size > 0 && !data.is_null() {
            unsafe {
                ptr::copy_nonoverlapping(
                    data as *const u8,
                    message.data.as_mut_ptr(),
                    size as usize,
                )
            };
        }
        Some(message)
    }
}

pub struct Worker {
    requests: Producer<WorkMessage>,
    responses: Consumer<WorkMessage>,
    shutdown: Arc<AtomicBool>,
    thread: OnceLock<Thread>,
    interface: *const WorkerInterface,
    instance: Handle,
    worker_requests: Option<Consumer<WorkMessage>>,
    worker_responses: Option<Producer<WorkMessage>>,
    join: Option<JoinHandle<()>>,
    schedule: WorkerSchedule,
}

unsafe impl Send for Worker {}

impl Worker {
    pub fn new() -> Box<Worker> {
        let (requests, worker_requests) = rtrb::RingBuffer::<WorkMessage>::new(QUEUE_CAPACITY);
        let (worker_responses, responses) = rtrb::RingBuffer::<WorkMessage>::new(QUEUE_CAPACITY);

        let mut worker = Box::new(Worker {
            requests,
            responses,
            shutdown: Arc::new(AtomicBool::new(false)),
            thread: OnceLock::new(),
            interface: ptr::null(),
            instance: ptr::null_mut(),
            worker_requests: Some(worker_requests),
            worker_responses: Some(worker_responses),
            join: None,
            schedule: WorkerSchedule {
                handle: ptr::null_mut(),
                schedule: worker_schedule,
            },
        });
        worker.schedule.handle = &mut *worker as *mut Worker as ScheduleHandle;
        worker
    }

    pub fn schedule_feature_data(&self) -> *mut c_void {
        &self.schedule as *const WorkerSchedule as *mut c_void
    }

    pub fn attach(&mut self, interface: *const WorkerInterface, instance: Handle) {
        self.interface = interface;
        self.instance = instance;

        let Some(requests) = self.worker_requests.take() else {
            return;
        };
        let Some(responses) = self.worker_responses.take() else {
            return;
        };

        let shutdown = Arc::clone(&self.shutdown);
        let interface_addr = interface as usize;
        let instance_addr = instance as usize;

        let join = thread::spawn(move || {
            worker_loop(
                requests,
                responses,
                shutdown,
                interface_addr as *const WorkerInterface,
                instance_addr as Handle,
            );
        });
        let _ = self.thread.set(join.thread().clone());
        self.join = Some(join);
    }

    pub fn deliver_responses(&mut self) {
        if self.interface.is_null() {
            return;
        }
        while let Ok(message) = self.responses.pop() {
            if self.interface.is_null() {
                continue;
            }
            let callback = unsafe { (*self.interface).work_response };
            if let Some(callback) = callback {
                unsafe {
                    callback(
                        self.instance,
                        message.len,
                        message.data.as_ptr() as *const c_void,
                    );
                }
            }
        }
        // Per the worker spec, tell the plugin once all of this block's
        // responses have been delivered.
        // SAFETY: `interface` is the plugin's worker interface, valid for the
        // life of the instance this worker is attached to.
        if let Some(end_run) = unsafe { (*self.interface).end_run } {
            unsafe { end_run(self.instance) };
        }
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.get() {
            thread.unpark();
        }
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

fn worker_loop(
    mut requests: Consumer<WorkMessage>,
    mut responses: Producer<WorkMessage>,
    shutdown: Arc<AtomicBool>,
    interface: *const WorkerInterface,
    instance: Handle,
) {
    let work = if interface.is_null() {
        None
    } else {
        unsafe { (*interface).work }
    };

    loop {
        let mut processed = false;
        while let Ok(message) = requests.pop() {
            processed = true;
            if let Some(work) = work {
                let respond_handle = &mut responses as *mut Producer<WorkMessage> as RespondHandle;
                unsafe {
                    work(
                        instance,
                        worker_respond,
                        respond_handle,
                        message.len,
                        message.data.as_ptr() as *const c_void,
                    );
                }
            }
        }

        if shutdown.load(Ordering::Relaxed) {
            break;
        }
        if !processed {
            thread::park_timeout(PARK_TIMEOUT);
        }
    }
}

extern "C" fn worker_schedule(handle: ScheduleHandle, size: u32, data: *const c_void) -> u32 {
    if handle.is_null() {
        return STATUS_ERR_UNKNOWN;
    }
    let Some(message) = WorkMessage::from_raw(size, data) else {
        return STATUS_ERR_NO_SPACE;
    };

    let worker = unsafe { &mut *(handle as *mut Worker) };
    match worker.requests.push(message) {
        Ok(()) => {
            if let Some(thread) = worker.thread.get() {
                thread.unpark();
            }
            STATUS_SUCCESS
        }
        Err(_) => STATUS_ERR_NO_SPACE,
    }
}

extern "C" fn worker_respond(handle: RespondHandle, size: u32, data: *const c_void) -> u32 {
    if handle.is_null() {
        return STATUS_ERR_UNKNOWN;
    }
    let Some(message) = WorkMessage::from_raw(size, data) else {
        return STATUS_ERR_NO_SPACE;
    };

    let responses = unsafe { &mut *(handle as *mut Producer<WorkMessage>) };
    match responses.push(message) {
        Ok(()) => STATUS_SUCCESS,
        Err(_) => STATUS_ERR_NO_SPACE,
    }
}
