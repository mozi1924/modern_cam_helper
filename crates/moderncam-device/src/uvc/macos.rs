//! macOS UVC EP0 control transfer transport via IOKit `IOUSBDeviceInterface182`.
//!
//! Reads the live Configuration Descriptor to locate the VideoControl interface and
//! actual Camera Terminal / Processing Unit IDs (e.g. CT=1, PU=4 on Modern Webcam),
//! then issues standard UVC class control requests without interrupting `UVCAssistant`.

use std::ffi::{CStr, c_char, c_void};
use std::ptr;

use moderncam_protocol::uvc::{
    AE_MODE_MANUAL, CT_AE_MODE_CONTROL, CT_EXPOSURE_TIME_ABSOLUTE_CONTROL, CT_PRIVACY_CONTROL,
    CT_ZOOM_ABSOLUTE_CONTROL, ControlRange, PU_BRIGHTNESS_CONTROL, PU_CONTRAST_CONTROL,
    PU_POWER_LINE_FREQUENCY_CONTROL, PU_SATURATION_CONTROL, PU_SHARPNESS_CONTROL,
    PU_WHITE_BALANCE_TEMPERATURE_AUTO_CONTROL, PU_WHITE_BALANCE_TEMPERATURE_CONTROL,
    PowerLineCapability, PowerLineFreq, UVC_GET_CUR, UVC_GET_DEF, UVC_GET_MAX, UVC_GET_MIN,
    UVC_GET_RES, UVC_SET_CUR, UvcCapabilities, UvcControlUpdate, UvcState, UvcTopology,
    choose_ae_mode_byte, parse_uvc_topology,
};
use moderncam_protocol::{MICROSOFT_VID, MODERN_WEBCAM_PID};

use crate::error::DeviceError;
use crate::uvc::UvcTransport;

const K_CF_STRING_ENCODING_UTF8: u32 = 0x0800_0100;
const K_CF_NUMBER_SINT32_TYPE: i32 = 3;

const K_IO_RETURN_NOT_OPEN: i32 = 0xE000_02CDu32 as i32;
const K_IO_RETURN_EXCLUSIVE_ACCESS: i32 = 0xE000_02C5u32 as i32;
const K_IO_RETURN_OFFLINE: i32 = 0xE000_02D7u32 as i32;
const K_IO_RETURN_NOT_ATTACHED: i32 = 0xE000_02D9u32 as i32;

const BM_REQ_UVC_GET: u8 = 0xA1; // IN | CLASS | INTERFACE
const BM_REQ_UVC_SET: u8 = 0x21; // OUT | CLASS | INTERFACE

// kIOUSBDeviceUserClientTypeID: 9DC7B780-9EC0-11D4-A54F-000A27052861
const UUID_USB_DEVICE_USER_CLIENT: [u8; 16] = [
    0x9D, 0xC7, 0xB7, 0x80, 0x9E, 0xC0, 0x11, 0xD4, 0xA5, 0x4F, 0x00, 0x0A, 0x27, 0x05, 0x28, 0x61,
];
// kIOCFPlugInInterfaceID: C244E858-109C-11D4-91D4-0050E4C6426F
const UUID_CF_PLUGIN_INTERFACE: [u8; 16] = [
    0xC2, 0x44, 0xE8, 0x58, 0x10, 0x9C, 0x11, 0xD4, 0x91, 0xD4, 0x00, 0x50, 0xE4, 0xC6, 0x42, 0x6F,
];
// kIOUSBDeviceInterfaceID182: 152FC496-4891-11D5-9D52-000A27801E86
const UUID_USB_DEVICE_INTERFACE_182: [u8; 16] = [
    0x15, 0x2F, 0xC4, 0x96, 0x48, 0x91, 0x11, 0xD5, 0x9D, 0x52, 0x00, 0x0A, 0x27, 0x80, 0x1E, 0x86,
];

#[repr(C)]
#[derive(Clone, Copy)]
struct CFUUIDBytes {
    bytes: [u8; 16],
}

#[repr(C)]
struct IOUSBDevRequest {
    bm_request_type: u8,
    b_request: u8,
    w_value: u16,
    w_index: u16,
    w_length: u16,
    p_data: *mut c_void,
    w_len_done: u32,
}

type CFTypeRef = *const c_void;
type CFUUIDRef = *const c_void;
type CFStringRef = *const c_void;
type CFMutableDictionaryRef = *mut c_void;

type QueryInterfaceFn = unsafe extern "C" fn(*mut c_void, CFUUIDBytes, *mut *mut c_void) -> i32;
type ReleaseFn = unsafe extern "C" fn(*mut c_void) -> u32;
type USBDeviceOpenFn = unsafe extern "C" fn(*mut c_void) -> i32;
type USBDeviceCloseFn = unsafe extern "C" fn(*mut c_void) -> i32;
type GetConfigDescriptorPtrFn = unsafe extern "C" fn(*mut c_void, u8, *mut *const u8) -> i32;
type DeviceRequestFn = unsafe extern "C" fn(*mut c_void, *mut IOUSBDevRequest) -> i32;

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFStringCreateWithCString(
        alloc: CFTypeRef,
        c_str: *const c_char,
        encoding: u32,
    ) -> CFStringRef;
    fn CFGetTypeID(cf: CFTypeRef) -> usize;
    fn CFNumberGetTypeID() -> usize;
    fn CFNumberGetValue(number: CFTypeRef, the_type: i32, value_ptr: *mut c_void) -> bool;
    fn CFRelease(cf: CFTypeRef);
    fn CFUUIDGetConstantUUIDWithBytes(
        alloc: CFTypeRef,
        b0: u8,
        b1: u8,
        b2: u8,
        b3: u8,
        b4: u8,
        b5: u8,
        b6: u8,
        b7: u8,
        b8: u8,
        b9: u8,
        b10: u8,
        b11: u8,
        b12: u8,
        b13: u8,
        b14: u8,
        b15: u8,
    ) -> CFUUIDRef;
    fn CFUUIDGetUUIDBytes(uuid: CFUUIDRef) -> CFUUIDBytes;
}

#[link(name = "IOKit", kind = "framework")]
unsafe extern "C" {
    fn IOServiceMatching(name: *const c_char) -> CFMutableDictionaryRef;
    fn IOServiceGetMatchingServices(
        main_port: u32,
        matching: CFMutableDictionaryRef,
        existing: *mut u32,
    ) -> i32;
    fn IOIteratorNext(iterator: u32) -> u32;
    fn IOObjectRelease(object: u32) -> i32;
    fn IORegistryEntryCreateCFProperty(
        entry: u32,
        key: CFStringRef,
        allocator: CFTypeRef,
        options: u32,
    ) -> CFTypeRef;
    fn IOCreatePlugInInterfaceForService(
        service: u32,
        plugin_type: CFUUIDRef,
        interface_type: CFUUIDRef,
        the_interface: *mut *mut c_void,
        the_score: *mut i32,
    ) -> i32;
}

/// macOS UVC transport backed by `IOUSBDeviceInterface182`.
pub struct MacOsUvcTransport {
    plugin: *mut c_void,
    device: *mut c_void,
    topology: UvcTopology,
    caps: UvcCapabilities,
}

// Synchronous COM calls on `IOUSBDeviceInterface182` do not depend on a thread-bound RunLoop.
unsafe impl Send for MacOsUvcTransport {}

impl Drop for MacOsUvcTransport {
    fn drop(&mut self) {
        unsafe {
            if !self.device.is_null() {
                let rel: ReleaseFn = vtable_fn(self.device, 3);
                rel(self.device);
            }
            if !self.plugin.is_null() {
                let rel: ReleaseFn = vtable_fn(self.plugin, 3);
                rel(self.plugin);
            }
        }
    }
}

impl MacOsUvcTransport {
    pub fn open() -> Result<Self, DeviceError> {
        let matching = unsafe { IOServiceMatching(c"IOUSBHostDevice".as_ptr()) };
        if matching.is_null() {
            return Err(DeviceError::Uvc("IOServiceMatching returned null".into()));
        }
        let mut iter: u32 = 0;
        let rc = unsafe { IOServiceGetMatchingServices(0, matching, &mut iter) };
        if rc != 0 || iter == 0 {
            return Err(DeviceError::Uvc(format!(
                "IOServiceGetMatchingServices failed: 0x{:08X}",
                rc as u32
            )));
        }

        let mut found = None;
        loop {
            let svc = unsafe { IOIteratorNext(iter) };
            if svc == 0 {
                break;
            }
            let vid = reg_num(svc, c"idVendor");
            let pid = reg_num(svc, c"idProduct");
            if vid == Some(MICROSOFT_VID as i32) && pid == Some(MODERN_WEBCAM_PID as i32) {
                found = Some(svc);
                break;
            }
            unsafe { IOObjectRelease(svc) };
        }
        unsafe { IOObjectRelease(iter) };

        let svc = found.ok_or(DeviceError::NotFound)?;

        let dev_type_uuid = cfuuid_from(&UUID_USB_DEVICE_USER_CLIENT);
        let plugin_uuid = cfuuid_from(&UUID_CF_PLUGIN_INTERFACE);
        let mut plugin: *mut c_void = ptr::null_mut();
        let mut score: i32 = 0;
        let plug_rc = unsafe {
            IOCreatePlugInInterfaceForService(
                svc,
                dev_type_uuid,
                plugin_uuid,
                &mut plugin,
                &mut score,
            )
        };
        unsafe { IOObjectRelease(svc) };
        if plug_rc != 0 || plugin.is_null() {
            return Err(DeviceError::Uvc(format!(
                "IOCreatePlugInInterfaceForService failed: 0x{:08X}",
                plug_rc as u32
            )));
        }

        let iface_uuid = unsafe { CFUUIDGetUUIDBytes(cfuuid_from(&UUID_USB_DEVICE_INTERFACE_182)) };
        let mut device: *mut c_void = ptr::null_mut();
        let qi_hr = unsafe {
            let qi: QueryInterfaceFn = vtable_fn(plugin, 1);
            qi(plugin, iface_uuid, &mut device)
        };
        if qi_hr != 0 || device.is_null() {
            unsafe {
                let rel: ReleaseFn = vtable_fn(plugin, 3);
                rel(plugin);
            }
            return Err(DeviceError::Uvc(format!(
                "QueryInterface(IOUSBDeviceInterface182) failed: 0x{:08X}",
                qi_hr as u32
            )));
        }

        let config_bytes = read_config_descriptor(device).inspect_err(|_| unsafe {
            let rel_d: ReleaseFn = vtable_fn(device, 3);
            rel_d(device);
            let rel_p: ReleaseFn = vtable_fn(plugin, 3);
            rel_p(plugin);
        })?;

        let topology = parse_uvc_topology(&config_bytes).ok_or_else(|| {
            unsafe {
                let rel_d: ReleaseFn = vtable_fn(device, 3);
                rel_d(device);
                let rel_p: ReleaseFn = vtable_fn(plugin, 3);
                rel_p(plugin);
            }
            DeviceError::Uvc("VideoControl interface not found in USB config descriptor".into())
        })?;

        let mut transport = Self {
            plugin,
            device,
            topology,
            caps: UvcCapabilities::default(),
        };
        transport.caps = transport.probe_capabilities()?;
        Ok(transport)
    }

    fn ct_id(&self) -> Result<u8, DeviceError> {
        self.topology
            .camera_terminal_id
            .ok_or_else(|| DeviceError::Uvc("missing Camera Terminal ID in descriptor".into()))
    }

    fn pu_id(&self) -> Result<u8, DeviceError> {
        self.topology
            .processing_unit_id
            .ok_or_else(|| DeviceError::Uvc("missing Processing Unit ID in descriptor".into()))
    }

    fn vc_if(&self) -> u8 {
        self.topology.vc_interface
    }

    fn uvc_get(
        &self,
        unit_id: u8,
        cs: u8,
        b_request: u8,
        buf: &mut [u8],
    ) -> Result<(), DeviceError> {
        let mut req = IOUSBDevRequest {
            bm_request_type: BM_REQ_UVC_GET,
            b_request,
            w_value: (cs as u16) << 8,
            w_index: ((unit_id as u16) << 8) | (self.vc_if() as u16),
            w_length: buf.len() as u16,
            p_data: buf.as_mut_ptr() as *mut c_void,
            w_len_done: 0,
        };
        self.device_request(&mut req)
    }

    fn uvc_set(&self, unit_id: u8, cs: u8, data: &[u8]) -> Result<(), DeviceError> {
        let mut buf = data.to_vec();
        let mut req = IOUSBDevRequest {
            bm_request_type: BM_REQ_UVC_SET,
            b_request: UVC_SET_CUR,
            w_value: (cs as u16) << 8,
            w_index: ((unit_id as u16) << 8) | (self.vc_if() as u16),
            w_length: buf.len() as u16,
            p_data: buf.as_mut_ptr() as *mut c_void,
            w_len_done: 0,
        };
        self.device_request(&mut req)
    }

    fn device_request(&self, req: &mut IOUSBDevRequest) -> Result<(), DeviceError> {
        let dev_req: DeviceRequestFn = unsafe { vtable_fn(self.device, 26) };
        let rc = unsafe { dev_req(self.device, req as *mut IOUSBDevRequest) };
        if rc == 0 {
            return Ok(());
        }
        if rc == K_IO_RETURN_NOT_OPEN {
            // Fall back to brief open/request/close only if kernel requires open handle.
            let open_fn: USBDeviceOpenFn = unsafe { vtable_fn(self.device, 8) };
            let close_fn: USBDeviceCloseFn = unsafe { vtable_fn(self.device, 9) };
            let open_rc = unsafe { open_fn(self.device) };
            if open_rc == 0 {
                let rc2 = unsafe { dev_req(self.device, req as *mut IOUSBDevRequest) };
                unsafe { close_fn(self.device) };
                if rc2 == 0 {
                    return Ok(());
                }
                return Err(map_uvc_ioreturn(rc2));
            }
        }
        Err(map_uvc_ioreturn(rc))
    }

    fn get_u8(&self, unit: u8, cs: u8, req: u8) -> Result<u8, DeviceError> {
        let mut b = [0u8; 1];
        self.uvc_get(unit, cs, req, &mut b)?;
        Ok(b[0])
    }

    fn get_i16(&self, unit: u8, cs: u8, req: u8) -> Result<i16, DeviceError> {
        let mut b = [0u8; 2];
        self.uvc_get(unit, cs, req, &mut b)?;
        Ok(i16::from_le_bytes(b))
    }

    fn get_u16(&self, unit: u8, cs: u8, req: u8) -> Result<u16, DeviceError> {
        let mut b = [0u8; 2];
        self.uvc_get(unit, cs, req, &mut b)?;
        Ok(u16::from_le_bytes(b))
    }

    fn get_u32(&self, unit: u8, cs: u8, req: u8) -> Result<u32, DeviceError> {
        let mut b = [0u8; 4];
        self.uvc_get(unit, cs, req, &mut b)?;
        Ok(u32::from_le_bytes(b))
    }

    fn probe_range_i16(&self, unit: u8, cs: u8) -> Option<ControlRange> {
        Some(ControlRange {
            min: self.get_i16(unit, cs, UVC_GET_MIN).ok()? as i32,
            max: self.get_i16(unit, cs, UVC_GET_MAX).ok()? as i32,
            step: self.get_i16(unit, cs, UVC_GET_RES).ok()?.max(1) as i32,
            default: self.get_i16(unit, cs, UVC_GET_DEF).ok()? as i32,
        })
    }

    fn probe_range_u16(&self, unit: u8, cs: u8) -> Option<ControlRange> {
        Some(ControlRange {
            min: self.get_u16(unit, cs, UVC_GET_MIN).ok()? as i32,
            max: self.get_u16(unit, cs, UVC_GET_MAX).ok()? as i32,
            step: self.get_u16(unit, cs, UVC_GET_RES).ok()?.max(1) as i32,
            default: self.get_u16(unit, cs, UVC_GET_DEF).ok()? as i32,
        })
    }

    fn probe_range_u32(&self, unit: u8, cs: u8) -> Option<ControlRange> {
        Some(ControlRange {
            min: self.get_u32(unit, cs, UVC_GET_MIN).ok()? as i32,
            max: self.get_u32(unit, cs, UVC_GET_MAX).ok()? as i32,
            step: self.get_u32(unit, cs, UVC_GET_RES).ok()?.max(1) as i32,
            default: self.get_u32(unit, cs, UVC_GET_DEF).ok()? as i32,
        })
    }

    fn probe_capabilities(&self) -> Result<UvcCapabilities, DeviceError> {
        let ct = self.ct_id()?;
        let pu = self.pu_id()?;

        let ae_supported_modes = self
            .get_u8(ct, CT_AE_MODE_CONTROL, UVC_GET_RES)
            .unwrap_or(0x09);
        let auto_exposure = self.get_u8(ct, CT_AE_MODE_CONTROL, UVC_GET_CUR).is_ok();
        let white_balance_auto = self
            .get_u8(pu, PU_WHITE_BALANCE_TEMPERATURE_AUTO_CONTROL, UVC_GET_CUR)
            .is_ok();
        let privacy_shutter = self.get_u8(ct, CT_PRIVACY_CONTROL, UVC_GET_CUR).is_ok();

        let power_line_freq = self
            .get_u8(pu, PU_POWER_LINE_FREQUENCY_CONTROL, UVC_GET_DEF)
            .ok()
            .and_then(PowerLineFreq::from_uvc_u8)
            .map(|def| {
                let mn = self
                    .get_u8(pu, PU_POWER_LINE_FREQUENCY_CONTROL, UVC_GET_MIN)
                    .unwrap_or(0);
                let mx = self
                    .get_u8(pu, PU_POWER_LINE_FREQUENCY_CONTROL, UVC_GET_MAX)
                    .unwrap_or(3);
                PowerLineCapability {
                    disabled: mn == 0,
                    hz50: mn <= 1 && mx >= 1,
                    hz60: mn <= 2 && mx >= 2,
                    auto: mx >= 3,
                    default: def,
                }
            });

        Ok(UvcCapabilities {
            brightness: self.probe_range_i16(pu, PU_BRIGHTNESS_CONTROL),
            contrast: self.probe_range_u16(pu, PU_CONTRAST_CONTROL),
            saturation: self.probe_range_u16(pu, PU_SATURATION_CONTROL),
            sharpness: self.probe_range_u16(pu, PU_SHARPNESS_CONTROL),
            white_balance_auto,
            white_balance_temp: self.probe_range_u16(pu, PU_WHITE_BALANCE_TEMPERATURE_CONTROL),
            auto_exposure,
            ae_supported_modes,
            exposure_time: self.probe_range_u32(ct, CT_EXPOSURE_TIME_ABSOLUTE_CONTROL),
            power_line_freq,
            zoom: self.probe_range_u16(ct, CT_ZOOM_ABSOLUTE_CONTROL),
            privacy_shutter,
        })
    }
}

impl UvcTransport for MacOsUvcTransport {
    fn capabilities(&mut self) -> Result<UvcCapabilities, DeviceError> {
        Ok(self.caps.clone())
    }

    fn read_state(&mut self) -> Result<UvcState, DeviceError> {
        let ct = self.ct_id()?;
        let pu = self.pu_id()?;

        let brightness = self
            .get_i16(pu, PU_BRIGHTNESS_CONTROL, UVC_GET_CUR)
            .ok()
            .map(|v| v as i32);
        let contrast = self
            .get_u16(pu, PU_CONTRAST_CONTROL, UVC_GET_CUR)
            .ok()
            .map(|v| v as i32);
        let saturation = self
            .get_u16(pu, PU_SATURATION_CONTROL, UVC_GET_CUR)
            .ok()
            .map(|v| v as i32);
        let sharpness = self
            .get_u16(pu, PU_SHARPNESS_CONTROL, UVC_GET_CUR)
            .ok()
            .map(|v| v as i32);
        let white_balance_auto = self
            .get_u8(pu, PU_WHITE_BALANCE_TEMPERATURE_AUTO_CONTROL, UVC_GET_CUR)
            .ok()
            .map(|v| v != 0);
        let white_balance_temp = self
            .get_u16(pu, PU_WHITE_BALANCE_TEMPERATURE_CONTROL, UVC_GET_CUR)
            .ok()
            .map(|v| v as i32);
        let auto_exposure = self
            .get_u8(ct, CT_AE_MODE_CONTROL, UVC_GET_CUR)
            .ok()
            .map(|v| v != AE_MODE_MANUAL);
        let exposure_time = self
            .get_u32(ct, CT_EXPOSURE_TIME_ABSOLUTE_CONTROL, UVC_GET_CUR)
            .ok()
            .map(|v| v as i32);
        let power_line_freq = self
            .get_u8(pu, PU_POWER_LINE_FREQUENCY_CONTROL, UVC_GET_CUR)
            .ok()
            .and_then(PowerLineFreq::from_uvc_u8);
        let zoom = self
            .get_u16(ct, CT_ZOOM_ABSOLUTE_CONTROL, UVC_GET_CUR)
            .ok()
            .map(|v| v as i32);
        let privacy_shutter = self
            .get_u8(ct, CT_PRIVACY_CONTROL, UVC_GET_CUR)
            .ok()
            .map(|v| v != 0);

        Ok(UvcState {
            brightness,
            contrast,
            saturation,
            sharpness,
            white_balance_auto,
            white_balance_temp,
            auto_exposure,
            exposure_time,
            power_line_freq,
            zoom,
            privacy_shutter,
        })
    }

    fn apply(&mut self, update: UvcControlUpdate) -> Result<(), DeviceError> {
        let ct = self.ct_id()?;
        let pu = self.pu_id()?;

        match update {
            UvcControlUpdate::Brightness(v) => {
                let snapped = self.caps.brightness.map(|r| r.clamp_snap(v)).unwrap_or(v) as i16;
                self.uvc_set(pu, PU_BRIGHTNESS_CONTROL, &snapped.to_le_bytes())
            }
            UvcControlUpdate::Contrast(v) => {
                let snapped = self.caps.contrast.map(|r| r.clamp_snap(v)).unwrap_or(v) as u16;
                self.uvc_set(pu, PU_CONTRAST_CONTROL, &snapped.to_le_bytes())
            }
            UvcControlUpdate::Saturation(v) => {
                let snapped = self.caps.saturation.map(|r| r.clamp_snap(v)).unwrap_or(v) as u16;
                self.uvc_set(pu, PU_SATURATION_CONTROL, &snapped.to_le_bytes())
            }
            UvcControlUpdate::Sharpness(v) => {
                let snapped = self.caps.sharpness.map(|r| r.clamp_snap(v)).unwrap_or(v) as u16;
                self.uvc_set(pu, PU_SHARPNESS_CONTROL, &snapped.to_le_bytes())
            }
            UvcControlUpdate::WhiteBalanceAuto(on) => self.uvc_set(
                pu,
                PU_WHITE_BALANCE_TEMPERATURE_AUTO_CONTROL,
                &[if on { 1 } else { 0 }],
            ),
            UvcControlUpdate::WhiteBalanceTemp(v) => {
                // Disable auto WB first if currently enabled so firmware accepts manual Kelvin.
                if self
                    .get_u8(pu, PU_WHITE_BALANCE_TEMPERATURE_AUTO_CONTROL, UVC_GET_CUR)
                    .ok()
                    == Some(1)
                {
                    let _ = self.uvc_set(pu, PU_WHITE_BALANCE_TEMPERATURE_AUTO_CONTROL, &[0]);
                }
                let snapped = self
                    .caps
                    .white_balance_temp
                    .map(|r| r.clamp_snap(v))
                    .unwrap_or(v) as u16;
                self.uvc_set(
                    pu,
                    PU_WHITE_BALANCE_TEMPERATURE_CONTROL,
                    &snapped.to_le_bytes(),
                )
            }
            UvcControlUpdate::AutoExposure(on) => {
                let mode = choose_ae_mode_byte(on, self.caps.ae_supported_modes);
                self.uvc_set(ct, CT_AE_MODE_CONTROL, &[mode])
            }
            UvcControlUpdate::ExposureTime(v) => {
                // Switch to manual AE first if currently in auto exposure mode.
                if self
                    .get_u8(ct, CT_AE_MODE_CONTROL, UVC_GET_CUR)
                    .ok()
                    .is_some_and(|m| m != AE_MODE_MANUAL)
                {
                    let _ = self.uvc_set(ct, CT_AE_MODE_CONTROL, &[AE_MODE_MANUAL]);
                }
                let snapped = self
                    .caps
                    .exposure_time
                    .map(|r| r.clamp_snap(v))
                    .unwrap_or(v) as u32;
                self.uvc_set(
                    ct,
                    CT_EXPOSURE_TIME_ABSOLUTE_CONTROL,
                    &snapped.to_le_bytes(),
                )
            }
            UvcControlUpdate::PowerLineFreq(freq) => {
                self.uvc_set(pu, PU_POWER_LINE_FREQUENCY_CONTROL, &[freq.to_uvc_u8()])
            }
            UvcControlUpdate::Zoom(v) => {
                let snapped = self.caps.zoom.map(|r| r.clamp_snap(v)).unwrap_or(v) as u16;
                self.uvc_set(ct, CT_ZOOM_ABSOLUTE_CONTROL, &snapped.to_le_bytes())
            }
        }
    }

    fn reset_defaults(&mut self) -> Result<(), DeviceError> {
        let caps = self.caps.clone();
        if let Some(r) = caps.brightness {
            self.apply(UvcControlUpdate::Brightness(r.default))?;
        }
        if let Some(r) = caps.contrast {
            self.apply(UvcControlUpdate::Contrast(r.default))?;
        }
        if let Some(r) = caps.saturation {
            self.apply(UvcControlUpdate::Saturation(r.default))?;
        }
        if let Some(r) = caps.sharpness {
            self.apply(UvcControlUpdate::Sharpness(r.default))?;
        }
        if let Some(p) = caps.power_line_freq {
            self.apply(UvcControlUpdate::PowerLineFreq(p.default))?;
        }
        if let Some(r) = caps.zoom {
            self.apply(UvcControlUpdate::Zoom(r.default))?;
        }
        if caps.white_balance_auto {
            self.apply(UvcControlUpdate::WhiteBalanceAuto(true))?;
        } else if let Some(r) = caps.white_balance_temp {
            self.apply(UvcControlUpdate::WhiteBalanceTemp(r.default))?;
        }
        if caps.auto_exposure {
            self.apply(UvcControlUpdate::AutoExposure(true))?;
        } else if let Some(r) = caps.exposure_time {
            self.apply(UvcControlUpdate::ExposureTime(r.default))?;
        }
        Ok(())
    }

    fn read_privacy_shutter(&mut self) -> Result<Option<bool>, DeviceError> {
        let ct = self.ct_id()?;
        Ok(self
            .get_u8(ct, CT_PRIVACY_CONTROL, UVC_GET_CUR)
            .ok()
            .map(|v| v != 0))
    }
}

unsafe fn vtable_fn<T: Copy>(obj: *mut c_void, index: usize) -> T {
    let vtbl = unsafe { *(obj as *const *const *const c_void) };
    let fn_ptr = unsafe { *vtbl.add(index) };
    unsafe { std::mem::transmute_copy(&fn_ptr) }
}

fn cfuuid_from(bytes: &[u8; 16]) -> CFUUIDRef {
    unsafe {
        CFUUIDGetConstantUUIDWithBytes(
            ptr::null(),
            bytes[0],
            bytes[1],
            bytes[2],
            bytes[3],
            bytes[4],
            bytes[5],
            bytes[6],
            bytes[7],
            bytes[8],
            bytes[9],
            bytes[10],
            bytes[11],
            bytes[12],
            bytes[13],
            bytes[14],
            bytes[15],
        )
    }
}

fn reg_num(svc: u32, key: &CStr) -> Option<i32> {
    let cf_key =
        unsafe { CFStringCreateWithCString(ptr::null(), key.as_ptr(), K_CF_STRING_ENCODING_UTF8) };
    if cf_key.is_null() {
        return None;
    }
    let val_ref = unsafe { IORegistryEntryCreateCFProperty(svc, cf_key, ptr::null(), 0) };
    unsafe { CFRelease(cf_key) };
    if val_ref.is_null() {
        return None;
    }
    let out = unsafe {
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
    unsafe { CFRelease(val_ref) };
    out
}

fn read_config_descriptor(device: *mut c_void) -> Result<Vec<u8>, DeviceError> {
    let get_cfg: GetConfigDescriptorPtrFn = unsafe { vtable_fn(device, 21) };
    let mut cfg_ptr: *const u8 = ptr::null();
    let rc = unsafe { get_cfg(device, 0, &mut cfg_ptr) };
    if rc != 0 || cfg_ptr.is_null() {
        return Err(DeviceError::Uvc(format!(
            "GetConfigurationDescriptorPtr failed: 0x{:08X}",
            rc as u32
        )));
    }
    let header = unsafe { std::slice::from_raw_parts(cfg_ptr, 9) };
    let total_len = u16::from_le_bytes([header[2], header[3]]) as usize;
    if total_len < 9 {
        return Err(DeviceError::Uvc(
            "invalid USB configuration descriptor length".into(),
        ));
    }
    let bytes = unsafe { std::slice::from_raw_parts(cfg_ptr, total_len) };
    Ok(bytes.to_vec())
}

fn map_uvc_ioreturn(rc: i32) -> DeviceError {
    if rc == K_IO_RETURN_OFFLINE || rc == K_IO_RETURN_NOT_ATTACHED {
        return DeviceError::Disconnected;
    }
    if rc == K_IO_RETURN_EXCLUSIVE_ACCESS {
        return DeviceError::Uvc(
            "USB interface/device exclusively locked (kIOReturnExclusiveAccess)".into(),
        );
    }
    DeviceError::Uvc(format!("IOUSBDeviceRequest failed: 0x{:08X}", rc as u32))
}
