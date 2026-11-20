//! Linux V4L2 UVC transport using `v4l`.
//!
//! Opens the Modern Webcam `/dev/video*` capture node directly (concurrent with live preview),
//! handles `V4L2_CID_EXPOSURE_AUTO` menu entries cleanly (Aperture Priority vs Full Auto),
//! and reads `V4L2_CID_PRIVACY`.

use std::fs;
use std::path::{Path, PathBuf};

use moderncam_protocol::uvc::{
    AE_MODE_APERTURE_PRIORITY, AE_MODE_AUTO, AE_MODE_MANUAL, ControlRange, PowerLineCapability,
    PowerLineFreq, UvcCapabilities, UvcControlUpdate, UvcState,
};
use v4l::Device as V4lDevice;
use v4l::context;
use v4l::control::{Control as V4lControl, Description, Flags as V4lFlags, Value as V4lValue};
use v4l::v4l_sys::{
    V4L2_CID_AUTO_WHITE_BALANCE, V4L2_CID_BRIGHTNESS, V4L2_CID_CONTRAST,
    V4L2_CID_EXPOSURE_ABSOLUTE, V4L2_CID_EXPOSURE_AUTO, V4L2_CID_POWER_LINE_FREQUENCY,
    V4L2_CID_PRIVACY, V4L2_CID_SATURATION, V4L2_CID_SHARPNESS, V4L2_CID_WHITE_BALANCE_TEMPERATURE,
    V4L2_CID_ZOOM_ABSOLUTE,
};

use crate::error::DeviceError;
use crate::uvc::UvcTransport;

const V4L2_EXPOSURE_AUTO_VAL: i64 = 0;
const V4L2_EXPOSURE_MANUAL_VAL: i64 = 1;
const V4L2_EXPOSURE_APERTURE_PRIORITY_VAL: i64 = 3;

pub struct LinuxUvcTransport {
    path: PathBuf,
    caps: UvcCapabilities,
    ae_auto_menu_val: i64,
}

impl LinuxUvcTransport {
    pub fn open() -> Result<Self, DeviceError> {
        let path = find_modern_webcam_video_node().ok_or(DeviceError::NotFound)?;
        let dev = V4lDevice::with_path(&path).map_err(map_v4l_err)?;
        let descs = dev.query_controls().map_err(map_v4l_err)?;
        let (caps, ae_auto_menu_val) = build_capabilities(&descs);
        Ok(Self {
            path,
            caps,
            ae_auto_menu_val,
        })
    }

    fn open_dev(&self) -> Result<V4lDevice, DeviceError> {
        V4lDevice::with_path(&self.path).map_err(map_v4l_err)
    }

    fn write_int(&self, dev: &V4lDevice, id: u32, value: i64) -> Result<(), DeviceError> {
        dev.set_control(V4lControl {
            id,
            value: V4lValue::Integer(value),
        })
        .map_err(map_v4l_err)
    }

    fn write_bool(&self, dev: &V4lDevice, id: u32, value: bool) -> Result<(), DeviceError> {
        dev.set_control(V4lControl {
            id,
            value: V4lValue::Boolean(value),
        })
        .map_err(map_v4l_err)
    }
}

impl UvcTransport for LinuxUvcTransport {
    fn capabilities(&mut self) -> Result<UvcCapabilities, DeviceError> {
        Ok(self.caps.clone())
    }

    fn read_state(&mut self) -> Result<UvcState, DeviceError> {
        let dev = self.open_dev()?;
        let descs = dev.query_controls().map_err(map_v4l_err)?;
        let mut state = UvcState::default();

        for d in descs {
            if is_unavailable(&d) {
                continue;
            }
            let Ok(ctrl) = dev.control(d.id) else {
                continue;
            };
            let Some(val) = control_as_i64(&ctrl.value) else {
                continue;
            };
            match d.id {
                V4L2_CID_BRIGHTNESS => state.brightness = Some(val as i32),
                V4L2_CID_CONTRAST => state.contrast = Some(val as i32),
                V4L2_CID_SATURATION => state.saturation = Some(val as i32),
                V4L2_CID_SHARPNESS => state.sharpness = Some(val as i32),
                V4L2_CID_AUTO_WHITE_BALANCE => state.white_balance_auto = Some(val != 0),
                V4L2_CID_WHITE_BALANCE_TEMPERATURE => state.white_balance_temp = Some(val as i32),
                V4L2_CID_EXPOSURE_AUTO => {
                    state.auto_exposure = Some(val != V4L2_EXPOSURE_MANUAL_VAL)
                }
                V4L2_CID_EXPOSURE_ABSOLUTE => state.exposure_time = Some(val as i32),
                V4L2_CID_POWER_LINE_FREQUENCY => {
                    state.power_line_freq = PowerLineFreq::from_uvc_u8(val as u8);
                }
                V4L2_CID_ZOOM_ABSOLUTE => state.zoom = Some(val as i32),
                V4L2_CID_PRIVACY => state.privacy_shutter = Some(val != 0),
                _ => {}
            }
        }
        Ok(state)
    }

    fn apply(&mut self, update: UvcControlUpdate) -> Result<(), DeviceError> {
        let dev = self.open_dev()?;
        match update {
            UvcControlUpdate::Brightness(v) => {
                let val = self.caps.brightness.map(|r| r.clamp_snap(v)).unwrap_or(v) as i64;
                self.write_int(&dev, V4L2_CID_BRIGHTNESS, val)
            }
            UvcControlUpdate::Contrast(v) => {
                let val = self.caps.contrast.map(|r| r.clamp_snap(v)).unwrap_or(v) as i64;
                self.write_int(&dev, V4L2_CID_CONTRAST, val)
            }
            UvcControlUpdate::Saturation(v) => {
                let val = self.caps.saturation.map(|r| r.clamp_snap(v)).unwrap_or(v) as i64;
                self.write_int(&dev, V4L2_CID_SATURATION, val)
            }
            UvcControlUpdate::Sharpness(v) => {
                let val = self.caps.sharpness.map(|r| r.clamp_snap(v)).unwrap_or(v) as i64;
                self.write_int(&dev, V4L2_CID_SHARPNESS, val)
            }
            UvcControlUpdate::WhiteBalanceAuto(on) => {
                self.write_bool(&dev, V4L2_CID_AUTO_WHITE_BALANCE, on)
            }
            UvcControlUpdate::WhiteBalanceTemp(v) => {
                let _ = self.write_bool(&dev, V4L2_CID_AUTO_WHITE_BALANCE, false);
                let val = self
                    .caps
                    .white_balance_temp
                    .map(|r| r.clamp_snap(v))
                    .unwrap_or(v) as i64;
                self.write_int(&dev, V4L2_CID_WHITE_BALANCE_TEMPERATURE, val)
            }
            UvcControlUpdate::AutoExposure(on) => {
                let val = if on {
                    self.ae_auto_menu_val
                } else {
                    V4L2_EXPOSURE_MANUAL_VAL
                };
                self.write_int(&dev, V4L2_CID_EXPOSURE_AUTO, val)
            }
            UvcControlUpdate::ExposureTime(v) => {
                let _ = self.write_int(&dev, V4L2_CID_EXPOSURE_AUTO, V4L2_EXPOSURE_MANUAL_VAL);
                let val = self
                    .caps
                    .exposure_time
                    .map(|r| r.clamp_snap(v))
                    .unwrap_or(v) as i64;
                self.write_int(&dev, V4L2_CID_EXPOSURE_ABSOLUTE, val)
            }
            UvcControlUpdate::PowerLineFreq(freq) => {
                self.write_int(&dev, V4L2_CID_POWER_LINE_FREQUENCY, freq.to_uvc_u8() as i64)
            }
            UvcControlUpdate::Zoom(v) => {
                let val = self.caps.zoom.map(|r| r.clamp_snap(v)).unwrap_or(v) as i64;
                self.write_int(&dev, V4L2_CID_ZOOM_ABSOLUTE, val)
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
        let dev = self.open_dev()?;
        match dev.control(V4L2_CID_PRIVACY) {
            Ok(ctrl) => Ok(control_as_i64(&ctrl.value).map(|v| v != 0)),
            Err(_) => Ok(None),
        }
    }
}

fn find_modern_webcam_video_node() -> Option<PathBuf> {
    for entry in context::enum_devices() {
        let path = entry.path().to_path_buf();
        let name_match = entry
            .name()
            .is_some_and(|n| n.to_ascii_lowercase().contains("modern webcam"));
        let sysfs_match = sysfs_matches_vid_pid(&path, "045e", "0840");
        if !(name_match || sysfs_match) {
            continue;
        }
        // Only the primary capture node exposes UVC camera/processing controls (not metadata node).
        if let Ok(dev) = V4lDevice::with_path(&path)
            && let Ok(descs) = dev.query_controls()
            && descs.iter().any(|d| d.id == V4L2_CID_BRIGHTNESS)
        {
            return Some(path);
        }
    }
    None
}

fn sysfs_matches_vid_pid(dev_path: &Path, vid_hex: &str, pid_hex: &str) -> bool {
    let Some(file_name) = dev_path.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    let sys_dev = PathBuf::from("/sys/class/video4linux")
        .join(file_name)
        .join("device");
    // Walk up 1..=2 levels to reach the USB device directory containing idVendor/idProduct.
    for candidate in [sys_dev.join(".."), sys_dev.join("../..")] {
        let vid = fs::read_to_string(candidate.join("idVendor")).ok();
        let pid = fs::read_to_string(candidate.join("idProduct")).ok();
        if let (Some(v), Some(p)) = (vid, pid)
            && v.trim().eq_ignore_ascii_case(vid_hex)
            && p.trim().eq_ignore_ascii_case(pid_hex)
        {
            return true;
        }
    }
    false
}

fn is_unavailable(d: &Description) -> bool {
    d.flags.contains(V4lFlags::DISABLED) || d.flags.contains(V4lFlags::INACTIVE)
}

fn desc_range(d: &Description) -> ControlRange {
    ControlRange {
        min: d.minimum as i32,
        max: d.maximum as i32,
        step: d.step.max(1) as i32,
        default: d.default as i32,
    }
}

fn build_capabilities(descs: &[Description]) -> (UvcCapabilities, i64) {
    let mut caps = UvcCapabilities::default();
    let mut ae_auto_menu_val = V4L2_EXPOSURE_APERTURE_PRIORITY_VAL;

    for d in descs {
        if is_unavailable(d) {
            continue;
        }
        match d.id {
            V4L2_CID_BRIGHTNESS => caps.brightness = Some(desc_range(d)),
            V4L2_CID_CONTRAST => caps.contrast = Some(desc_range(d)),
            V4L2_CID_SATURATION => caps.saturation = Some(desc_range(d)),
            V4L2_CID_SHARPNESS => caps.sharpness = Some(desc_range(d)),
            V4L2_CID_AUTO_WHITE_BALANCE => caps.white_balance_auto = true,
            V4L2_CID_WHITE_BALANCE_TEMPERATURE => caps.white_balance_temp = Some(desc_range(d)),
            V4L2_CID_EXPOSURE_AUTO => {
                caps.auto_exposure = true;
                let mut mask = AE_MODE_MANUAL;
                if let Some(items) = d.items.as_ref() {
                    let has_ap = items
                        .iter()
                        .any(|(v, _)| *v as i64 == V4L2_EXPOSURE_APERTURE_PRIORITY_VAL);
                    let has_full = items
                        .iter()
                        .any(|(v, _)| *v as i64 == V4L2_EXPOSURE_AUTO_VAL);
                    if has_ap {
                        mask |= AE_MODE_APERTURE_PRIORITY;
                        ae_auto_menu_val = V4L2_EXPOSURE_APERTURE_PRIORITY_VAL;
                    } else if has_full {
                        mask |= AE_MODE_AUTO;
                        ae_auto_menu_val = V4L2_EXPOSURE_AUTO_VAL;
                    }
                }
                caps.ae_supported_modes = mask;
            }
            V4L2_CID_EXPOSURE_ABSOLUTE => caps.exposure_time = Some(desc_range(d)),
            V4L2_CID_POWER_LINE_FREQUENCY => {
                let items = d.items.as_ref();
                let has = |target: i64| {
                    items.is_some_and(|list| list.iter().any(|(v, _)| *v as i64 == target))
                };
                caps.power_line_freq = Some(PowerLineCapability {
                    disabled: has(0),
                    hz50: has(1),
                    hz60: has(2),
                    auto: has(3),
                    default: PowerLineFreq::from_uvc_u8(d.default as u8)
                        .unwrap_or(PowerLineFreq::Auto),
                });
            }
            V4L2_CID_ZOOM_ABSOLUTE => caps.zoom = Some(desc_range(d)),
            V4L2_CID_PRIVACY => caps.privacy_shutter = true,
            _ => {}
        }
    }

    (caps, ae_auto_menu_val)
}

fn control_as_i64(val: &V4lValue) -> Option<i64> {
    match val {
        V4lValue::Integer(n) => Some(*n),
        V4lValue::Boolean(b) => Some(if *b { 1 } else { 0 }),
        _ => None,
    }
}

fn map_v4l_err(err: std::io::Error) -> DeviceError {
    match err.kind() {
        std::io::ErrorKind::PermissionDenied => DeviceError::PermissionDenied(err.to_string()),
        std::io::ErrorKind::NotFound => DeviceError::Disconnected,
        _ => DeviceError::Uvc(err.to_string()),
    }
}
