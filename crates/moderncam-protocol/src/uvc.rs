//! Standard UVC descriptor parsing and unified cross-platform UVC control model.

use std::fmt;

/// USB Video class code (`bInterfaceClass = 0x0E`).
pub const USB_CLASS_VIDEO: u8 = 0x0E;
/// UVC VideoControl subclass (`bInterfaceSubClass = 0x01`).
pub const USB_SUBCLASS_VIDEOCONTROL: u8 = 0x01;
/// Class-specific interface descriptor type (`CS_INTERFACE`).
pub const DESC_CS_INTERFACE: u8 = 0x24;
/// Standard interface descriptor type (`INTERFACE`).
pub const DESC_INTERFACE: u8 = 0x04;

/// UVC Class-Specific Request Codes.
pub const UVC_SET_CUR: u8 = 0x01;
pub const UVC_GET_CUR: u8 = 0x81;
pub const UVC_GET_MIN: u8 = 0x82;
pub const UVC_GET_MAX: u8 = 0x83;
pub const UVC_GET_RES: u8 = 0x84;
pub const UVC_GET_DEF: u8 = 0x87;

/// UVC Camera Terminal Control Selectors.
pub const CT_AE_MODE_CONTROL: u8 = 0x02;
pub const CT_EXPOSURE_TIME_ABSOLUTE_CONTROL: u8 = 0x04;
pub const CT_ZOOM_ABSOLUTE_CONTROL: u8 = 0x0B;
pub const CT_PRIVACY_CONTROL: u8 = 0x11;

/// UVC Processing Unit Control Selectors.
pub const PU_BRIGHTNESS_CONTROL: u8 = 0x02;
pub const PU_CONTRAST_CONTROL: u8 = 0x03;
pub const PU_POWER_LINE_FREQUENCY_CONTROL: u8 = 0x05;
pub const PU_SATURATION_CONTROL: u8 = 0x07;
pub const PU_SHARPNESS_CONTROL: u8 = 0x08;
pub const PU_WHITE_BALANCE_TEMPERATURE_CONTROL: u8 = 0x0A;
pub const PU_WHITE_BALANCE_TEMPERATURE_AUTO_CONTROL: u8 = 0x0B;

/// Camera Terminal (`CT`) Auto-Exposure Mode bitmask flags.
pub const AE_MODE_MANUAL: u8 = 0x01;
pub const AE_MODE_AUTO: u8 = 0x02;
pub const AE_MODE_SHUTTER_PRIORITY: u8 = 0x04;
pub const AE_MODE_APERTURE_PRIORITY: u8 = 0x08;

/// Discovered VideoControl entity IDs parsed from a USB Configuration Descriptor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UvcTopology {
    pub vc_interface: u8,
    pub camera_terminal_id: Option<u8>,
    pub camera_controls_mask: u32,
    pub processing_unit_id: Option<u8>,
    pub processing_controls_mask: u32,
    pub extension_unit_ids: Vec<u8>,
}

/// Parses a full USB Configuration Descriptor to locate the VideoControl
/// interface, Camera Terminal ID, Processing Unit ID, and capability masks.
pub fn parse_uvc_topology(config_desc: &[u8]) -> Option<UvcTopology> {
    let mut i = 0usize;
    let mut vc_interface: Option<u8> = None;
    let mut in_vc_interface = false;
    let mut camera_terminal_id = None;
    let mut camera_controls_mask = 0u32;
    let mut processing_unit_id = None;
    let mut processing_controls_mask = 0u32;
    let mut extension_unit_ids = Vec::new();

    while i + 2 <= config_desc.len() {
        let len = config_desc[i] as usize;
        if len < 2 || i + len > config_desc.len() {
            break;
        }
        let desc = &config_desc[i..i + len];
        let desc_type = desc[1];

        if desc_type == DESC_INTERFACE && desc.len() >= 9 {
            let iface_num = desc[2];
            let class = desc[5];
            let subclass = desc[6];
            in_vc_interface = class == USB_CLASS_VIDEO && subclass == USB_SUBCLASS_VIDEOCONTROL;
            if in_vc_interface && vc_interface.is_none() {
                vc_interface = Some(iface_num);
            }
        } else if desc_type == DESC_CS_INTERFACE && in_vc_interface && desc.len() >= 4 {
            let subtype = desc[2];
            match subtype {
                // VC_INPUT_TERMINAL
                0x02 if desc.len() >= 15 => {
                    let terminal_type = u16::from_le_bytes([desc[4], desc[5]]);
                    // ITT_CAMERA = 0x0201
                    if terminal_type == 0x0201 {
                        camera_terminal_id = Some(desc[3]);
                        let ctl_size = desc[14] as usize;
                        camera_controls_mask = read_le_mask(&desc[15..], ctl_size);
                    }
                }
                // VC_PROCESSING_UNIT
                0x05 if desc.len() >= 8 => {
                    processing_unit_id = Some(desc[3]);
                    let ctl_size = desc[7] as usize;
                    processing_controls_mask = read_le_mask(&desc[8..], ctl_size);
                }
                // VC_EXTENSION_UNIT
                0x06 => {
                    extension_unit_ids.push(desc[3]);
                }
                _ => {}
            }
        }

        i += len;
    }

    vc_interface.map(|vc| UvcTopology {
        vc_interface: vc,
        camera_terminal_id,
        camera_controls_mask,
        processing_unit_id,
        processing_controls_mask,
        extension_unit_ids,
    })
}

fn read_le_mask(bytes: &[u8], count: usize) -> u32 {
    let take = count.min(bytes.len()).min(4);
    let mut acc = 0u32;
    for (idx, &b) in bytes[..take].iter().enumerate() {
        acc |= (b as u32) << (idx * 8);
    }
    acc
}

/// AC mains anti-flicker setting mapped to UVC `PU_POWER_LINE_FREQUENCY_CONTROL`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PowerLineFreq {
    Disabled,
    Hz50,
    Hz60,
    Auto,
}

impl PowerLineFreq {
    pub const fn from_uvc_u8(val: u8) -> Option<Self> {
        match val {
            0 => Some(Self::Disabled),
            1 => Some(Self::Hz50),
            2 => Some(Self::Hz60),
            3 => Some(Self::Auto),
            _ => None,
        }
    }

    pub const fn to_uvc_u8(self) -> u8 {
        match self {
            Self::Disabled => 0,
            Self::Hz50 => 1,
            Self::Hz60 => 2,
            Self::Auto => 3,
        }
    }

    pub const fn label_zh(self) -> &'static str {
        match self {
            Self::Disabled => "关闭",
            Self::Hz50 => "50 Hz",
            Self::Hz60 => "60 Hz",
            Self::Auto => "自动",
        }
    }
}

impl fmt::Display for PowerLineFreq {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label_zh())
    }
}

/// Integer bounds reported by the hardware/driver for a numeric UVC control.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ControlRange {
    pub min: i32,
    pub max: i32,
    pub step: i32,
    pub default: i32,
}

impl ControlRange {
    /// Clamps `val` into `[min, max]` and snaps to the nearest step grid relative to `min`.
    pub fn clamp_snap(&self, val: i32) -> i32 {
        let clamped = val.clamp(self.min, self.max);
        let step = self.step.max(1);
        let offset = clamped - self.min;
        let snapped = self.min + ((offset + step / 2) / step) * step;
        snapped.clamp(self.min, self.max)
    }
}

/// Supported power-line frequency modes reported by the device.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PowerLineCapability {
    pub disabled: bool,
    pub hz50: bool,
    pub hz60: bool,
    pub auto: bool,
    pub default: PowerLineFreq,
}

impl Default for PowerLineCapability {
    fn default() -> Self {
        Self {
            disabled: false,
            hz50: true,
            hz60: true,
            auto: true,
            default: PowerLineFreq::Auto,
        }
    }
}

/// Capability ranges probed from the UVC backend.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UvcCapabilities {
    pub brightness: Option<ControlRange>,
    pub contrast: Option<ControlRange>,
    pub saturation: Option<ControlRange>,
    pub sharpness: Option<ControlRange>,
    pub white_balance_auto: bool,
    pub white_balance_temp: Option<ControlRange>,
    pub auto_exposure: bool,
    /// Bitmask of supported UVC AE modes (e.g., `0x09` = Manual | AperturePriority).
    pub ae_supported_modes: u8,
    pub exposure_time: Option<ControlRange>,
    pub power_line_freq: Option<PowerLineCapability>,
    pub zoom: Option<ControlRange>,
    pub privacy_shutter: bool,
}

/// Live snapshot of UVC control values.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UvcState {
    pub brightness: Option<i32>,
    pub contrast: Option<i32>,
    pub saturation: Option<i32>,
    pub sharpness: Option<i32>,
    pub white_balance_auto: Option<bool>,
    pub white_balance_temp: Option<i32>,
    pub auto_exposure: Option<bool>,
    /// Exposure time in UVC native units (`100 µs` on macOS/V4L2; platform-specific on Windows).
    pub exposure_time: Option<i32>,
    pub power_line_freq: Option<PowerLineFreq>,
    pub zoom: Option<i32>,
    /// Read-only physical privacy shutter state (`true` = shutter closed).
    pub privacy_shutter: Option<bool>,
}

/// Single control update sent from UI/CLI to the UVC backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum UvcControlKey {
    Brightness,
    Contrast,
    Saturation,
    Sharpness,
    WhiteBalanceAuto,
    WhiteBalanceTemp,
    AutoExposure,
    ExposureTime,
    PowerLineFreq,
    Zoom,
}

/// Value-carrying update for a writable UVC control.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UvcControlUpdate {
    Brightness(i32),
    Contrast(i32),
    Saturation(i32),
    Sharpness(i32),
    WhiteBalanceAuto(bool),
    WhiteBalanceTemp(i32),
    AutoExposure(bool),
    ExposureTime(i32),
    PowerLineFreq(PowerLineFreq),
    Zoom(i32),
}

impl UvcControlUpdate {
    pub const fn key(&self) -> UvcControlKey {
        match self {
            Self::Brightness(_) => UvcControlKey::Brightness,
            Self::Contrast(_) => UvcControlKey::Contrast,
            Self::Saturation(_) => UvcControlKey::Saturation,
            Self::Sharpness(_) => UvcControlKey::Sharpness,
            Self::WhiteBalanceAuto(_) => UvcControlKey::WhiteBalanceAuto,
            Self::WhiteBalanceTemp(_) => UvcControlKey::WhiteBalanceTemp,
            Self::AutoExposure(_) => UvcControlKey::AutoExposure,
            Self::ExposureTime(_) => UvcControlKey::ExposureTime,
            Self::PowerLineFreq(_) => UvcControlKey::PowerLineFreq,
            Self::Zoom(_) => UvcControlKey::Zoom,
        }
    }
}

/// Picks the best UVC `CT_AE_MODE_CONTROL` byte when toggling Auto Exposure.
pub fn choose_ae_mode_byte(auto: bool, supported_mask: u8) -> u8 {
    if !auto {
        return AE_MODE_MANUAL;
    }
    if supported_mask & AE_MODE_APERTURE_PRIORITY != 0 {
        AE_MODE_APERTURE_PRIORITY
    } else if supported_mask & AE_MODE_AUTO != 0 {
        AE_MODE_AUTO
    } else if supported_mask & AE_MODE_SHUTTER_PRIORITY != 0 {
        AE_MODE_SHUTTER_PRIORITY
    } else {
        AE_MODE_APERTURE_PRIORITY
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_real_modern_webcam_vc_descriptors() {
        // Real VideoControl descriptors captured from Microsoft Modern Webcam (PID 0x0840).
        let mut desc = Vec::new();
        // Config descriptor stub (9B)
        desc.extend_from_slice(&[0x09, 0x02, 0x00, 0x00, 0x05, 0x01, 0x00, 0x80, 0xFA]);
        // Interface #0: VideoControl (class 14, sub 1)
        desc.extend_from_slice(&[0x09, 0x04, 0x00, 0x00, 0x01, 0x0E, 0x01, 0x00, 0x00]);
        // VC_HEADER
        desc.extend_from_slice(&[
            0x0D, 0x24, 0x01, 0x00, 0x01, 0x55, 0x00, 0x00, 0x00, 0xE0, 0x01, 0x01, 0x01,
        ]);
        // VC_INPUT_TERMINAL (ID=1, ITT_CAMERA=0x0201, bmControls=0x240A0A)
        desc.extend_from_slice(&[
            0x12, 0x24, 0x02, 0x01, 0x01, 0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x03, 0x0A, 0x0A, 0x24,
        ]);
        // VC_OUTPUT_TERMINAL
        desc.extend_from_slice(&[0x09, 0x24, 0x03, 0x02, 0x01, 0x01, 0x00, 0x04, 0x00]);
        // VC_SELECTOR_UNIT
        desc.extend_from_slice(&[0x07, 0x24, 0x04, 0x03, 0x01, 0x01, 0x00]);
        // VC_PROCESSING_UNIT (ID=4, bmControls=0x00145B)
        desc.extend_from_slice(&[
            0x0B, 0x24, 0x05, 0x04, 0x03, 0x00, 0x00, 0x02, 0x5B, 0x14, 0x00,
        ]);
        // VC_EXTENSION_UNIT (ID=5)
        desc.extend_from_slice(&[
            0x1B, 0x24, 0x06, 0x05, 0x29, 0xA7, 0x87, 0xC9, 0xD3, 0x59, 0x69, 0x45, 0x84, 0x67,
            0xFF, 0x08, 0x49, 0xFC, 0x19, 0xE8, 0x08, 0x01, 0x04, 0x02, 0xFF, 0x00, 0x00,
        ]);
        // Interface #1: VideoStreaming (must not overwrite VC)
        desc.extend_from_slice(&[0x09, 0x04, 0x01, 0x00, 0x00, 0x0E, 0x02, 0x00, 0x00]);

        let topo = parse_uvc_topology(&desc).unwrap();
        assert_eq!(topo.vc_interface, 0);
        assert_eq!(topo.camera_terminal_id, Some(1));
        assert_eq!(topo.camera_controls_mask, 0x0024_0A0A);
        assert_eq!(topo.processing_unit_id, Some(4));
        assert_eq!(topo.processing_controls_mask, 0x0000_145B);
        assert_eq!(topo.extension_unit_ids, vec![5]);
    }

    #[test]
    fn ae_mode_prefers_aperture_priority_when_full_auto_unsupported() {
        assert_eq!(choose_ae_mode_byte(true, 0x09), AE_MODE_APERTURE_PRIORITY);
        assert_eq!(choose_ae_mode_byte(false, 0x09), AE_MODE_MANUAL);
        assert_eq!(choose_ae_mode_byte(true, 0x03), AE_MODE_AUTO);
    }

    #[test]
    fn control_range_snaps_to_step() {
        let wb = ControlRange {
            min: 2800,
            max: 10000,
            step: 100,
            default: 4500,
        };
        assert_eq!(wb.clamp_snap(4540), 4500);
        assert_eq!(wb.clamp_snap(4560), 4600);
        assert_eq!(wb.clamp_snap(15000), 10000);
    }
}
