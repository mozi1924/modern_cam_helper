//! Windows UVC control transport backed by `cameras` (`controls` feature / DirectShow).
//!
//! Uses `IAMVideoProcAmp` and `IAMCameraControl` so image adjustments coexist cleanly with
//! Media Foundation live preview and require no custom WinUSB/Zadig driver replacement.

use cameras::{
    ControlCapabilities, ControlRange as CamRange, Controls, Device, PowerLineFrequency,
    apply_controls, control_capabilities, devices, read_controls, reset_to_defaults,
};
use moderncam_protocol::uvc::{
    AE_MODE_APERTURE_PRIORITY, AE_MODE_MANUAL, ControlRange, PowerLineCapability, PowerLineFreq,
    UvcCapabilities, UvcControlUpdate, UvcState,
};

use crate::error::DeviceError;
use crate::uvc::UvcTransport;

pub struct WindowsUvcTransport {
    device: Device,
    caps: UvcCapabilities,
}

impl WindowsUvcTransport {
    pub fn open() -> Result<Self, DeviceError> {
        let list = devices().map_err(|e| DeviceError::Uvc(e.to_string()))?;
        let device = list
            .into_iter()
            .find(|d| {
                let id_l = d.id.0.to_ascii_lowercase();
                let name_l = d.name.to_ascii_lowercase();
                (id_l.contains("vid_045e") && id_l.contains("pid_0840"))
                    || name_l.contains("modern webcam")
            })
            .ok_or(DeviceError::NotFound)?;

        let raw_caps =
            control_capabilities(&device).map_err(|e| DeviceError::Uvc(e.to_string()))?;
        let caps = convert_caps(&raw_caps);
        Ok(Self { device, caps })
    }
}

impl UvcTransport for WindowsUvcTransport {
    fn capabilities(&mut self) -> Result<UvcCapabilities, DeviceError> {
        Ok(self.caps.clone())
    }

    fn read_state(&mut self) -> Result<UvcState, DeviceError> {
        let raw = read_controls(&self.device).map_err(|e| DeviceError::Uvc(e.to_string()))?;
        Ok(UvcState {
            brightness: raw.brightness.map(|v| v.round() as i32),
            contrast: raw.contrast.map(|v| v.round() as i32),
            saturation: raw.saturation.map(|v| v.round() as i32),
            sharpness: raw.sharpness.map(|v| v.round() as i32),
            white_balance_auto: raw.auto_white_balance,
            white_balance_temp: raw.white_balance_temperature.map(|v| v.round() as i32),
            auto_exposure: raw.auto_exposure,
            exposure_time: raw.exposure.map(|v| v.round() as i32),
            power_line_freq: raw.power_line_frequency.map(from_cam_plf),
            zoom: raw.zoom.map(|v| v.round() as i32),
            privacy_shutter: None,
        })
    }

    fn apply(&mut self, update: UvcControlUpdate) -> Result<(), DeviceError> {
        let mut req = Controls::default();
        match update {
            UvcControlUpdate::Brightness(v) => req.brightness = Some(v as f32),
            UvcControlUpdate::Contrast(v) => req.contrast = Some(v as f32),
            UvcControlUpdate::Saturation(v) => req.saturation = Some(v as f32),
            UvcControlUpdate::Sharpness(v) => req.sharpness = Some(v as f32),
            UvcControlUpdate::WhiteBalanceAuto(on) => req.auto_white_balance = Some(on),
            UvcControlUpdate::WhiteBalanceTemp(v) => {
                req.auto_white_balance = Some(false);
                req.white_balance_temperature = Some(v as f32);
            }
            UvcControlUpdate::AutoExposure(on) => req.auto_exposure = Some(on),
            UvcControlUpdate::ExposureTime(v) => {
                req.auto_exposure = Some(false);
                req.exposure = Some(v as f32);
            }
            UvcControlUpdate::PowerLineFreq(freq) => {
                req.power_line_frequency = Some(to_cam_plf(freq));
            }
            UvcControlUpdate::Zoom(v) => req.zoom = Some(v as f32),
        }
        apply_controls(&self.device, &req).map_err(|e| DeviceError::Uvc(e.to_string()))
    }

    fn reset_defaults(&mut self) -> Result<(), DeviceError> {
        reset_to_defaults(&self.device).map_err(|e| DeviceError::Uvc(e.to_string()))
    }
}

fn convert_range(r: CamRange) -> ControlRange {
    ControlRange {
        min: r.min.round() as i32,
        max: r.max.round() as i32,
        step: r.step.round().max(1.0) as i32,
        default: r.default.round() as i32,
    }
}

fn from_cam_plf(p: PowerLineFrequency) -> PowerLineFreq {
    match p {
        PowerLineFrequency::Disabled => PowerLineFreq::Disabled,
        PowerLineFrequency::Hz50 => PowerLineFreq::Hz50,
        PowerLineFrequency::Hz60 => PowerLineFreq::Hz60,
        _ => PowerLineFreq::Auto,
    }
}

fn to_cam_plf(p: PowerLineFreq) -> PowerLineFrequency {
    match p {
        PowerLineFreq::Disabled => PowerLineFrequency::Disabled,
        PowerLineFreq::Hz50 => PowerLineFrequency::Hz50,
        PowerLineFreq::Hz60 => PowerLineFrequency::Hz60,
        PowerLineFreq::Auto => PowerLineFrequency::Auto,
    }
}

fn convert_caps(raw: &ControlCapabilities) -> UvcCapabilities {
    UvcCapabilities {
        brightness: raw.brightness.map(convert_range),
        contrast: raw.contrast.map(convert_range),
        saturation: raw.saturation.map(convert_range),
        sharpness: raw.sharpness.map(convert_range),
        white_balance_auto: raw.auto_white_balance.unwrap_or(false),
        white_balance_temp: raw.white_balance_temperature.map(convert_range),
        auto_exposure: raw.auto_exposure.unwrap_or(false),
        ae_supported_modes: AE_MODE_MANUAL | AE_MODE_APERTURE_PRIORITY,
        exposure_time: raw.exposure.map(convert_range),
        power_line_freq: raw.power_line_frequency.map(|p| PowerLineCapability {
            disabled: p.disabled,
            hz50: p.hz50,
            hz60: p.hz60,
            auto: p.auto,
            default: from_cam_plf(p.default),
        }),
        zoom: raw.zoom.map(convert_range),
        privacy_shutter: false,
    }
}
