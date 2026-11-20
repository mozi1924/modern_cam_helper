//! macOS targeted `IOHIDManager` transport.
//!
//! Uses an exact `{VendorID, ProductID}` matching dictionary instead of `NULL` so macOS 15+
//! TCC Input Monitoring permission checks (`kIOReturnNotPermitted`) are never triggered.

use std::ffi::{CStr, c_char, c_int, c_void};
use std::ptr;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use moderncam_protocol::{
    CONTROL_USAGE, CONTROL_USAGE_PAGE, HID_REPORT_LEN, MICROSOFT_VID, MODERN_WEBCAM_PID,
    RESP_REPORT_ID,
};

use crate::error::DeviceError;
use crate::hid::HidTransport;

const K_CF_STRING_ENCODING_UTF8: u32 = 0x0800_0100;
const K_CF_NUMBER_SINT32_TYPE: c_int = 3;
const K_IO_HID_REPORT_TYPE_FEATURE: c_int = 2;

const K_IO_RETURN_NOT_PERMITTED: i32 = 0xE000_02E2u32 as i32;
const K_IO_RETURN_NOT_PRIVILEGED: i32 = 0xE000_02C1u32 as i32;
const K_IO_RETURN_OFFLINE: i32 = 0xE000_02D7u32 as i32;
const K_IO_RETURN_NOT_ATTACHED: i32 = 0xE000_02D9u32 as i32;

type CFTypeRef = *const c_void;
type CFStringRef = *const c_void;
type CFDictionaryRef = *const c_void;
type CFSetRef = *const c_void;
type CFRunLoopRef = *mut c_void;
type IOHIDManagerRef = *mut c_void;
type IOHIDDeviceRef = *mut c_void;
type CFIndex = isize;
type IOReturn = i32;

type IOHIDReportCallback = extern "C" fn(
    context: *mut c_void,
    result: IOReturn,
    sender: *mut c_void,
    report_type: c_int,
    report_id: u32,
    report: *const u8,
    report_length: CFIndex,
);

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    static kCFTypeDictionaryKeyCallBacks: c_void;
    static kCFTypeDictionaryValueCallBacks: c_void;
    static kCFRunLoopDefaultMode: CFStringRef;

    fn CFStringCreateWithCString(
        alloc: CFTypeRef,
        c_str: *const c_char,
        encoding: u32,
    ) -> CFStringRef;
    fn CFNumberCreate(alloc: CFTypeRef, the_type: c_int, value_ptr: *const c_void) -> CFTypeRef;
    fn CFDictionaryCreate(
        alloc: CFTypeRef,
        keys: *const CFTypeRef,
        values: *const CFTypeRef,
        num_values: CFIndex,
        key_callbacks: *const c_void,
        value_callbacks: *const c_void,
    ) -> CFDictionaryRef;
    fn CFSetGetCount(the_set: CFSetRef) -> CFIndex;
    fn CFSetGetValues(the_set: CFSetRef, values: *mut CFTypeRef);
    fn CFGetTypeID(cf: CFTypeRef) -> usize;
    fn CFNumberGetTypeID() -> usize;
    fn CFNumberGetValue(number: CFTypeRef, the_type: c_int, value_ptr: *mut c_void) -> bool;
    fn CFRelease(cf: CFTypeRef);
    fn CFRunLoopGetCurrent() -> CFRunLoopRef;
    fn CFRunLoopRunInMode(
        mode: CFStringRef,
        seconds: f64,
        return_after_source_handled: bool,
    ) -> i32;
}

#[link(name = "IOKit", kind = "framework")]
unsafe extern "C" {
    fn IOHIDManagerCreate(allocator: CFTypeRef, options: u32) -> IOHIDManagerRef;
    fn IOHIDManagerSetDeviceMatching(manager: IOHIDManagerRef, matching: CFDictionaryRef);
    fn IOHIDManagerOpen(manager: IOHIDManagerRef, options: u32) -> IOReturn;
    fn IOHIDManagerClose(manager: IOHIDManagerRef, options: u32) -> IOReturn;
    fn IOHIDManagerCopyDevices(manager: IOHIDManagerRef) -> CFSetRef;
    fn IOHIDDeviceOpen(device: IOHIDDeviceRef, options: u32) -> IOReturn;
    fn IOHIDDeviceClose(device: IOHIDDeviceRef, options: u32) -> IOReturn;
    fn IOHIDDeviceGetProperty(device: IOHIDDeviceRef, key: CFStringRef) -> CFTypeRef;
    fn IOHIDDeviceSetReport(
        device: IOHIDDeviceRef,
        report_type: c_int,
        report_id: CFIndex,
        report: *const u8,
        report_length: CFIndex,
    ) -> IOReturn;
    fn IOHIDDeviceRegisterInputReportCallback(
        device: IOHIDDeviceRef,
        report: *mut u8,
        report_length: CFIndex,
        callback: IOHIDReportCallback,
        context: *mut c_void,
    );
    fn IOHIDDeviceScheduleWithRunLoop(
        device: IOHIDDeviceRef,
        run_loop: CFRunLoopRef,
        run_loop_mode: CFStringRef,
    );
    fn IOHIDDeviceUnscheduleFromRunLoop(
        device: IOHIDDeviceRef,
        run_loop: CFRunLoopRef,
        run_loop_mode: CFStringRef,
    );
}

enum WorkerCmd {
    Send {
        frame: [u8; HID_REPORT_LEN],
        expected_echo: u8,
        timeout: Duration,
        reply: Sender<Result<Vec<u8>, DeviceError>>,
    },
    Shutdown,
}

struct ReportSink {
    tx: Sender<(u32, Vec<u8>)>,
}

extern "C" fn on_input_report(
    context: *mut c_void,
    _result: IOReturn,
    _sender: *mut c_void,
    _report_type: c_int,
    report_id: u32,
    report: *const u8,
    report_length: CFIndex,
) {
    if context.is_null() || report.is_null() || report_length <= 0 || report_length > 4096 {
        return;
    }
    let sink = unsafe { &*(context as *const ReportSink) };
    let slice = unsafe { std::slice::from_raw_parts(report, report_length as usize) };
    let _ = sink.tx.send((report_id, slice.to_vec()));
}

/// `Send`-safe handle to a macOS `IOHIDDevice` pinned to its owning `CFRunLoop` thread.
pub struct MacOsHidTransport {
    cmd_tx: Sender<WorkerCmd>,
    thread: Option<JoinHandle<()>>,
}

impl MacOsHidTransport {
    pub fn open() -> Result<Self, DeviceError> {
        let (cmd_tx, cmd_rx) = mpsc::channel::<WorkerCmd>();
        let (init_tx, init_rx) = mpsc::channel::<Result<(), DeviceError>>();

        let thread = thread::Builder::new()
            .name("moderncam-macos-hid".into())
            .spawn(move || run_hid_loop(cmd_rx, init_tx))
            .map_err(|e| DeviceError::Hid(format!("failed to spawn macOS HID thread: {e}")))?;

        match init_rx.recv() {
            Ok(Ok(())) => Ok(Self {
                cmd_tx,
                thread: Some(thread),
            }),
            Ok(Err(err)) => {
                let _ = thread.join();
                Err(err)
            }
            Err(_) => {
                let _ = thread.join();
                Err(DeviceError::Hid(
                    "macOS HID worker exited before initialization".into(),
                ))
            }
        }
    }
}

impl Drop for MacOsHidTransport {
    fn drop(&mut self) {
        let _ = self.cmd_tx.send(WorkerCmd::Shutdown);
        if let Some(handle) = self.thread.take() {
            let _ = handle.join();
        }
    }
}

impl HidTransport for MacOsHidTransport {
    fn send_feature_and_wait_input(
        &mut self,
        frame: &[u8; HID_REPORT_LEN],
        expected_echo: u8,
        timeout: Duration,
    ) -> Result<Vec<u8>, DeviceError> {
        let (reply_tx, reply_rx) = mpsc::channel();
        self.cmd_tx
            .send(WorkerCmd::Send {
                frame: *frame,
                expected_echo,
                timeout,
                reply: reply_tx,
            })
            .map_err(|_| DeviceError::Disconnected)?;

        reply_rx
            .recv_timeout(timeout + Duration::from_secs(1))
            .map_err(|_| DeviceError::Timeout)?
    }
}

fn cf_str(s: &CStr) -> CFStringRef {
    unsafe { CFStringCreateWithCString(ptr::null(), s.as_ptr(), K_CF_STRING_ENCODING_UTF8) }
}

fn device_num_prop(dev: IOHIDDeviceRef, key: &CStr) -> Option<i32> {
    let cf_key = cf_str(key);
    if cf_key.is_null() {
        return None;
    }
    let val_ref = unsafe { IOHIDDeviceGetProperty(dev, cf_key) };
    unsafe { CFRelease(cf_key) };
    if val_ref.is_null() {
        return None;
    }
    let result = unsafe {
        if CFGetTypeID(val_ref) == CFNumberGetTypeID() {
            let mut val: i32 = 0;
            if CFNumberGetValue(
                val_ref,
                K_CF_NUMBER_SINT32_TYPE,
                &mut val as *mut i32 as *mut c_void,
            ) {
                Some(val)
            } else {
                None
            }
        } else {
            None
        }
    };
    // Note: IOHIDDeviceGetProperty follows the CoreFoundation Get Rule (not Copy/Create),
    // so the returned CFTypeRef must NOT be CFRelease'd.
    result
}

fn create_vid_pid_match_dict(vid: u16, pid: u16) -> Option<CFDictionaryRef> {
    let k_vid = cf_str(c"VendorID");
    let k_pid = cf_str(c"ProductID");
    let v_vid_raw = vid as i32;
    let v_pid_raw = pid as i32;
    let v_vid = unsafe {
        CFNumberCreate(
            ptr::null(),
            K_CF_NUMBER_SINT32_TYPE,
            &v_vid_raw as *const i32 as *const c_void,
        )
    };
    let v_pid = unsafe {
        CFNumberCreate(
            ptr::null(),
            K_CF_NUMBER_SINT32_TYPE,
            &v_pid_raw as *const i32 as *const c_void,
        )
    };

    if k_vid.is_null() || k_pid.is_null() || v_vid.is_null() || v_pid.is_null() {
        for r in [k_vid, k_pid, v_vid, v_pid] {
            if !r.is_null() {
                unsafe { CFRelease(r) };
            }
        }
        return None;
    }

    let keys = [k_vid, k_pid];
    let vals = [v_vid, v_pid];
    let dict = unsafe {
        CFDictionaryCreate(
            ptr::null(),
            keys.as_ptr(),
            vals.as_ptr(),
            2,
            &raw const kCFTypeDictionaryKeyCallBacks,
            &raw const kCFTypeDictionaryValueCallBacks,
        )
    };

    for r in [k_vid, k_pid, v_vid, v_pid] {
        unsafe { CFRelease(r) };
    }

    if dict.is_null() { None } else { Some(dict) }
}

fn map_ioreturn(rc: IOReturn, op: &str) -> DeviceError {
    if rc == K_IO_RETURN_NOT_PERMITTED || rc == K_IO_RETURN_NOT_PRIVILEGED {
        return DeviceError::PermissionDenied(format!("{op}: IOReturn 0x{:08X}", rc as u32));
    }
    if rc == K_IO_RETURN_OFFLINE || rc == K_IO_RETURN_NOT_ATTACHED {
        return DeviceError::Disconnected;
    }
    DeviceError::Hid(format!("{op} failed: IOReturn 0x{:08X}", rc as u32))
}

fn select_matching_device(dev_set: CFSetRef) -> Option<IOHIDDeviceRef> {
    if dev_set.is_null() {
        return None;
    }
    let count = unsafe { CFSetGetCount(dev_set) };
    if count <= 0 {
        return None;
    }
    let mut refs = vec![ptr::null(); count as usize];
    unsafe { CFSetGetValues(dev_set, refs.as_mut_ptr()) };

    let mut fallback: Option<IOHIDDeviceRef> = None;
    for r in refs {
        if r.is_null() {
            continue;
        }
        let dev = r as IOHIDDeviceRef;
        let vid = device_num_prop(dev, c"VendorID");
        let pid = device_num_prop(dev, c"ProductID");
        if vid != Some(MICROSOFT_VID as i32) || pid != Some(MODERN_WEBCAM_PID as i32) {
            continue;
        }
        let up = device_num_prop(dev, c"PrimaryUsagePage");
        let u = device_num_prop(dev, c"PrimaryUsage");
        if up == Some(CONTROL_USAGE_PAGE as i32) && u == Some(CONTROL_USAGE as i32) {
            return Some(dev);
        }
        if fallback.is_none() {
            fallback = Some(dev);
        }
    }
    fallback
}

fn run_hid_loop(cmd_rx: Receiver<WorkerCmd>, init_tx: Sender<Result<(), DeviceError>>) {
    let mgr = unsafe { IOHIDManagerCreate(ptr::null(), 0) };
    if mgr.is_null() {
        let _ = init_tx.send(Err(DeviceError::Hid("IOHIDManagerCreate failed".into())));
        return;
    }

    let Some(match_dict) = create_vid_pid_match_dict(MICROSOFT_VID, MODERN_WEBCAM_PID) else {
        unsafe { CFRelease(mgr) };
        let _ = init_tx.send(Err(DeviceError::Hid(
            "failed to build HID matching CFDictionary".into(),
        )));
        return;
    };

    unsafe { IOHIDManagerSetDeviceMatching(mgr, match_dict) };
    let open_rc = unsafe { IOHIDManagerOpen(mgr, 0) };
    unsafe { CFRelease(match_dict) };
    if open_rc != 0 {
        unsafe { CFRelease(mgr) };
        let _ = init_tx.send(Err(map_ioreturn(open_rc, "IOHIDManagerOpen")));
        return;
    }

    let dev_set = unsafe { IOHIDManagerCopyDevices(mgr) };
    let Some(dev) = select_matching_device(dev_set) else {
        if !dev_set.is_null() {
            unsafe { CFRelease(dev_set) };
        }
        unsafe {
            IOHIDManagerClose(mgr, 0);
            CFRelease(mgr);
        }
        let _ = init_tx.send(Err(DeviceError::NotFound));
        return;
    };

    let dev_open_rc = unsafe { IOHIDDeviceOpen(dev, 0) };
    if dev_open_rc != 0 {
        unsafe {
            CFRelease(dev_set);
            IOHIDManagerClose(mgr, 0);
            CFRelease(mgr);
        }
        let _ = init_tx.send(Err(map_ioreturn(dev_open_rc, "IOHIDDeviceOpen")));
        return;
    }

    let (report_tx, report_rx) = mpsc::channel::<(u32, Vec<u8>)>();
    let sink = Box::new(ReportSink { tx: report_tx });
    let sink_ptr = Box::into_raw(sink);
    let mut report_buf = vec![0u8; 4096];

    let run_loop = unsafe { CFRunLoopGetCurrent() };
    let mode = unsafe { kCFRunLoopDefaultMode };
    unsafe {
        IOHIDDeviceRegisterInputReportCallback(
            dev,
            report_buf.as_mut_ptr(),
            report_buf.len() as CFIndex,
            on_input_report,
            sink_ptr as *mut c_void,
        );
        IOHIDDeviceScheduleWithRunLoop(dev, run_loop, mode);
    }

    let _ = init_tx.send(Ok(()));

    while let Ok(cmd) = cmd_rx.recv() {
        match cmd {
            WorkerCmd::Shutdown => break,
            WorkerCmd::Send {
                frame,
                expected_echo,
                timeout,
                reply,
            } => {
                let res = perform_command(
                    dev,
                    run_loop,
                    mode,
                    &report_rx,
                    &frame,
                    expected_echo,
                    timeout,
                );
                let _ = reply.send(res);
            }
        }
    }

    unsafe {
        IOHIDDeviceUnscheduleFromRunLoop(dev, run_loop, mode);
        IOHIDDeviceClose(dev, 0);
        drop(Box::from_raw(sink_ptr));
        CFRelease(dev_set);
        IOHIDManagerClose(mgr, 0);
        CFRelease(mgr);
    }
    let _ = report_buf;
}

fn perform_command(
    dev: IOHIDDeviceRef,
    _run_loop: CFRunLoopRef,
    mode: CFStringRef,
    report_rx: &Receiver<(u32, Vec<u8>)>,
    frame: &[u8; HID_REPORT_LEN],
    expected_echo: u8,
    timeout: Duration,
) -> Result<Vec<u8>, DeviceError> {
    while report_rx.try_recv().is_ok() {}

    let report_id = frame[0] as CFIndex;
    let mut rc = unsafe {
        IOHIDDeviceSetReport(
            dev,
            K_IO_HID_REPORT_TYPE_FEATURE,
            report_id,
            frame.as_ptr(),
            frame.len() as CFIndex,
        )
    };
    if rc != 0 && rc != K_IO_RETURN_OFFLINE && rc != K_IO_RETURN_NOT_ATTACHED {
        // Unsupported opcodes stall the control pipe briefly; retry once after 50ms.
        thread::sleep(Duration::from_millis(50));
        rc = unsafe {
            IOHIDDeviceSetReport(
                dev,
                K_IO_HID_REPORT_TYPE_FEATURE,
                report_id,
                frame.as_ptr(),
                frame.len() as CFIndex,
            )
        };
    }
    if rc != 0 {
        return Err(map_ioreturn(rc, "IOHIDDeviceSetReport"));
    }

    let deadline = Instant::now() + timeout;
    loop {
        while let Ok((rid, data)) = report_rx.try_recv() {
            if rid == RESP_REPORT_ID as u32 || data.first() == Some(&RESP_REPORT_ID) {
                let normalized = if data.first() == Some(&RESP_REPORT_ID) {
                    data
                } else {
                    let mut prefixed = Vec::with_capacity(data.len() + 1);
                    prefixed.push(RESP_REPORT_ID);
                    prefixed.extend_from_slice(&data);
                    prefixed
                };
                if normalized.get(1) == Some(&expected_echo) {
                    return Ok(normalized);
                }
            }
        }
        if Instant::now() >= deadline {
            return Err(DeviceError::Timeout);
        }
        unsafe { CFRunLoopRunInMode(mode, 0.03, false) };
    }
}
